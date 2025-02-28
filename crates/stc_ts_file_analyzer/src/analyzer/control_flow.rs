use std::{
    borrow::{Borrow, Cow},
    collections::hash_map::Entry,
    hash::Hash,
    mem::{replace, take},
    ops::{AddAssign, BitOr, Not},
};

use fxhash::FxHashMap;
use rnode::{NodeId, VisitWith};
use stc_ts_ast_rnode::{
    RBinExpr, RBindingIdent, RCondExpr, RExpr, RIdent, RIfStmt, RObjectPatProp, RPat, RPatOrExpr, RStmt, RSwitchCase, RSwitchStmt,
};
use stc_ts_errors::{debug::dump_type_as_string, DebugExt, ErrorKind};
use stc_ts_type_ops::Fix;
use stc_ts_types::{name::Name, Array, ArrayMetadata, Id, Key, KeywordType, KeywordTypeMetadata, Union};
use stc_ts_utils::MapWithMut;
use stc_utils::{
    cache::Freeze,
    debug_ctx,
    ext::{SpanExt, TypeVecExt},
};
use swc_atoms::JsWord;
use swc_common::{Span, Spanned, SyntaxContext, TypeEq, DUMMY_SP};
use swc_ecma_ast::*;
use tracing::info;

use super::types::NormalizeTypeOpts;
use crate::{
    analyzer::{
        assign::AssignOpts,
        expr::{optional_chaining::is_obj_opt_chaining, AccessPropertyOpts, IdCtx, TypeOfMode},
        scope::{ScopeKind, VarInfo},
        util::ResultExt,
        Analyzer, Ctx,
    },
    ty::Type,
    type_facts::TypeFacts,
    util::EndsWithRet,
    validator,
    validator::ValidateWith,
    VResult,
};

/// Conditional facts
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct CondFacts {
    pub facts: FxHashMap<Name, TypeFacts>,
    pub vars: FxHashMap<Name, Type>,
    pub excludes: FxHashMap<Name, Vec<Type>>,
    pub types: FxHashMap<Id, Type>,
}

impl CondFacts {
    #[inline]
    pub(crate) fn assert_valid(&self) {
        if !cfg!(debug_assertions) {
            return;
        }

        for ty in self.vars.values() {
            ty.assert_valid();
            ty.assert_clone_cheap();
        }

        for types in self.excludes.values() {
            for ty in types {
                ty.assert_valid();
            }
        }

        for ty in self.types.values() {
            ty.assert_valid();
            ty.assert_clone_cheap();
        }
    }

    #[inline]
    pub(crate) fn assert_clone_cheap(&self) {
        if !cfg!(debug_assertions) {
            return;
        }

        for ty in self.vars.values() {
            if !ty.is_union_type() {
                debug_assert!(ty.is_clone_cheap(), "ty.is_clone_cheap() should be true:\n{:?}", &self.vars);
            }
        }

        for types in self.excludes.values() {
            for ty in types {
                debug_assert!(ty.is_clone_cheap());
            }
        }

        for ty in self.types.values() {
            debug_assert!(ty.is_clone_cheap());
        }
    }

    pub fn override_vars_using(&mut self, r: &mut Self) {
        for (k, ty) in r.vars.drain() {
            ty.assert_valid();
            ty.assert_clone_cheap();

            match self.vars.entry(k) {
                Entry::Occupied(mut e) => {
                    *e.get_mut() = ty;
                }
                Entry::Vacant(e) => {
                    e.insert(ty);
                }
            }
        }
    }

    pub fn take(&mut self) -> Self {
        Self {
            facts: take(&mut self.facts),
            vars: take(&mut self.vars),
            excludes: take(&mut self.excludes),
            types: take(&mut self.types),
        }
    }

    fn or<K, T>(mut map: FxHashMap<K, T>, map2: FxHashMap<K, T>) -> FxHashMap<K, T>
    where
        K: Eq + Hash,
        T: Merge,
    {
        for (k, v) in map2 {
            match map.entry(k) {
                Entry::Occupied(mut e) => {
                    e.get_mut().or(v);
                }
                Entry::Vacant(e) => {
                    e.insert(v);
                }
            }
        }

        map
    }
}

#[derive(Debug, Default, Clone)]
pub(super) struct Facts {
    pub true_facts: CondFacts,
    pub false_facts: CondFacts,
}

impl Facts {
    #[inline]
    pub(crate) fn assert_valid(&self) {
        if !cfg!(debug_assertions) {
            return;
        }

        self.true_facts.assert_valid();
        self.false_facts.assert_valid();
    }

    #[inline]
    pub(crate) fn assert_clone_cheap(&self) {
        if !cfg!(debug_assertions) {
            return;
        }

        self.true_facts.assert_clone_cheap();
        self.false_facts.assert_clone_cheap();
    }

    pub fn take(&mut self) -> Self {
        self.assert_valid();

        Self {
            true_facts: self.true_facts.take(),
            false_facts: self.false_facts.take(),
        }
    }
}

impl Not for Facts {
    type Output = Self;

    #[inline]
    fn not(self) -> Self {
        Facts {
            true_facts: self.false_facts,
            false_facts: self.true_facts,
        }
    }
}

impl AddAssign for Facts {
    fn add_assign(&mut self, rhs: Self) {
        self.true_facts += rhs.true_facts;
        self.false_facts += rhs.false_facts;
    }
}

impl AddAssign<Option<Self>> for Facts {
    fn add_assign(&mut self, rhs: Option<Self>) {
        if let Some(rhs) = rhs {
            *self += rhs;
        }
    }
}

impl BitOr for Facts {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Facts {
            true_facts: self.true_facts | rhs.true_facts,
            false_facts: self.false_facts | rhs.false_facts,
        }
    }
}

trait Merge {
    fn or(&mut self, other: Self);
}

impl<T> Merge for Box<T>
where
    T: Merge,
{
    fn or(&mut self, other: Self) {
        T::or(&mut **self, *other)
    }
}

impl<T> Merge for Vec<T> {
    fn or(&mut self, other: Self) {
        self.extend(other)
    }
}

impl Merge for TypeFacts {
    fn or(&mut self, other: Self) {
        *self |= other
    }
}

impl Merge for VarInfo {
    fn or(&mut self, other: Self) {
        self.copied |= other.copied;
        self.initialized |= other.initialized;
        Merge::or(&mut self.ty, other.ty);
    }
}

impl Merge for Type {
    fn or(&mut self, r: Self) {
        let l_span = self.span();

        let l = replace(self, Type::never(l_span, Default::default()));

        *self = Type::new_union(l_span, vec![l, r]);
    }
}

impl<T> Merge for Option<T>
where
    T: Merge,
{
    fn or(&mut self, other: Self) {
        match *self {
            Some(ref mut v) => {
                if let Some(other) = other {
                    v.or(other)
                }
            }
            _ => *self = other,
        }
    }
}

impl AddAssign for CondFacts {
    #[allow(clippy::suspicious_op_assign_impl)]
    fn add_assign(&mut self, rhs: Self) {
        self.assert_valid();
        rhs.assert_valid();

        for (k, v) in rhs.facts {
            *self.facts.entry(k.clone()).or_insert(TypeFacts::None) |= v;
        }

        self.types.extend(rhs.types);

        for (k, v) in rhs.vars {
            match self.vars.entry(k) {
                Entry::Occupied(mut e) => {
                    match e.get_mut().normalize_mut() {
                        Type::Union(u) => {
                            u.types.push(v);
                        }
                        prev => {
                            let prev = prev.take();
                            *e.get_mut() = Type::new_union(DUMMY_SP, vec![prev, v]).freezed();
                        }
                    };
                    e.get_mut().fix();
                    e.get_mut().make_clone_cheap();
                }
                Entry::Vacant(e) => {
                    e.insert(v);
                }
            }
        }

        for (k, v) in rhs.excludes {
            self.excludes.entry(k).or_default().extend(v);
        }
    }
}

impl AddAssign<Option<Self>> for CondFacts {
    fn add_assign(&mut self, rhs: Option<Self>) {
        self.assert_valid();

        if let Some(rhs) = rhs {
            *self += rhs;
        }
    }
}

impl BitOr for CondFacts {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        CondFacts {
            facts: CondFacts::or(self.facts, rhs.facts),
            vars: CondFacts::or(self.vars, rhs.vars),
            types: CondFacts::or(self.types, rhs.types),
            excludes: CondFacts::or(self.excludes, rhs.excludes),
        }
    }
}

#[validator]
impl Analyzer<'_, '_> {
    fn validate(&mut self, stmt: &RIfStmt) -> VResult<()> {
        let prev_facts = self.cur_facts.take();
        prev_facts.assert_clone_cheap();

        let facts_from_test: Facts = {
            let ctx = Ctx {
                in_cond: true,
                should_store_truthy_for_access: true,
                ..self.ctx
            };
            let facts = self
                .with_ctx(ctx)
                .with_child(ScopeKind::Flow, prev_facts.true_facts.clone(), |child: &mut Analyzer| {
                    let test = stmt.test.validate_with_default(child);
                    match test {
                        Ok(_) => {}
                        Err(err) => {
                            child.storage.report(err);
                        }
                    }

                    Ok(child.cur_facts.take())
                });

            facts.report(&mut self.storage).unwrap_or_default()
        };

        facts_from_test.assert_clone_cheap();

        let true_facts = facts_from_test.true_facts;
        let false_facts = facts_from_test.false_facts;

        let mut cons_ends_with_unreachable = false;

        let ends_with_ret = stmt.cons.ends_with_ret();

        self.cur_facts = prev_facts.clone();
        self.with_child(ScopeKind::Flow, true_facts, |child: &mut Analyzer| {
            stmt.cons.visit_with(child);

            cons_ends_with_unreachable = child.ctx.in_unreachable;

            Ok(())
        })
        .report(&mut self.storage);

        let mut alt_ends_with_unreachable = None;

        if let Some(alt) = &stmt.alt {
            self.cur_facts = prev_facts.clone();
            self.with_child(ScopeKind::Flow, false_facts.clone(), |child: &mut Analyzer| {
                alt.visit_with(child);

                alt_ends_with_unreachable = Some(child.ctx.in_unreachable);

                Ok(())
            })
            .report(&mut self.storage);
        }

        self.cur_facts = prev_facts;

        if ends_with_ret {
            self.cur_facts.true_facts += false_facts;
            return Ok(());
        }

        if cons_ends_with_unreachable {
            if let Some(true) = alt_ends_with_unreachable {
                self.ctx.in_unreachable = true;
            } else {
                self.cur_facts.true_facts += false_facts;
            }
        }

        Ok(())
    }
}

impl Analyzer<'_, '_> {
    /// This method may remove `SafeSubscriber` from `Subscriber` |
    /// `SafeSubscriber` or downgrade the type, like converting `Subscriber` |
    /// `SafeSubscriber` into `SafeSubscriber`. This behavior is controlled by
    /// the mark applied while handling type facts related to call.
    fn adjust_ternary_type(&mut self, span: Span, mut types: Vec<Type>) -> VResult<Vec<Type>> {
        types.iter_mut().for_each(|ty| {
            // Tuple -> Array
            if let Type::Tuple(tuple) = ty.normalize_mut() {
                let span = tuple.span;

                let mut elem_types: Vec<_> = tuple.elems.take().into_iter().map(|elem| *elem.ty).collect();
                elem_types.dedup_type();
                let elem_type = box Type::new_union(DUMMY_SP, elem_types);
                *ty = Type::Array(Array {
                    span,
                    elem_type,
                    metadata: ArrayMetadata {
                        common: tuple.metadata.common,
                        ..Default::default()
                    },
                })
                .freezed();
            }
        });

        let should_preserve = types
            .iter()
            .flat_map(|ty| ty.iter_union())
            .all(|ty| !ty.metadata().prevent_converting_to_children);

        if should_preserve {
            return self.remove_child_types(span, types);
        }

        self.downcast_types(span, types)
    }

    fn downcast_types(&mut self, span: Span, types: Vec<Type>) -> VResult<Vec<Type>> {
        fn need_work(ty: &Type) -> bool {
            !matches!(
                ty.normalize(),
                Type::Lit(..)
                    | Type::Keyword(KeywordType {
                        kind: TsKeywordTypeKind::TsNullKeyword,
                        ..
                    })
            )
        }

        let mut new = vec![];

        'outer: for (ai, ty) in types.iter().flat_map(|ty| ty.iter_union()).enumerate() {
            if need_work(ty) {
                for (bi, b) in types.iter().flat_map(|ty| ty.iter_union()).enumerate() {
                    if ai == bi || !need_work(b) {
                        continue;
                    }

                    // If type is same, we need to add it.
                    if b.type_eq(ty) {
                        break;
                    }

                    match self.extends(span, b, ty, Default::default()) {
                        Some(true) => {
                            // Remove ty.
                            continue 'outer;
                        }
                        res => {}
                    }
                }
            }

            new.push(ty.clone());
        }
        if new.is_empty() {
            // All types can be merged

            return Ok(types);
        }

        Ok(new)
    }

    /// Remove `SafeSubscriber` from `Subscriber` | `SafeSubscriber`.
    fn remove_child_types(&mut self, span: Span, types: Vec<Type>) -> VResult<Vec<Type>> {
        let mut new = vec![];

        'outer: for (ai, ty) in types.iter().flat_map(|ty| ty.iter_union()).enumerate() {
            for (bi, b) in types.iter().enumerate() {
                if ai == bi {
                    continue;
                }

                match self.extends(span, ty, b, Default::default()) {
                    Some(true) => {
                        // Remove ty.
                        continue 'outer;
                    }
                    res => {}
                }
            }

            new.push(ty.clone());
        }
        if new.is_empty() {
            // All types can be merged

            return Ok(types);
        }

        Ok(new)
    }

    /// Returns the type of discriminant.
    ///
    /// TODO(kdy1): Implement this.
    fn report_errors_for_incomparable_switch_cases(&mut self, s: &RSwitchStmt) -> VResult<Type> {
        let discriminant_ty = s.discriminant.validate_with_default(self)?;
        for case in &s.cases {
            if let Some(test) = &case.test {
                let case_ty = test.validate_with_default(self)?;
                // self.assign(&discriminant_ty, &case_ty, test.span())
                //     .context("tried to assign the discriminant of switch to
                // the test of a case")     .report(&mut
                // self.storage);
            }
        }

        Ok(discriminant_ty)
    }
}

#[validator]
impl Analyzer<'_, '_> {
    fn validate(&mut self, stmt: &RSwitchStmt) -> VResult<()> {
        self.record(stmt);

        let discriminant_ty = self.report_errors_for_incomparable_switch_cases(stmt).report(&mut self.storage);

        let mut false_facts = CondFacts::default();
        let mut base_true_facts = self.cur_facts.true_facts.take();
        // Declared at here as it's important to know if last one ends with return.
        let mut ends_with_ret = false;
        let len = stmt.cases.len();
        let stmt_span = stmt.span();

        let mut errored = false;
        // Check cases *in order*
        for (i, case) in stmt.cases.iter().enumerate() {
            if errored {
                break;
            }

            let span = case.test.as_ref().map(|v| v.span()).unwrap_or_else(|| stmt_span);

            let RSwitchCase { cons, .. } = case;
            let last = i == len - 1;

            ends_with_ret = cons.ends_with_ret();

            if let Some(ref test) = case.test {
                let binary_test_expr = RExpr::Bin(RBinExpr {
                    node_id: NodeId::invalid(),
                    op: op!("==="),
                    span,
                    left: stmt.discriminant.clone(),
                    right: test.clone(),
                });
                let ctx = Ctx {
                    in_cond: true,
                    in_switch_case_test: true,
                    should_store_truthy_for_access: true,
                    checking_switch_discriminant_as_bin: true,
                    ..self.ctx
                };
                let mut a = self.with_ctx(ctx);
                match binary_test_expr.validate_with_default(&mut *a) {
                    Ok(..) => {}
                    Err(err) => {
                        a.storage.report(err);
                        errored = true;
                        continue;
                    }
                }
            }

            let true_facts_created_by_case = self.cur_facts.true_facts.take();
            let false_facts_created_by_case = self.cur_facts.false_facts.take();

            let mut facts_for_body = base_true_facts.clone();
            facts_for_body += true_facts_created_by_case;

            self.with_child(ScopeKind::Flow, facts_for_body, |child| {
                cons.visit_with(child);
                Ok(())
            })?;

            if ends_with_ret || last {
                false_facts += false_facts_created_by_case.clone();
                base_true_facts += false_facts_created_by_case;
            }
        }

        if !errored {
            self.ctx.in_unreachable |= stmt
                .cases
                .iter()
                .all(|case| self.is_switch_case_body_unconditional_termination(&case.cons));
        }

        if ends_with_ret {
            self.cur_facts.true_facts += false_facts;
        }

        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct PatAssignOpts {
    pub assign: AssignOpts,
    pub ignore_lhs_errors: bool,
}

impl Analyzer<'_, '_> {
    /// Returns true if a body of switch always ends with `return`, `throw` or
    /// `continue`.
    ///
    /// TODO(kdy1): Support break with other label.
    fn is_switch_case_body_unconditional_termination<S>(&mut self, body: &[S]) -> bool
    where
        S: Borrow<RStmt>,
    {
        for stmt in body {
            match stmt.borrow() {
                RStmt::Return(..) | RStmt::Throw(..) | RStmt::Continue(..) => return true,
                RStmt::Break(..) => return false,

                RStmt::If(s) => match &s.alt {
                    Some(alt) => {
                        return self.is_switch_case_body_unconditional_termination(&[&*s.cons])
                            && self.is_switch_case_body_unconditional_termination(&[&**alt]);
                    }
                    None => return self.is_switch_case_body_unconditional_termination(&[&*s.cons]),
                },
                _ => {}
            }
        }

        false
    }

    pub(super) fn try_assign(&mut self, span: Span, op: AssignOp, lhs: &RPatOrExpr, ty: &Type) {
        ty.assert_valid();

        let res: VResult<()> = try {
            match *lhs {
                RPatOrExpr::Expr(ref expr) | RPatOrExpr::Pat(box RPat::Expr(ref expr)) => {
                    let lhs_ty = expr.validate_with_args(self, (TypeOfMode::LValue, None, None));
                    let mut lhs_ty = match lhs_ty {
                        Ok(v) => v,
                        _ => Type::any(lhs.span(), Default::default()),
                    };
                    lhs_ty.make_clone_cheap();

                    if op == op!("=") {
                        self.assign(span, &mut Default::default(), &lhs_ty, ty)?;
                    } else {
                        self.assign_with_op(span, op, &lhs_ty, ty)?;
                    }

                    if let RExpr::Ident(left) = &**expr {
                        if op == op!("??=") {
                            if let Ok(prev) = self.type_of_var(left, TypeOfMode::RValue, None) {
                                let new_actual_ty = self.apply_type_facts_to_type(TypeFacts::NEUndefinedOrNull, prev);

                                if let Some(var) = self.scope.vars.get_mut(&Id::from(left)) {
                                    var.actual_ty = Some(new_actual_ty);
                                }
                            }
                        }
                    }
                }

                RPatOrExpr::Pat(ref pat) => {
                    if op == op!("=") {
                        self.try_assign_pat_with_opts(
                            span,
                            pat,
                            ty,
                            PatAssignOpts {
                                ignore_lhs_errors: true,
                                ..Default::default()
                            },
                        )?;
                    } else {
                        // TODO
                        match &**pat {
                            RPat::Ident(left) => {
                                let lhs = self.type_of_var(&left.id, TypeOfMode::LValue, None);

                                if let Ok(lhs) = lhs {
                                    self.assign_with_op(span, op, &lhs, ty)?;
                                }
                            }
                            _ => Err(ErrorKind::InvalidOperatorForLhs { span, op })?,
                        }
                    }
                }
            }
        };

        match res {
            Ok(()) => {}
            Err(err) => self.storage.report(err),
        }
    }

    pub(super) fn try_assign_pat(&mut self, span: Span, lhs: &RPat, ty: &Type) -> VResult<()> {
        ty.assert_valid();

        self.try_assign_pat_with_opts(span, lhs, ty, Default::default())
    }

    fn try_assign_pat_with_opts(&mut self, span: Span, lhs: &RPat, ty: &Type, opts: PatAssignOpts) -> VResult<()> {
        ty.assert_valid();

        let span = span.with_ctxt(SyntaxContext::empty());

        let is_in_loop = self.scope.is_in_loop_body();
        let orig_ty = self
            .normalize(Some(ty.span().or_else(|| span)), Cow::Borrowed(ty), Default::default())
            .context("tried to normalize a type to assign it to a pattern")?
            .into_owned()
            .freezed();
        let _panic_ctx = debug_ctx!(format!("ty = {}", dump_type_as_string(&orig_ty)));

        let ty = orig_ty.normalize();

        ty.assert_valid();

        // Update variable's type
        match lhs {
            // We emitted some parsing errors.
            RPat::Invalid(..) => Ok(()),

            RPat::Assign(assign) => {
                // TODO(kdy1): Use type annotation?
                let res = assign
                    .right
                    .validate_with_default(self)
                    .context("tried to validate type of default expression in an assignment pattern");

                self.try_assign_pat_with_opts(span, &assign.left, ty, opts)
                    .report(&mut self.storage);

                res.and_then(|default_value_type| self.try_assign_pat_with_opts(span, &assign.left, &default_value_type, opts))
                    .report(&mut self.storage);

                Ok(())
            }

            RPat::Ident(i) => {
                // Verify using immutable references.
                if let Some(var_info) = self.scope.get_var(&i.id.clone().into()) {
                    if let Some(mut var_ty) = var_info.ty.clone() {
                        let _panic_ctx = debug_ctx!(format!("var_ty = {}", dump_type_as_string(ty)));

                        var_ty.make_clone_cheap();

                        self.assign_with_opts(
                            &mut Default::default(),
                            &var_ty,
                            ty,
                            AssignOpts {
                                span: i.id.span,
                                ..opts.assign
                            },
                        )?;
                    }
                }

                let mut actual_ty = None;
                if let Some(var_info) = self
                    .scope
                    .get_var(&i.id.clone().into())
                    .or_else(|| self.scope.search_parent(&i.id.clone().into()))
                {
                    if let Some(declared_ty) = &var_info.ty {
                        declared_ty.assert_valid();

                        if declared_ty.is_any()
                            || ty.is_kwd(TsKeywordTypeKind::TsNullKeyword)
                            || ty.is_kwd(TsKeywordTypeKind::TsUndefinedKeyword)
                        {
                            return Ok(());
                        }

                        let declared_ty = declared_ty.clone();

                        let ty = ty.clone();
                        let ty = self.apply_type_facts_to_type(TypeFacts::NEUndefined | TypeFacts::NENull, ty);

                        ty.assert_valid();

                        if ty.is_never() {
                            return Ok(());
                        }

                        let mut narrowed_ty = self.narrowed_type_of_assignment(span, declared_ty, &ty)?;
                        narrowed_ty.assert_valid();
                        narrowed_ty.make_clone_cheap();
                        actual_ty = Some(narrowed_ty);
                    }
                } else {
                    if !opts.ignore_lhs_errors {
                        self.storage.report(
                            ErrorKind::NoSuchVar {
                                span,
                                name: i.id.clone().into(),
                            }
                            .into(),
                        );
                    }
                    return Ok(());
                }

                if let Some(ty) = &actual_ty {
                    ty.assert_valid();
                    ty.assert_clone_cheap();
                }

                // Update actual types.
                if let Some(var_info) = self.scope.get_var_mut(&i.id.clone().into()) {
                    var_info.is_actual_type_modified_in_loop |= is_in_loop;
                    let mut new_ty = actual_ty.unwrap_or_else(|| ty.clone());
                    new_ty.assert_valid();
                    new_ty.make_clone_cheap();
                    var_info.actual_ty = Some(new_ty);
                    return Ok(());
                }

                let var_info = if let Some(var_info) = self.scope.search_parent(&i.id.clone().into()) {
                    let actual_ty = actual_ty.unwrap_or_else(|| orig_ty.clone());
                    actual_ty.assert_valid();
                    actual_ty.assert_clone_cheap();

                    VarInfo {
                        actual_ty: Some(actual_ty),
                        copied: true,
                        ..var_info.clone()
                    }
                } else {
                    if let Some(types) = self.find_type(&i.id.clone().into())? {
                        for ty in types {
                            if let Type::Module(..) = ty.normalize() {
                                return Err(ErrorKind::NotVariable {
                                    span: i.id.span,
                                    left: lhs.span(),
                                    ty: Some(box ty.normalize().clone()),
                                }
                                .into());
                            }
                        }
                    }

                    return if self.ctx.allow_ref_declaring && self.scope.declaring.contains(&i.id.clone().into()) {
                        Ok(())
                    } else {
                        // undefined symbol
                        Err(ErrorKind::UndefinedSymbol {
                            sym: i.id.clone().into(),
                            span: i.id.span,
                        }
                        .into())
                    };
                };

                // Variable is defined on parent scope.
                //
                // We copy varinfo with enhanced type.
                self.scope.insert_var(i.id.clone().into(), var_info);

                Ok(())
            }

            RPat::Array(ref arr) => {
                let ty = self
                    .get_iterator(span, Cow::Borrowed(ty), Default::default())
                    .context("tried to convert a type to an iterator to assign with an array pattern")
                    .report(&mut self.storage)
                    .unwrap_or_else(|| Cow::Owned(Type::any(span, Default::default())));
                //
                for (i, elem) in arr.elems.iter().enumerate() {
                    if let Some(elem) = elem {
                        if let RPat::Rest(elem) = elem {
                            // Rest element is special.
                            let type_for_rest_arg = self
                                .get_rest_elements(None, ty, i)
                                .context("tried to get lefting elements of an iterator to assign using a rest pattern")?;

                            self.try_assign_pat_with_opts(
                                span,
                                &elem.arg,
                                &type_for_rest_arg,
                                PatAssignOpts {
                                    assign: AssignOpts {
                                        allow_iterable_on_rhs: true,
                                        ..opts.assign
                                    },
                                    ..opts
                                },
                            )
                            .context("tried to assign lefting elements to the arugment of a rest pattern")?;
                            break;
                        }

                        let elem_ty = self
                            .get_element_from_iterator(span, Cow::Borrowed(&ty), i)
                            .context("tried to get an element of type to assign with an array pattern")
                            .report(&mut self.storage);
                        if let Some(elem_ty) = elem_ty {
                            self.try_assign_pat_with_opts(span, elem, &elem_ty, opts)
                                .context("tried to assign an element of an array pattern")
                                .report(&mut self.storage);
                        }
                    }
                }
                Ok(())
            }

            RPat::Object(ref obj) => {
                //
                for prop in obj.props.iter() {
                    match prop {
                        RObjectPatProp::KeyValue(kv) => {
                            let key = kv.key.validate_with(self)?;
                            let prop_ty = self
                                .access_property(
                                    span,
                                    ty,
                                    &key,
                                    TypeOfMode::RValue,
                                    IdCtx::Var,
                                    AccessPropertyOpts {
                                        disallow_indexing_array_with_string: true,

                                        ..Default::default()
                                    },
                                )
                                .unwrap_or_else(|_| Type::any(span, Default::default()));

                            self.try_assign_pat_with_opts(span, &kv.value, &prop_ty, opts)
                                .report(&mut self.storage);
                        }
                        RObjectPatProp::Assign(a) => {
                            let key = Key::Normal {
                                span: a.key.span,
                                sym: a.key.sym.clone(),
                            };

                            let prop_ty = self
                                .access_property(
                                    span,
                                    ty,
                                    &key,
                                    TypeOfMode::RValue,
                                    IdCtx::Var,
                                    AccessPropertyOpts {
                                        disallow_indexing_array_with_string: true,
                                        ..Default::default()
                                    },
                                )
                                .unwrap_or_else(|_| Type::any(span, Default::default()))
                                .freezed();

                            self.try_assign_pat_with_opts(
                                span,
                                &RPat::Ident(RBindingIdent {
                                    node_id: NodeId::invalid(),
                                    id: a.key.clone(),
                                    type_ann: None,
                                }),
                                &prop_ty,
                                opts,
                            )
                            .report(&mut self.storage);
                        }
                        RObjectPatProp::Rest(r) => {
                            if r.type_ann.is_none() {
                                if let Some(m) = &mut self.mutations {
                                    m.for_pats.entry(r.node_id).or_default().ty = Some(Type::any(span, Default::default()));
                                }
                            }

                            match &*r.arg {
                                RPat::Ident(_) => {}

                                RPat::Array(_) => {
                                    self.storage.report(ErrorKind::NotArrayType { span: r.arg.span() }.into());
                                    self.storage
                                        .report(ErrorKind::BindingPatNotAllowedInRestPatArg { span: r.arg.span() }.into());
                                }

                                RPat::Object(_) => {
                                    self.storage
                                        .report(ErrorKind::BindingPatNotAllowedInRestPatArg { span: r.arg.span() }.into());
                                }

                                RPat::Expr(expr) => {
                                    // { ...obj?.a["b"] }
                                    if is_obj_opt_chaining(expr) {
                                        return Err(ErrorKind::InvalidRestPatternInOptionalChain { span: r.span }.into());
                                    }

                                    self.storage
                                        .report(ErrorKind::BindingPatNotAllowedInRestPatArg { span: r.arg.span() }.into());
                                }

                                RPat::Invalid(_) => {
                                    // self.storage.report(Error::BindingPatNotAllowedInRestPatArg { span:
                                    // r.arg.span() });
                                    self.storage
                                        .report(ErrorKind::RestArgMustBeVarOrMemberAccess { span: r.arg.span() }.into());
                                }

                                _ => {}
                            }
                            // TODO
                            // self.try_assign_pat_with_opts(span, lhs,
                            // &prop_ty).report(&mut self.storage);
                        }
                    }
                }

                Ok(())
            }

            RPat::Rest(rest) => {
                // TODO(kdy1): Check if this is correct. (in object rest context)
                let ty = Type::Array(Array {
                    span,
                    elem_type: box ty.clone(),
                    metadata: Default::default(),
                });
                self.try_assign_pat_with_opts(span, &rest.arg, &ty, opts)
            }

            RPat::Expr(lhs) => {
                if let RExpr::Lit(..) = &**lhs {
                    self.storage.report(ErrorKind::InvalidLhsOfAssign { span: lhs.span() }.into());
                    return Ok(());
                }
                let lhs_ty = lhs
                    .validate_with_args(self, (TypeOfMode::LValue, None, None))
                    .context("tried to validate type of the expression in lhs of assignment")
                    .report(&mut self.storage);

                if let Some(lhs_ty) = &lhs_ty {
                    self.assign_with_opts(&mut Default::default(), lhs_ty, ty, AssignOpts { span, ..opts.assign })?;
                }
                Ok(())
            }
        }
    }

    /// While this type fact is in scope, the var named `sym` will be treated as
    /// `ty`.
    pub(super) fn add_type_fact(&mut self, sym: &Id, ty: Type, exclude: Type) {
        info!("add_type_fact({}); ty = {:?}", sym, ty);

        ty.assert_clone_cheap();
        exclude.assert_clone_cheap();

        self.cur_facts.insert_var(sym, ty, exclude, false);
    }

    pub(super) fn add_deep_type_fact(&mut self, span: Span, name: Name, ty: Type, is_for_true: bool) {
        debug_assert!(!self.is_builtin);

        ty.assert_valid();
        ty.assert_clone_cheap();

        if let Some((name, mut ty)) = self
            .determine_type_fact_by_field_fact(span, &name, &ty)
            .report(&mut self.storage)
            .flatten()
        {
            ty.make_clone_cheap();
            ty.assert_valid();

            if is_for_true {
                self.cur_facts.true_facts.vars.insert(name, ty);
            } else {
                self.cur_facts.false_facts.vars.insert(name, ty);
            }
            return;
        }

        if is_for_true {
            self.cur_facts.true_facts.vars.insert(name, ty);
        } else {
            self.cur_facts.false_facts.vars.insert(name, ty);
        }
    }

    /// If `type_facts` is [None], this method calculates type facts created by
    /// `'foo' in obj`.
    ///
    /// Otherwise, this method calculates type facts created by `if (a.foo) ;`.
    /// In this case, this method tests if `type_facts` matches the type of
    /// property and returns `never` if it does not.
    pub(super) fn narrow_types_with_property(
        &mut self,
        span: Span,
        src: &Type,
        property: &JsWord,
        type_facts: Option<TypeFacts>,
    ) -> VResult<Type> {
        src.assert_valid();

        let src = self.normalize(
            Some(span),
            Cow::Borrowed(src),
            NormalizeTypeOpts {
                preserve_union: true,
                preserve_global_this: true,
                ..Default::default()
            },
        )?;

        if let Type::Union(ty) = src.normalize() {
            let mut new_types = vec![];
            for ty in &ty.types {
                let ty = self.narrow_types_with_property(span, ty, property, type_facts)?;
                new_types.push(ty);
            }
            new_types.retain(|ty| !ty.is_never());
            new_types.dedup_type();

            if new_types.len() == 1 {
                return Ok(new_types.into_iter().next().unwrap());
            }

            return Ok(Type::Union(Union {
                span: ty.span(),
                types: new_types,
                metadata: ty.metadata,
            }));
        }

        let prop_res = self
            .access_property(
                src.span().or_else(|| span),
                &src,
                &Key::Normal {
                    span: DUMMY_SP,
                    sym: property.clone(),
                },
                TypeOfMode::RValue,
                IdCtx::Var,
                AccessPropertyOpts {
                    disallow_creating_indexed_type_from_ty_els: true,
                    ..Default::default()
                },
            )
            .map(Freeze::freezed);

        match prop_res {
            Ok(mut prop_ty) => {
                // Check if property matches the type fact.
                if let Some(type_facts) = type_facts {
                    let orig = prop_ty.clone();
                    prop_ty = self.apply_type_facts_to_type(type_facts, prop_ty);

                    // TODO(kdy1): See if which one is correct.
                    //
                    // if !orig.normalize().type_eq(prop_ty.normalize()) {
                    //     return Ok(Type::never(src.span()));
                    // }

                    if prop_ty.is_never() {
                        return Ok(Type::never(
                            src.span(),
                            KeywordTypeMetadata {
                                common: src.metadata(),
                                ..Default::default()
                            },
                        ));
                    }
                }
            }
            Err(err) => match *err {
                ErrorKind::NoSuchProperty { .. } | ErrorKind::NoSuchPropertyInClass { .. } => {
                    return Ok(Type::never(
                        src.span(),
                        KeywordTypeMetadata {
                            common: src.metadata(),
                            ..Default::default()
                        },
                    ))
                }
                _ => {}
            },
        }

        Ok(src.into_owned())
    }

    fn determine_type_fact_by_field_fact(&mut self, span: Span, name: &Name, ty: &Type) -> VResult<Option<(Name, Type)>> {
        ty.assert_valid();

        if name.len() == 1 {
            return Ok(None);
        }

        let ids = name.as_ids();
        let mut id: RIdent = ids[0].clone().into();
        id.span.lo = span.lo;
        id.span.hi = span.hi;

        let obj = self.type_of_var(&id, TypeOfMode::RValue, None)?;
        let obj = self.normalize(
            Some(span),
            Cow::Owned(obj),
            NormalizeTypeOpts {
                preserve_global_this: true,
                preserve_union: true,
                ..Default::default()
            },
        )?;

        if let Type::Union(u) = obj.normalize() {
            if ids.len() == 2 {
                let mut new_obj_types = vec![];

                for obj in &u.types {
                    if let Ok(prop_ty) = self.access_property(
                        obj.span(),
                        obj,
                        &Key::Normal {
                            span: ty.span(),
                            sym: ids[1].sym().clone(),
                        },
                        TypeOfMode::RValue,
                        IdCtx::Var,
                        Default::default(),
                    ) {
                        if ty.type_eq(&prop_ty) {
                            new_obj_types.push(obj.clone());
                        }
                    }
                }

                if new_obj_types.is_empty() {
                    return Ok(None);
                }
                let mut ty = Type::union(new_obj_types);
                ty.fix();

                return Ok(Some((Name::from(ids[0].clone()), ty)));
            }
        }

        Ok(None)
    }
}

#[validator]
impl Analyzer<'_, '_> {
    fn validate(&mut self, e: &RCondExpr, mode: TypeOfMode, type_ann: Option<&Type>) -> VResult<Type> {
        self.record(e);

        let RCondExpr {
            span,
            ref test,
            ref alt,
            ref cons,
            ..
        } = *e;

        self.validate_with(|a| {
            let ctx = Ctx {
                in_cond: true,
                should_store_truthy_for_access: true,
                ..a.ctx
            };
            test.validate_with_default(&mut *a.with_ctx(ctx))?;

            Ok(())
        });

        let true_facts = self.cur_facts.true_facts.take();
        let false_facts = self.cur_facts.false_facts.take();
        let mut cons = self.with_child(ScopeKind::Flow, true_facts, |child: &mut Analyzer| {
            let ty = cons.validate_with_args(child, (mode, None, type_ann)).report(&mut child.storage);

            Ok(ty.unwrap_or_else(|| Type::any(cons.span(), Default::default())))
        })?;
        cons.make_clone_cheap();
        let mut alt = self.with_child(ScopeKind::Flow, false_facts, |child: &mut Analyzer| {
            let ty = alt.validate_with_args(child, (mode, None, type_ann)).report(&mut child.storage);

            Ok(ty.unwrap_or_else(|| Type::any(alt.span(), Default::default())))
        })?;
        alt.make_clone_cheap();

        if cons.type_eq(&alt) {
            return Ok(cons);
        }

        let new_types = if type_ann.is_none() {
            self.adjust_ternary_type(span, vec![cons, alt])?
        } else {
            vec![cons, alt]
        };
        let mut ty = Type::union(new_types).fixed();
        ty.reposition(span);
        ty.assert_valid();
        Ok(ty)
    }
}

impl Facts {
    fn insert_var<N: Into<Name>>(&mut self, name: N, ty: Type, exclude: Type, negate: bool) {
        ty.assert_valid();
        ty.assert_clone_cheap();

        let name = name.into();

        if negate {
            self.false_facts.vars.insert(name.clone(), ty);
            self.true_facts.excludes.entry(name).or_default().push(exclude);
        } else {
            self.true_facts.vars.insert(name.clone(), ty);
            self.false_facts.excludes.entry(name).or_default().push(exclude);
        }
    }
}
