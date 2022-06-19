use std::fmt;

use diagnostic::{DiagnosticMessage, Label, Note, Span, Urls};
use value::Value;

use crate::{
    expression::{self, Expr, Resolved},
    parser::{ast, Node},
    state::{ExternalEnv, LocalEnv},
    value::VrlValueArithmetic,
    BatchContext, Context, Expression, TypeDef,
};

#[derive(Clone, PartialEq)]
pub struct Op {
    pub(crate) lhs: Box<Expr>,
    pub(crate) rhs: Box<Expr>,
    pub(crate) opcode: ast::Opcode,
}

impl Op {
    pub fn new(
        lhs: Node<Expr>,
        opcode: Node<ast::Opcode>,
        rhs: Node<Expr>,
        state: (&LocalEnv, &ExternalEnv),
    ) -> Result<Self, Error> {
        use ast::Opcode::{Eq, Ge, Gt, Le, Lt, Ne};

        let (op_span, opcode) = opcode.take();

        let (lhs_span, lhs) = lhs.take();
        let lhs_type_def = lhs.type_def(state);

        let (rhs_span, rhs) = rhs.take();

        if matches!(opcode, Eq | Ne | Lt | Le | Gt | Ge) {
            if let Expr::Op(op) = &lhs {
                if matches!(op.opcode, Eq | Ne | Lt | Le | Gt | Ge) {
                    return Err(Error::ChainedComparison { span: op_span });
                }
            }
        }

        if let ast::Opcode::Err = opcode {
            if lhs_type_def.is_infallible() {
                return Err(Error::UnnecessaryCoalesce {
                    lhs_span,
                    rhs_span,
                    op_span,
                });
            }
        }

        if let ast::Opcode::Merge = opcode {
            if !(lhs.type_def(state).is_object() && rhs.type_def(state).is_object()) {
                return Err(Error::MergeNonObjects {
                    lhs_span: if lhs.type_def(state).is_object() {
                        None
                    } else {
                        Some(lhs_span)
                    },
                    rhs_span: if rhs.type_def(state).is_object() {
                        None
                    } else {
                        Some(rhs_span)
                    },
                });
            }
        }

        Ok(Op {
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
            opcode,
        })
    }
}

impl Expression for Op {
    fn resolve(&self, ctx: &mut Context) -> Resolved {
        use ast::Opcode::{Add, And, Div, Eq, Err, Ge, Gt, Le, Lt, Merge, Mul, Ne, Or, Rem, Sub};
        use value::Value::{Boolean, Null};

        match self.opcode {
            Err => return self.lhs.resolve(ctx).or_else(|_| self.rhs.resolve(ctx)),
            Or => {
                return self
                    .lhs
                    .resolve(ctx)?
                    .try_or(|| self.rhs.resolve(ctx))
                    .map_err(Into::into);
            }
            And => {
                return match self.lhs.resolve(ctx)? {
                    Null | Boolean(false) => Ok(false.into()),
                    v => v.try_and(self.rhs.resolve(ctx)?).map_err(Into::into),
                };
            }
            _ => (),
        }

        let lhs = self.lhs.resolve(ctx)?;
        let rhs = self.rhs.resolve(ctx)?;

        match self.opcode {
            Mul => lhs.try_mul(rhs),
            Div => lhs.try_div(rhs),
            Add => lhs.try_add(rhs),
            Sub => lhs.try_sub(rhs),
            Rem => lhs.try_rem(rhs),
            Eq => Ok(lhs.eq_lossy(&rhs).into()),
            Ne => Ok((!lhs.eq_lossy(&rhs)).into()),
            Gt => lhs.try_gt(rhs),
            Ge => lhs.try_ge(rhs),
            Lt => lhs.try_lt(rhs),
            Le => lhs.try_le(rhs),
            Merge => lhs.try_merge(rhs),
            And | Or | Err => unreachable!(),
        }
        .map_err(Into::into)
    }

    fn resolve_batch(&self, ctx: &mut BatchContext) {
        use ast::Opcode::*;
        use value::Value::*;

        match self.opcode {
            Err => {
                return for (resolved, target, state, timezone) in ctx.iter_mut() {
                    let target = &mut *(*target).borrow_mut();
                    let state = &mut *(*state).borrow_mut();
                    let mut ctx = Context::new(target, state, &timezone);
                    let ctx = &mut ctx;
                    *resolved = self.lhs.resolve(ctx).or_else(|_| self.rhs.resolve(ctx));
                }
            }
            Or => {
                return for (resolved, target, state, timezone) in ctx.iter_mut() {
                    let target = &mut *(*target).borrow_mut();
                    let state = &mut *(*state).borrow_mut();
                    let mut ctx = Context::new(target, state, &timezone);
                    let ctx = &mut ctx;
                    *resolved = (|| {
                        self.lhs
                            .resolve(ctx)?
                            .try_or(|| self.rhs.resolve(ctx))
                            .map_err(Into::into)
                    })();
                }
            }
            And => {
                return for (resolved, target, state, timezone) in ctx.iter_mut() {
                    let target = &mut *(*target).borrow_mut();
                    let state = &mut *(*state).borrow_mut();
                    let mut ctx = Context::new(target, state, &timezone);
                    let ctx = &mut ctx;
                    *resolved = (|| match self.lhs.resolve(ctx)? {
                        Null | Boolean(false) => Ok(false.into()),
                        v => v.try_and(self.rhs.resolve(ctx)?).map_err(Into::into),
                    })();
                }
            }
            _ => (),
        }

        self.lhs.resolve_batch(ctx);
        let ctx_lhs_err = ctx.drain_filter(|resolved| resolved.is_err());
        let lhs = {
            let mut moved = vec![Ok(Value::Null); ctx.len()];
            std::mem::swap(ctx.resolved_values_mut(), &mut moved);
            moved
        };
        self.rhs.resolve_batch(ctx);
        let mut ctx_rhs_ok_resolved_values = Vec::with_capacity(ctx.len());
        let mut ctx_rhs_ok_targets = Vec::with_capacity(ctx.len());
        let mut ctx_rhs_ok_states = Vec::with_capacity(ctx.len());
        let mut ctx_rhs_err_resolved_values = Vec::new();
        let mut ctx_rhs_err_targets = Vec::new();
        let mut ctx_rhs_err_states = Vec::new();

        for ((rhs, target, state, _), lhs) in ctx.iter_mut().zip(lhs) {
            if rhs.is_err() {
                let rhs = {
                    let mut moved = Ok(Value::Null);
                    std::mem::swap(rhs, &mut moved);
                    moved
                };

                ctx_rhs_err_resolved_values.push(rhs);
                ctx_rhs_err_targets.push(target);
                ctx_rhs_err_states.push(state);
            } else {
                ctx_rhs_ok_resolved_values.push((lhs, rhs));
                ctx_rhs_ok_targets.push(target);
                ctx_rhs_ok_states.push(state);
            }
        }

        let values = ctx_rhs_ok_resolved_values.into_iter().map(|(lhs, rhs)| {
            let resolved = rhs;
            let lhs = lhs.expect("is not an error");
            let rhs = {
                let mut moved = Ok(Value::Null);
                std::mem::swap(resolved, &mut moved);
                moved
            }
            .expect("is not an error");
            (resolved, lhs, rhs)
        });

        match self.opcode {
            Mul => {
                values.for_each(|(resolved, lhs, rhs)| {
                    *resolved = lhs.try_mul(rhs).map_err(Into::into);
                });
            }
            Div => {
                values.for_each(|(resolved, lhs, rhs)| {
                    *resolved = lhs.try_div(rhs).map_err(Into::into);
                });
            }
            Add => {
                values.for_each(|(resolved, lhs, rhs)| {
                    *resolved = lhs.try_add(rhs).map_err(Into::into);
                });
            }
            Sub => {
                values.for_each(|(resolved, lhs, rhs)| {
                    *resolved = lhs.try_sub(rhs).map_err(Into::into);
                });
            }
            Rem => {
                values.for_each(|(resolved, lhs, rhs)| {
                    *resolved = lhs.try_rem(rhs).map_err(Into::into);
                });
            }
            Eq => {
                values.for_each(|(resolved, lhs, rhs)| {
                    *resolved = Ok(lhs.eq_lossy(&rhs).into());
                });
            }
            Ne => {
                values.for_each(|(resolved, lhs, rhs)| {
                    *resolved = Ok((!lhs.eq_lossy(&rhs)).into());
                });
            }
            Gt => {
                values.for_each(|(resolved, lhs, rhs)| {
                    *resolved = lhs.try_gt(rhs).map_err(Into::into);
                });
            }
            Ge => {
                values.for_each(|(resolved, lhs, rhs)| {
                    *resolved = lhs.try_ge(rhs).map_err(Into::into);
                });
            }
            Lt => {
                values.for_each(|(resolved, lhs, rhs)| {
                    *resolved = lhs.try_lt(rhs).map_err(Into::into);
                });
            }
            Le => {
                values.for_each(|(resolved, lhs, rhs)| {
                    *resolved = lhs.try_le(rhs).map_err(Into::into);
                });
            }
            Merge => {
                values.for_each(|(resolved, lhs, rhs)| {
                    *resolved = lhs.try_merge(rhs).map_err(Into::into);
                });
            }
            And | Or | Err => unreachable!(),
        }

        let ctx_rhs_err = BatchContext::new(
            ctx_rhs_err_resolved_values,
            ctx_rhs_err_targets,
            ctx_rhs_err_states,
            ctx.timezone(),
        );

        ctx.extend(ctx_lhs_err);
        ctx.extend(ctx_rhs_err);
    }

    fn type_def(&self, state: (&LocalEnv, &ExternalEnv)) -> TypeDef {
        use ast::Opcode::{Add, And, Div, Eq, Err, Ge, Gt, Le, Lt, Merge, Mul, Ne, Or, Rem, Sub};
        use value::Kind as K;

        let mut lhs_def = self.lhs.type_def(state);
        let rhs_def = self.rhs.type_def(state);

        match self.opcode {
            // ok/err ?? ok
            Err if rhs_def.is_infallible() => lhs_def.merge_deep(rhs_def).infallible(),

            // ... ?? ...
            Err => lhs_def.merge_deep(rhs_def),

            // null || ...
            Or if lhs_def.is_null() => rhs_def,

            // not null || ...
            Or if !(lhs_def.is_superset(&K::null()) || lhs_def.is_superset(&K::boolean())) => {
                lhs_def
            }

            // ... || ...
            Or if !lhs_def.is_boolean() => {
                // We can remove Null from the lhs since we know that if the lhs is Null
                // we will be taking the rhs and only the rhs type_def will then be relevant.
                lhs_def.remove_null();

                lhs_def.merge_deep(rhs_def)
            }

            Or => lhs_def.merge_deep(rhs_def),

            // ... | ...
            Merge => lhs_def.merge_overwrite(rhs_def),

            // null && ...
            And if lhs_def.is_null() => rhs_def
                .fallible_unless(K::null().or_boolean())
                .with_kind(K::boolean()),

            // ... && ...
            And => lhs_def
                .fallible_unless(K::null().or_boolean())
                .merge_deep(rhs_def.fallible_unless(K::null().or_boolean()))
                .with_kind(K::boolean()),

            // ... == ...
            // ... != ...
            Eq | Ne => lhs_def.merge_deep(rhs_def).with_kind(K::boolean()),

            // "b" >  "a"
            // "a" >= "a"
            // "a" <  "b"
            // "b" <= "b"
            Gt | Ge | Lt | Le if lhs_def.is_bytes() && rhs_def.is_bytes() => {
                lhs_def.merge_deep(rhs_def).with_kind(K::boolean())
            }

            // ... >  ...
            // ... >= ...
            // ... <  ...
            // ... <= ...
            Gt | Ge | Lt | Le => lhs_def
                .fallible_unless(K::integer().or_float())
                .merge_deep(rhs_def.fallible_unless(K::integer().or_float()))
                .with_kind(K::boolean()),

            // ... / ...
            Div => {
                let td = TypeDef::float();

                // Division is infallible if the rhs is a literal normal float or integer.
                match self.rhs.as_value() {
                    Some(value) if lhs_def.is_float() || lhs_def.is_integer() => match value {
                        Value::Float(v) if v.is_normal() => td.infallible(),
                        Value::Integer(v) if v != 0 => td.infallible(),
                        _ => td.fallible(),
                    },
                    _ => td.fallible(),
                }
            }

            // ... % ...
            Rem => {
                // Division is infallible if the rhs is a literal normal float or integer.
                match self.rhs.as_value() {
                    Some(value) if lhs_def.is_float() || lhs_def.is_integer() => match value {
                        Value::Float(v) if v.is_normal() => TypeDef::float().infallible(),
                        Value::Float(_) => TypeDef::float().fallible(),
                        Value::Integer(v) if v != 0 => TypeDef::integer().infallible(),
                        Value::Integer(_) => TypeDef::integer().fallible(),
                        _ => TypeDef::float().add_integer().fallible(),
                    },
                    _ => TypeDef::float().add_integer().fallible(),
                }
            }

            // "bar" + ...
            // ... + "bar"
            Add if lhs_def.is_bytes() || rhs_def.is_bytes() => lhs_def
                .fallible_unless(K::bytes().or_null())
                .merge_deep(rhs_def.fallible_unless(K::bytes().or_null()))
                .with_kind(K::bytes()),

            // ... + 1.0
            // ... - 1.0
            // ... * 1.0
            // ... % 1.0
            // 1.0 + ...
            // 1.0 - ...
            // 1.0 * ...
            // 1.0 % ...
            Add | Sub | Mul if lhs_def.is_float() || rhs_def.is_float() => lhs_def
                .fallible_unless(K::integer().or_float())
                .merge_deep(rhs_def.fallible_unless(K::integer().or_float()))
                .with_kind(K::float()),

            // 1 + 1
            // 1 - 1
            // 1 * 1
            // 1 % 1
            Add | Sub | Mul if lhs_def.is_integer() && rhs_def.is_integer() => {
                lhs_def.merge_deep(rhs_def).with_kind(K::integer())
            }

            // "bar" * 1
            Mul if lhs_def.is_bytes() && rhs_def.is_integer() => {
                lhs_def.merge_deep(rhs_def).with_kind(K::bytes())
            }

            // 1 * "bar"
            Mul if lhs_def.is_integer() && rhs_def.is_bytes() => {
                lhs_def.merge_deep(rhs_def).with_kind(K::bytes())
            }

            // ... + ...
            // ... * ...
            Add | Mul => lhs_def
                .merge_deep(rhs_def)
                .fallible()
                .with_kind(K::bytes().or_integer().or_float()),

            // ... - ...
            Sub => lhs_def
                .merge_deep(rhs_def)
                .fallible()
                .with_kind(K::integer().or_float()),
        }
    }
}

impl fmt::Display for Op {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {} {}", self.lhs, self.opcode, self.rhs)
    }
}

impl fmt::Debug for Op {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Op({} {} {})", self.lhs, self.opcode, self.rhs)
    }
}

// -----------------------------------------------------------------------------

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("comparison operators can't be chained together")]
    ChainedComparison { span: Span },

    #[error("unnecessary error coalescing operation")]
    UnnecessaryCoalesce {
        lhs_span: Span,
        rhs_span: Span,
        op_span: Span,
    },

    #[error("only objects can be merged")]
    MergeNonObjects {
        lhs_span: Option<Span>,
        rhs_span: Option<Span>,
    },

    #[error("fallible operation")]
    Expr(#[from] expression::Error),
}

impl DiagnosticMessage for Error {
    fn code(&self) -> usize {
        use Error::{ChainedComparison, Expr, MergeNonObjects, UnnecessaryCoalesce};

        match self {
            ChainedComparison { .. } => 650,
            UnnecessaryCoalesce { .. } => 651,
            MergeNonObjects { .. } => 652,
            Expr(err) => err.code(),
        }
    }

    fn message(&self) -> String {
        use Error::Expr;

        match self {
            Expr(err) => err.message(),
            err => err.to_string(),
        }
    }

    fn labels(&self) -> Vec<Label> {
        use Error::{ChainedComparison, Expr, MergeNonObjects, UnnecessaryCoalesce};

        match self {
            ChainedComparison { span } => vec![Label::primary("", span)],
            UnnecessaryCoalesce {
                lhs_span,
                rhs_span,
                op_span,
            } => vec![
                Label::primary("this expression can't fail", lhs_span),
                Label::context("this expression never resolves", rhs_span),
                Label::context("remove this error coalescing operation", op_span),
            ],
            MergeNonObjects { lhs_span, rhs_span } => {
                let mut labels = Vec::new();
                if let Some(lhs_span) = lhs_span {
                    labels.push(Label::primary(
                        "this expression must resolve to an object",
                        lhs_span,
                    ));
                }
                if let Some(rhs_span) = rhs_span {
                    labels.push(Label::primary(
                        "this expression must resolve to an object",
                        rhs_span,
                    ));
                }

                labels
            }
            Expr(err) => err.labels(),
        }
    }

    fn notes(&self) -> Vec<Note> {
        use Error::{ChainedComparison, Expr};

        match self {
            ChainedComparison { .. } => vec![Note::SeeDocs(
                "comparisons".to_owned(),
                Urls::expression_docs_url("#comparison"),
            )],
            Expr(err) => err.notes(),
            _ => vec![],
        }
    }
}

// -----------------------------------------------------------------------------

#[cfg(all(test, feature = "expressions"))]
mod tests {
    use std::convert::TryInto;

    use ast::{
        Ident,
        Opcode::{Add, And, Div, Eq, Err, Ge, Gt, Le, Lt, Mul, Ne, Or, Rem, Sub},
    };
    use ordered_float::NotNan;

    use super::*;
    use crate::{
        expression::{Block, IfStatement, Literal, Predicate, Variable},
        test_type_def,
    };

    fn op(
        opcode: ast::Opcode,
        lhs: impl TryInto<Literal> + fmt::Debug + Clone,
        rhs: impl TryInto<Literal> + fmt::Debug + Clone,
    ) -> Op {
        use std::result::Result::Err;

        let lhs = match lhs.clone().try_into() {
            Ok(v) => v,
            Err(_) => panic!("not a valid lhs expression: {:?}", lhs),
        };

        let rhs = match rhs.clone().try_into() {
            Ok(v) => v,
            Err(_) => panic!("not a valid rhs expression: {:?}", rhs),
        };

        Op {
            lhs: Box::new(lhs.into()),
            rhs: Box::new(rhs.into()),
            opcode,
        }
    }

    fn f(f: f64) -> NotNan<f64> {
        NotNan::new(f).unwrap()
    }

    test_type_def![
        or_exact {
            expr: |_| op(Or, "foo", true),
            want: TypeDef::bytes(),
        }

        or_null {
            expr: |_| op(Or, (), true),
            want: TypeDef::boolean(),
        }

        multiply_string_integer {
            expr: |_| op(Mul, "foo", 1),
            want: TypeDef::bytes(),
        }

        multiply_integer_string {
            expr: |_| op(Mul, 1, "foo"),
            want: TypeDef::bytes(),
        }

        multiply_float_integer {
            expr: |_| op(Mul, f(1.0), 1),
            want: TypeDef::float(),
        }

        multiply_integer_float {
            expr: |_| op(Mul, 1, f(1.0)),
            want: TypeDef::float(),
        }

        multiply_integer_integer {
            expr: |_| op(Mul, 1, 1),
            want: TypeDef::integer(),
        }

        multiply_other {
            expr: |_| op(Mul, (), ()),
            want: TypeDef::bytes().fallible().add_integer().add_float(),
        }

        add_string_string {
            expr: |_| op(Add, "foo", "bar"),
            want: TypeDef::bytes(),
        }

        add_string_null {
            expr: |_| op(Add, "foo", ()),
            want: TypeDef::bytes(),
        }

        add_null_string {
            expr: |_| op(Add, (), "foo"),
            want: TypeDef::bytes(),
        }

        add_string_bool {
            expr: |_| op(Add, "foo", true),
            want: TypeDef::bytes().fallible(),
        }

        add_float_integer {
            expr: |_| op(Add, f(1.0), 1),
            want: TypeDef::float(),
        }

        add_integer_float {
            expr: |_| op(Add, 1, f(1.0)),
            want: TypeDef::float(),
        }

        add_float_other {
            expr: |_| op(Add, f(1.0), ()),
            want: TypeDef::float().fallible(),
        }

        add_other_float {
            expr: |_| op(Add, (), f(1.0)),
            want: TypeDef::float().fallible(),
        }

        add_integer_integer {
            expr: |_| op(Add, 1, 1),
            want: TypeDef::integer(),
        }

        add_other {
            expr: |_| op(Add, (), ()),
            want: TypeDef::bytes().add_integer().add_float().fallible(),
        }

        remainder_integer {
            expr: |_| op(Rem, 5, 5),
            want: TypeDef::integer().infallible(),
        }

        remainder_integer_zero {
            expr: |_| op(Rem, 5, 0),
            want: TypeDef::integer().fallible(),
        }

        remainder_float {
            expr: |_| op(Rem, 5.0, 5.0),
            want: TypeDef::float().infallible(),
        }

        remainder_mixed {
            expr: |_| op(Rem, 5, 5.0),
            want: TypeDef::float().infallible(),
        }

        remainder_other {
            expr: |_| op(Rem, 5, ()),
            want: TypeDef::integer().add_float().fallible(),
        }

        subtract_integer {
            expr: |_| op(Sub, 1, 1),
            want: TypeDef::integer().infallible(),
        }

        subtract_float {
            expr: |_| op(Sub, 1.0, 1.0),
            want: TypeDef::float().infallible(),
        }

        subtract_mixed {
            expr: |_| op(Sub, 1, 1.0),
            want: TypeDef::float().infallible(),
        }

        subtract_other {
            expr: |_| op(Sub, 1, ()),
            want: TypeDef::integer().fallible().add_float(),
        }

        divide_integer_literal {
            expr: |_| op(Div, 1, 1),
            want: TypeDef::float().infallible(),
        }

        divide_float_literal {
            expr: |_| op(Div, 1.0, 1.0),
            want: TypeDef::float().infallible(),
        }

        divide_mixed_literal {
            expr: |_| op(Div, 1, 1.0),
            want: TypeDef::float().infallible(),
        }

        divide_float_zero_literal {
            expr: |_| op(Div, 1, 0.0),
            want: TypeDef::float().fallible(),
        }

        divide_integer_zero_literal {
            expr: |_| op(Div, 1, 0),
            want: TypeDef::float().fallible(),
        }

        divide_lhs_literal_wrong_rhs {
            expr: |_| Op {
                lhs: Box::new(Literal::from(true).into()),
                rhs: Box::new(Literal::from(NotNan::new(1.0).unwrap()).into()),
                opcode: Div,
            },
            want: TypeDef::float().fallible(),
        }

        divide_dynamic_rhs {
            expr: |(local, _): (&mut LocalEnv, &mut ExternalEnv)| {
                local.insert_variable(Ident::new("foo"), crate::type_def::Details {
                    type_def: TypeDef::null(),
                    value: None,
                });

                Op {
                    lhs: Box::new(Literal::from(1).into()),
                    rhs: Box::new(Variable::new(Span::default(), Ident::new("foo"), local).unwrap().into()),
                    opcode: Div,
                }
            },
            want: TypeDef::float().fallible(),
        }

        divide_other {
            expr: |_| op(Div, 1.0, ()),
            want: TypeDef::float().fallible(),
        }

        and_null {
            expr: |_| op(And, (), ()),
            want: TypeDef::boolean().infallible(),
        }

        and_boolean {
            expr: |_| op(And, true, true),
            want: TypeDef::boolean().infallible(),
        }

        and_mixed {
            expr: |_| op(And, (), true),
            want: TypeDef::boolean().infallible(),
        }

        and_other {
            expr: |_| op(And, (), "bar"),
            want: TypeDef::boolean().fallible(),
        }

        equal {
            expr: |_| op(Eq, (), ()),
            want: TypeDef::boolean().infallible(),
        }

        not_equal {
            expr: |_| op(Ne, (), "foo"),
            want: TypeDef::boolean().infallible(),
        }

        greater_integer {
            expr: |_| op(Gt, 1, 1),
            want: TypeDef::boolean().infallible(),
        }

        greater_float {
            expr: |_| op(Gt, 1.0, 1.0),
            want: TypeDef::boolean().infallible(),
        }

        greater_mixed {
            expr: |_| op(Gt, 1, 1.0),
            want: TypeDef::boolean().infallible(),
        }

        greater_bytes {
            expr: |_| op(Gt, "c", "b"),
            want: TypeDef::boolean().infallible(),
        }

        greater_other {
            expr: |_| op(Gt, 1, "foo"),
            want: TypeDef::boolean().fallible(),
        }

        greater_or_equal_integer {
            expr: |_| op(Ge, 1, 1),
            want: TypeDef::boolean().infallible(),
        }

        greater_or_equal_float {
            expr: |_| op(Ge, 1.0, 1.0),
            want: TypeDef::boolean().infallible(),
        }

        greater_or_equal_mixed {
            expr: |_| op(Ge, 1, 1.0),
            want: TypeDef::boolean().infallible(),
        }

        greater_or_equal_bytes {
            expr: |_| op(Ge, "foo", "foo"),
            want: TypeDef::boolean().infallible(),
        }

        greater_or_equal_other {
            expr: |_| op(Ge, 1, "foo"),
            want: TypeDef::boolean().fallible(),
        }

        less_integer {
            expr: |_| op(Lt, 1, 1),
            want: TypeDef::boolean().infallible(),
        }

        less_float {
            expr: |_| op(Lt, 1.0, 1.0),
            want: TypeDef::boolean().infallible(),
        }

        less_mixed {
            expr: |_| op(Lt, 1, 1.0),
            want: TypeDef::boolean().infallible(),
        }

        less_bytes {
            expr: |_| op(Lt, "bar", "foo"),
            want: TypeDef::boolean().infallible(),
        }

        less_other {
            expr: |_| op(Lt, 1, "foo"),
            want: TypeDef::boolean().fallible(),
        }

        less_or_equal_integer {
            expr: |_| op(Le, 1, 1),
            want: TypeDef::boolean().infallible(),
        }

        less_or_equal_float {
            expr: |_| op(Le, 1.0, 1.0),
            want: TypeDef::boolean().infallible(),
        }

        less_or_equal_mixed {
            expr: |_| op(Le, 1, 1.0),
            want: TypeDef::boolean().infallible(),
        }

        less_or_equal_bytes {
            expr: |_| op(Le, "bar", "bar"),
            want: TypeDef::boolean().infallible(),
        }

        less_or_equal_other {
            expr: |_| op(Le, 1, "baz"),
            want: TypeDef::boolean().fallible(),
        }

        error_or_rhs_infallible {
            expr: |_| Op {
                lhs: Box::new(Op {
                    lhs: Box::new(Literal::from("foo").into()),
                    rhs: Box::new(Literal::from(1).into()),
                    opcode: Div,
                }.into()),
                rhs: Box::new(Literal::from(true).into()),
                opcode: Err,
            },
            want: TypeDef::float().add_boolean(),
        }

        error_or_fallible {
            expr: |_| Op {
                lhs: Box::new(Op {
                    lhs: Box::new(Literal::from("foo").into()),
                    rhs: Box::new(Literal::from(NotNan::new(0.0).unwrap()).into()),
                    opcode: Div,
                }.into()),
                rhs: Box::new(Op {
                    lhs: Box::new(Literal::from(true).into()),
                    rhs: Box::new(Literal::from(NotNan::new(0.0).unwrap()).into()),
                    opcode: Div,
                }.into()),
                opcode: Err,
            },
            want: TypeDef::float().fallible(),
        }

        error_or_nested_infallible {
            expr: |_| Op {
                lhs: Box::new(Op {
                    lhs: Box::new(Literal::from("foo").into()),
                    rhs: Box::new(Literal::from(1).into()),
                    opcode: Div,
                }.into()),
                rhs: Box::new(Op {
                    lhs: Box::new(Op {
                        lhs: Box::new(Literal::from(true).into()),
                        rhs: Box::new(Literal::from(1).into()),
                        opcode: Div,
                    }.into()),
                    rhs: Box::new(Literal::from("foo").into()),
                    opcode: Err,
                }.into()),
                opcode: Err,
            },
            want: TypeDef::float().add_bytes(),
        }

        or_nullable {
            expr: |_| Op {
                lhs: Box::new(
                    IfStatement {
                        predicate: Predicate::new_unchecked(vec![Literal::from(true).into()]),
                        consequent: Block::new(vec![Literal::from("string").into()], LocalEnv::default()),
                        alternative: None,
                    }.into()),
                rhs: Box::new(Literal::from("another string").into()),
                opcode: Or,
            },
            want: TypeDef::bytes(),
        }

        or_not_nullable {
            expr: |_| Op {
                lhs: Box::new(
                    IfStatement {
                        predicate: Predicate::new_unchecked(vec![Literal::from(true).into()]),
                        consequent: Block::new(vec![Literal::from("string").into()], LocalEnv::default()),
                        alternative:  Some(Block::new(vec![Literal::from(42).into()], LocalEnv::default()))
                }.into()),
                rhs: Box::new(Literal::from("another string").into()),
                opcode: Or,
            },
            want: TypeDef::bytes().add_integer(),
        }
    ];
}
