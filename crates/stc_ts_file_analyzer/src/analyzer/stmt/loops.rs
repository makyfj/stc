use std::borrow::Cow;

use rnode::VisitWith;
use stc_ts_ast_rnode::{
    RDoWhileStmt, RExpr, RForInStmt, RForOfStmt, RIdent, RPat, RStmt, RTsEntityName, RVarDecl, RVarDeclOrPat, RWhileStmt,
};
use stc_ts_errors::{DebugExt, ErrorKind};
use stc_ts_file_analyzer_macros::extra_validator;
use stc_ts_types::{Id, KeywordType, KeywordTypeMetadata, Operator, Ref, RefMetadata, TypeParamInstantiation};
use stc_ts_utils::{find_ids_in_pat, PatExt};
use stc_utils::cache::Freeze;
use swc_common::{Span, Spanned, DUMMY_SP};
use swc_ecma_ast::{EsVersion, TsKeywordTypeKind, TsTypeOperatorOp, VarDeclKind};

use crate::{
    analyzer::{control_flow::CondFacts, types::NormalizeTypeOpts, util::ResultExt, Analyzer, Ctx, ScopeKind},
    ty::Type,
    util::is_str_or_union,
    validator,
    validator::ValidateWith,
    VResult,
};

#[derive(Clone, Copy)]
enum ForHeadKind {
    In,
    Of { is_awaited: bool },
}

impl Analyzer<'_, '_> {
    /// We evaluate loop bodies multiple time.
    /// But actually we don't report errors
    ///
    /// If type does not change due to a loop, we evaluate
    fn validate_loop_body_with_scope(&mut self, test: Option<&RExpr>, body: &RStmt) -> VResult<()> {
        let mut orig_facts = self.cur_facts.take();

        let mut prev_facts = orig_facts.true_facts.take();
        let prev_false_facts = orig_facts.false_facts.take();
        let mut facts_of_prev_body_eval = CondFacts::default();
        let mut last = false;
        let mut orig_vars = Some(self.scope.vars.clone());

        loop {
            let mut facts_from_body: CondFacts = self.with_child_with_hook(
                ScopeKind::LoopBody { last },
                prev_facts.clone(),
                |child: &mut Analyzer| {
                    child.ctx.ignore_errors |= !last;

                    {
                        let ctx = Ctx {
                            in_cond: true,
                            ..child.ctx
                        };
                        test.visit_with(&mut *child.with_ctx(ctx));
                    }

                    body.visit_with(child);

                    Ok(child.cur_facts.true_facts.take())
                },
                |analyzer: &mut Analyzer| {
                    if last {
                        analyzer.scope.vars = orig_vars.take().unwrap();
                    }
                },
            )?;

            facts_from_body.excludes.clear();

            if last {
                prev_facts += facts_from_body;
                break;
            }

            if facts_of_prev_body_eval == facts_from_body {
                last = true;
            } else {
                facts_of_prev_body_eval = facts_from_body.clone();
            }

            // We copy `actual` types and type facts from the child scope.

            prev_facts.override_vars_using(&mut facts_from_body);

            prev_facts += facts_from_body;
        }

        self.cur_facts.true_facts += prev_facts;
        self.cur_facts.false_facts += prev_false_facts;

        Ok(())
    }

    #[extra_validator]
    fn validate_lhs_of_for_loop(&mut self, e: &RVarDeclOrPat, elem_ty: &Type, kind: ForHeadKind) {
        let span = e.span();

        match self.validate_lhs_of_for_in_of_loop(e, kind) {
            Ok(()) => {}
            Err(err) => {
                self.storage.report(err);
            }
        }

        match *e {
            RVarDeclOrPat::VarDecl(ref v) => {
                // It is a parsing error if there are multiple variable declarators.
                // So we only handle the case where there's only one variable declarator.
                if v.decls.len() == 1 {
                    if let Some(m) = &mut self.mutations {
                        // We use node id of a variable declarator with `for_pats`.
                        let node_id = v.decls.first().unwrap().node_id;
                        m.for_pats.entry(node_id).or_default().ty.get_or_insert_with(|| elem_ty.clone());
                    }
                    // Add types.
                }

                // Store variables
                v.visit_with(self);
            }
            RVarDeclOrPat::Pat(ref pat) => {
                self.try_assign_pat(span, pat, elem_ty)
                    .context("tried to assign to the pattern of a for-of/for-in loop")
                    .convert_err(|err| {
                        if let ForHeadKind::In = kind {
                            if err.is_assign_failure() {
                                return ErrorKind::WrongTypeForLhsOfForInLoop { span: err.span() };
                            }
                        }

                        err
                    })
                    .report(&mut self.storage);
            }
        }
    }

    fn validate_lhs_of_for_in_of_loop(&mut self, e: &RVarDeclOrPat, kind: ForHeadKind) -> VResult<()> {
        match e {
            RVarDeclOrPat::VarDecl(v) => {
                if !v.decls.is_empty() {
                    self.validate_lhs_of_for_in_of_loop_pat(&v.decls[0].name, kind)
                } else {
                    Ok(())
                }
            }
            RVarDeclOrPat::Pat(p) => self.validate_lhs_of_for_in_of_loop_pat(p, kind),
        }
    }

    fn validate_lhs_of_for_in_of_loop_pat(&mut self, p: &RPat, kind: ForHeadKind) -> VResult<()> {
        match p {
            RPat::Object(..) | RPat::Array(..) => match kind {
                ForHeadKind::In => Err(ErrorKind::DestructuringBindingNotAllowedInLhsOfForIn { span: p.span() }.into()),
                ForHeadKind::Of { .. } => Ok(()),
            },
            RPat::Expr(e) => self.validate_lhs_of_for_in_of_loop_expr(e, kind),
            _ => Ok(()),
        }
    }

    fn validate_lhs_of_for_in_of_loop_expr(&mut self, e: &RExpr, kind: ForHeadKind) -> VResult<()> {
        // for (obj?.a["b"] in obj) {}
        use crate::analyzer::expr::optional_chaining::is_obj_opt_chaining;
        if is_obj_opt_chaining(e) {
            return match kind {
                ForHeadKind::In => Err(ErrorKind::InvalidRestPatternInForIn { span: e.span() }.into()),
                ForHeadKind::Of { .. } => Err(ErrorKind::InvalidRestPatternInForOf { span: e.span() }.into()),
            };
        }

        match e {
            RExpr::Ident(..) | RExpr::This(..) | RExpr::Member(..) | RExpr::Paren(..) => Ok(()),
            // We use different error code for this.
            RExpr::Assign(..) => Ok(()),
            _ => match kind {
                ForHeadKind::In => Err(ErrorKind::InvalidExprOfLhsOfForIn { span: e.span() }.into()),
                ForHeadKind::Of { .. } => Err(ErrorKind::InvalidExprOfLhsOfForOf { span: e.span() }.into()),
            },
        }
    }

    fn get_element_type_of_for_in(&mut self, rhs: &Type) -> VResult<Type> {
        let rhs = self
            .normalize(
                None,
                Cow::Borrowed(rhs),
                NormalizeTypeOpts {
                    preserve_mapped: true,
                    ..Default::default()
                },
            )
            .context("tried to normalize a type to handle a for-in loop")?;
        let rhs = rhs.normalize();

        if rhs.is_kwd(TsKeywordTypeKind::TsObjectKeyword) || rhs.is_array() || rhs.is_tuple() {
            return Ok(Type::Keyword(KeywordType {
                span: rhs.span(),
                kind: TsKeywordTypeKind::TsStringKeyword,
                metadata: KeywordTypeMetadata {
                    common: rhs.metadata(),
                    ..Default::default()
                },
            }));
        }

        if let Type::Mapped(m) = rhs.normalize() {
            // { [P in keyof K]: T[P]; }
            // =>
            // Extract<keyof K, string>
            if let Some(
                contraint @ Type::Operator(Operator {
                    op: TsTypeOperatorOp::KeyOf,
                    ..
                }),
            ) = m.type_param.constraint.as_deref().map(|ty| ty.normalize())
            {
                // Extract<keyof T
                return Ok(Type::Ref(Ref {
                    span: m.span,
                    type_name: RTsEntityName::Ident(RIdent::new("Extract".into(), DUMMY_SP)),
                    type_args: Some(box TypeParamInstantiation {
                        span: DUMMY_SP,
                        params: vec![
                            contraint.clone(),
                            Type::Keyword(KeywordType {
                                span: rhs.span(),
                                kind: TsKeywordTypeKind::TsStringKeyword,
                                metadata: KeywordTypeMetadata {
                                    common: rhs.metadata(),
                                    ..Default::default()
                                },
                            }),
                        ],
                    }),
                    metadata: RefMetadata {
                        common: m.metadata.common,
                        ..Default::default()
                    },
                }));
            }

            // { [P in K]: T[P]; }
            if let Some(..) = m.type_param.constraint.as_deref() {
                return Ok(Type::Param(m.type_param.clone()));
            }
        }

        let s = Type::Keyword(KeywordType {
            span: rhs.span(),
            kind: TsKeywordTypeKind::TsStringKeyword,
            metadata: KeywordTypeMetadata {
                common: rhs.metadata(),
                ..Default::default()
            },
        });
        let n = Type::Keyword(KeywordType {
            span: rhs.span(),
            kind: TsKeywordTypeKind::TsNumberKeyword,
            metadata: KeywordTypeMetadata {
                common: rhs.metadata(),
                ..Default::default()
            },
        });
        Ok(Type::union(vec![s, n]))
    }

    #[extra_validator]
    fn check_for_of_in_loop(&mut self, span: Span, left: &RVarDeclOrPat, rhs: &RExpr, kind: ForHeadKind, body: &RStmt) {
        self.with_child(ScopeKind::Flow, Default::default(), |child: &mut Analyzer| -> VResult<()> {
            // Error should not be `no such var` if it's used in rhs.
            let created_vars: Vec<Id> = match left {
                RVarDeclOrPat::VarDecl(v) => find_ids_in_pat(&v.decls),
                RVarDeclOrPat::Pat(_) => {
                    vec![]
                }
            };
            debug_assert_eq!(child.scope.declaring, Vec::<Id>::new());
            child.scope.declaring.extend(created_vars);

            child.ctx.allow_ref_declaring = matches!(
                left,
                RVarDeclOrPat::VarDecl(box RVarDecl {
                    kind: VarDeclKind::Var,
                    ..
                })
            );

            // Type annotation on lhs of for in/of loops is invalid.
            if let RVarDeclOrPat::VarDecl(box RVarDecl { decls, .. }) = left {
                if !decls.is_empty() {
                    if decls[0].name.get_ty().is_some() {
                        match kind {
                            ForHeadKind::In => {
                                child
                                    .storage
                                    .report(ErrorKind::TypeAnnOnLhsOfForInLoops { span: decls[0].span }.into());
                            }
                            ForHeadKind::Of { .. } => {
                                child
                                    .storage
                                    .report(ErrorKind::TypeAnnOnLhsOfForOfLoops { span: decls[0].span }.into());
                            }
                        }
                    }
                }
            }

            let rhs_ctx = Ctx {
                cannot_be_tuple: true,
                // use_undefined_for_empty_tuple: true,
                ..child.ctx
            };

            let rty = rhs
                .validate_with_default(&mut *child.with_ctx(rhs_ctx))
                .context("tried to validate rhs of a for in/of loop");
            let rty = rty
                .report(&mut child.storage)
                .unwrap_or_else(|| Type::any(span, Default::default()));

            if let ForHeadKind::Of { is_awaited: false } = kind {
                if child.env.target() < EsVersion::Es5 {
                    if rty
                        .iter_union()
                        .flat_map(|ty| ty.iter_union())
                        .flat_map(|ty| ty.iter_union())
                        .any(is_str_or_union)
                    {
                        child.storage.report(ErrorKind::ForOfStringUsedInEs3 { span }.into())
                    }
                }
            }

            let mut elem_ty = match kind {
                ForHeadKind::Of { is_awaited: false } => child
                    .get_iterator_element_type(rhs.span(), Cow::Owned(rty), false, Default::default())
                    .convert_err(|err| match err {
                        ErrorKind::NotArrayType { span } if matches!(rhs, RExpr::Lit(..)) => ErrorKind::NotArrayTypeNorStringType { span },
                        _ => err,
                    })
                    .context("tried to get the element type of an iterator to calculate type for a for-of loop")
                    .report(&mut child.storage)
                    .unwrap_or_else(|| Cow::Owned(Type::any(span, Default::default()))),

                ForHeadKind::Of { is_awaited: true } => child
                    .get_async_iterator_element_type(rhs.span(), Cow::Owned(rty))
                    .context("tried to get element type of an async iteratror")
                    .report(&mut child.storage)
                    .unwrap_or_else(|| Cow::Owned(Type::any(span, Default::default()))),

                ForHeadKind::In => Cow::Owned(
                    child
                        .get_element_type_of_for_in(&rty)
                        .context("tried to calculate the element type for a for-in loop")
                        .report(&mut child.storage)
                        .unwrap_or_else(|| Type::any(span, Default::default())),
                ),
            };
            elem_ty.make_clone_cheap();

            child.scope.declaring.clear();

            child.validate_lhs_of_for_loop(left, &elem_ty, kind);

            child.validate_loop_body_with_scope(None, body).report(&mut child.storage);

            Ok(())
        })?;
    }
}

#[validator]
impl Analyzer<'_, '_> {
    fn validate(&mut self, s: &RForInStmt) {
        self.check_for_of_in_loop(s.span, &s.left, &s.right, ForHeadKind::In, &s.body);

        Ok(())
    }
}

#[validator]
impl Analyzer<'_, '_> {
    fn validate(&mut self, s: &RForOfStmt) {
        self.check_for_of_in_loop(
            s.span,
            &s.left,
            &s.right,
            ForHeadKind::Of {
                is_awaited: s.await_token.is_some(),
            },
            &s.body,
        );

        Ok(())
    }
}

#[validator]
impl Analyzer<'_, '_> {
    fn validate(&mut self, node: &RWhileStmt) {
        self.validate_loop_body_with_scope(Some(&node.test), &node.body)
            .report(&mut self.storage);

        Ok(())
    }
}

#[validator]
impl Analyzer<'_, '_> {
    fn validate(&mut self, node: &RDoWhileStmt) {
        node.body.visit_with(self);

        self.validate_loop_body_with_scope(Some(&node.test), &node.body)
            .report(&mut self.storage);

        Ok(())
    }
}
