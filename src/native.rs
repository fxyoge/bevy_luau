use std::{
    borrow::Cow,
    collections::{BTreeSet, HashMap, HashSet},
};

use crate::{Error, LuauValue as V};
use full_moon::{
    ast::{
        Block, Expression, FunctionBody, FunctionCall, FunctionDeclaration, GenericFor, Index,
        LastStmt, LocalAssignment, LocalFunction, NumericFor, Parameter, Prefix, Repeat, Suffix,
        UnOp, Var, VarExpression,
    },
    tokenizer::{TokenReference, TokenType},
    visitors::Visitor,
};

#[derive(Clone, Debug)]
pub(crate) enum Plan {
    Literal(V),
    Root(String),
    Index(Box<Self>, Box<Self>),
    Not(Box<Self>),
    If(Box<Self>, Box<Self>, Box<Self>),
}

fn name(token: &TokenReference) -> String {
    token.token().to_string()
}

pub(crate) fn compile(ast: &full_moon::ast::Ast) -> Option<Plan> {
    if ast.nodes().stmts().next().is_some() {
        return None;
    }
    let LastStmt::Return(ret) = ast.nodes().last_stmt()? else {
        return None;
    };
    if ret.returns().len() != 1 {
        return None;
    }
    expression(ret.returns().iter().next()?)
}

fn prefix(value: &Prefix) -> Option<Plan> {
    match value {
        Prefix::Name(token) => Some(Plan::Root(name(token))),
        Prefix::Expression(value) => expression(value),
        _ => None,
    }
}

fn indexed(value: &VarExpression) -> Option<Plan> {
    let mut plan = prefix(value.prefix())?;
    for suffix in value.suffixes() {
        let key = match suffix {
            Suffix::Index(Index::Dot { name: token, .. }) => Plan::Literal(V::String(name(token))),
            Suffix::Index(Index::Brackets {
                expression: value, ..
            }) => expression(value)?,
            _ => return None,
        };
        plan = Plan::Index(Box::new(plan), Box::new(key));
    }
    Some(plan)
}

fn expression(value: &Expression) -> Option<Plan> {
    Some(match value {
        Expression::Parentheses {
            expression: value, ..
        } => expression(value)?,
        Expression::Var(Var::Name(token)) => Plan::Root(name(token)),
        Expression::Var(Var::Expression(value)) => indexed(value)?,
        Expression::UnaryOperator {
            unop: UnOp::Not(_),
            expression: value,
        } => Plan::Not(Box::new(expression(value)?)),
        Expression::IfExpression(value) if value.else_if_expressions().is_none() => Plan::If(
            Box::new(expression(value.condition())?),
            Box::new(expression(value.if_expression())?),
            Box::new(expression(value.else_expression())?),
        ),
        Expression::Symbol(token) => Plan::Literal(match name(token).as_str() {
            "nil" => V::Nil,
            "true" => V::Boolean(true),
            "false" => V::Boolean(false),
            _ => return None,
        }),
        Expression::Number(token) => {
            let text = name(token);
            // Complex number spellings remain the VM's responsibility.
            if !text
                .bytes()
                .all(|c| c.is_ascii_digit() || matches!(c, b'.' | b'e' | b'E' | b'+' | b'-'))
            {
                return None;
            }
            Plan::Literal(V::Number(text.parse().ok()?))
        }
        Expression::String(token) => {
            let TokenType::StringLiteral { literal, .. } = token.token().token_type() else {
                return None;
            };
            if literal.contains(['\\', '\n', '\r']) {
                return None;
            }
            Plan::Literal(V::String(literal.to_string()))
        }
        _ => return None,
    })
}

pub(crate) enum Evaluated<'a> {
    Portable(Cow<'a, V>),
    // String metatable reads can produce a builtin function as an intermediate
    // value. It is truthy, but cannot escape as a portable result.
    Function,
}

fn truthy(value: &Evaluated<'_>) -> bool {
    !matches!(value, Evaluated::Portable(value) if matches!(value.as_ref(), V::Nil | V::Boolean(false)))
}

impl Plan {
    pub(crate) fn roots_declared<'a>(&self, lookup: &impl Fn(&str) -> Option<&'a V>) -> bool {
        match self {
            Self::Root(root) => lookup(root).is_some(),
            Self::Index(a, b) => a.roots_declared(lookup) && b.roots_declared(lookup),
            Self::Not(a) => a.roots_declared(lookup),
            Self::If(a, b, c) => {
                a.roots_declared(lookup) && b.roots_declared(lookup) && c.roots_declared(lookup)
            }
            Self::Literal(_) => true,
        }
    }

    pub(crate) fn evaluate<'a>(
        &'a self,
        lookup: &impl Fn(&str) -> Option<&'a V>,
    ) -> Result<V, Error> {
        match self.evaluate_value(lookup)? {
            Evaluated::Portable(value) => Ok(value.into_owned()),
            Evaluated::Function => Err(Error::runtime("Luau function values cannot be persisted")),
        }
    }

    fn evaluate_value<'a>(
        &'a self,
        lookup: &impl Fn(&str) -> Option<&'a V>,
    ) -> Result<Evaluated<'a>, Error> {
        use Evaluated::Portable as P;
        let eval = |plan: &'a Self| plan.evaluate_value(lookup);
        Ok(match self {
            Self::Literal(value) => P(Cow::Borrowed(value)),
            Self::Root(root) => P(Cow::Borrowed(lookup(root).unwrap_or(&V::Nil))),
            Self::Not(value) => P(Cow::Owned(V::Boolean(!truthy(&eval(value)?)))),
            Self::If(condition, yes, no) => eval(if truthy(&eval(condition)?) { yes } else { no })?,
            Self::Index(value, key) => {
                let value = eval(value)?;
                let key = eval(key)?;
                index(value, &key)?
            }
        })
    }
}

fn index<'a>(value: Evaluated<'a>, key: &Evaluated<'_>) -> Result<Evaluated<'a>, Error> {
    use Evaluated::Portable as P;
    // Most native intermediates borrow the original input. An owned intermediate
    // (e.g. a VM string-library lookup) must own its selected result as well.
    if let P(Cow::Owned(value)) = value {
        return index(P(Cow::Borrowed(&value)), key).map(|result| match result {
            P(value) => P(Cow::Owned(value.into_owned())),
            Evaluated::Function => Evaluated::Function,
        });
    }
    let borrowed = |value| P(Cow::Borrowed(value));
    Ok(match (value, key) {
        (P(Cow::Borrowed(V::Record(record))), P(key)) => match key.as_ref() {
            V::String(key) => borrowed(record.get(key).unwrap_or(&V::Nil)),
            _ => borrowed(&V::Nil),
        },
        (P(Cow::Borrowed(V::Record(_))), _) => borrowed(&V::Nil),
        (P(Cow::Borrowed(V::Sequence(sequence))), P(key)) => {
            let index = match key.as_ref() {
                V::Integer(index) if *index > 0 => Some((*index - 1) as usize),
                V::Number(index)
                    if index.fract() == 0.0 && *index >= 1.0 && *index <= usize::MAX as f64 =>
                {
                    Some((*index as usize).saturating_sub(1))
                }
                _ => None,
            };
            borrowed(
                index
                    .and_then(|index| sequence.get(index))
                    .unwrap_or(&V::Nil),
            )
        }
        (P(Cow::Borrowed(V::Sequence(_))), _) => borrowed(&V::Nil),
        (P(Cow::Borrowed(V::String(_))), P(key)) => crate::string_index(key)?,
        (P(Cow::Borrowed(V::String(_))), Evaluated::Function) => borrowed(&V::Nil),
        _ => return Err(Error::runtime("attempt to index a non-table value")),
    })
}

/// Match Luau's double-based numbers and table persistence representation.
pub(crate) fn normalize(value: V) -> Result<V, Error> {
    fn visit(value: V, depth: usize, remaining: &mut usize) -> Result<V, Error> {
        if depth >= 64 || *remaining == 0 {
            return Err(Error::runtime("Luau value exceeds persistence limits"));
        }
        *remaining -= 1;
        Ok(match value {
            V::Integer(n) => number(n as f64),
            V::Number(n) => number(n),
            V::Record(values) => {
                let values = values
                    .into_iter()
                    .filter(|(_, v)| !matches!(v, V::Nil))
                    .map(|(k, v)| Ok((k, visit(v, depth + 1, remaining)?)))
                    .collect::<Result<std::collections::BTreeMap<_, _>, Error>>()?;
                if values.is_empty() {
                    V::Sequence(vec![])
                } else {
                    V::Record(values)
                }
            }
            V::Sequence(mut values) => {
                while matches!(values.last(), Some(V::Nil)) {
                    values.pop();
                }
                if values.iter().any(|v| matches!(v, V::Nil)) {
                    return Err(Error::runtime("sparse Luau sequences cannot be persisted"));
                }
                V::Sequence(
                    values
                        .into_iter()
                        .map(|v| visit(v, depth + 1, remaining))
                        .collect::<Result<_, _>>()?,
                )
            }
            other => other,
        })
    }
    visit(value, 0, &mut 100_000)
}

fn number(n: f64) -> V {
    let integer = n as i64;
    if n.to_bits() == (integer as f64).to_bits() {
        V::Integer(integer)
    } else {
        V::Number(n)
    }
}

#[derive(Default)]
pub(crate) struct Dependencies {
    pub roots: BTreeSet<String>,
    scopes: Vec<HashSet<String>>,
    block_names: HashMap<usize, HashSet<String>>,
    repeat_blocks: HashSet<usize>,
}

impl Dependencies {
    fn prefix(&mut self, prefix: &Prefix) {
        if let Prefix::Name(token) = prefix {
            self.read(token);
        }
    }
    fn read(&mut self, token: &TokenReference) {
        let root = name(token);
        if !self.scopes.iter().rev().any(|scope| scope.contains(&root)) {
            self.roots.insert(root);
        }
    }
}

impl Visitor for Dependencies {
    fn visit_var(&mut self, value: &Var) {
        if let Var::Name(token) = value {
            self.read(token);
        }
    }
    fn visit_var_expression(&mut self, value: &VarExpression) {
        self.prefix(value.prefix());
    }
    fn visit_function_call(&mut self, value: &FunctionCall) {
        self.prefix(value.prefix());
    }
    fn visit_block(&mut self, block: &Block) {
        self.scopes.push(
            self.block_names
                .remove(&(block as *const Block as usize))
                .unwrap_or_default(),
        );
    }
    fn visit_block_end(&mut self, block: &Block) {
        if !self
            .repeat_blocks
            .contains(&(block as *const Block as usize))
        {
            self.scopes.pop();
        }
    }
    fn visit_local_assignment_end(&mut self, assignment: &LocalAssignment) {
        if let Some(scope) = self.scopes.last_mut() {
            scope.extend(assignment.names().iter().map(name));
        }
    }
    fn visit_local_function(&mut self, function: &LocalFunction) {
        if let Some(scope) = self.scopes.last_mut() {
            scope.insert(name(function.name()));
        }
    }
    fn visit_function_body(&mut self, body: &FunctionBody) {
        let names = body
            .parameters()
            .iter()
            .filter_map(|parameter| match parameter {
                Parameter::Name(token) => Some(name(token)),
                _ => None,
            });
        self.block_names
            .entry(body.block() as *const Block as usize)
            .or_default()
            .extend(names);
    }
    fn visit_function_declaration(&mut self, declaration: &FunctionDeclaration) {
        if let Some(root) = declaration.name().names().iter().next() {
            self.read(root);
        }
        if declaration.name().method_name().is_some() {
            self.block_names
                .entry(declaration.body().block() as *const Block as usize)
                .or_default()
                .insert("self".into());
        }
    }
    fn visit_numeric_for(&mut self, loop_: &NumericFor) {
        self.block_names.insert(
            loop_.block() as *const Block as usize,
            HashSet::from([name(loop_.index_variable())]),
        );
    }
    fn visit_generic_for(&mut self, loop_: &GenericFor) {
        self.block_names.insert(
            loop_.block() as *const Block as usize,
            loop_.names().iter().map(name).collect(),
        );
    }
    fn visit_repeat(&mut self, loop_: &Repeat) {
        self.repeat_blocks
            .insert(loop_.block() as *const Block as usize);
    }
    fn visit_repeat_end(&mut self, _: &Repeat) {
        self.scopes.pop();
    }
}
