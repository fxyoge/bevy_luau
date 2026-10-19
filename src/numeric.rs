//! Schema-bound, allocation-free scalar Luau expressions. Compilation rejects
//! unsupported syntax instead of falling back to a VM. Inputs are numeric slots;
//! booleans are supported as intermediate values and follow Luau truthiness.
use full_moon::ast::{Expression, FunctionArgs, Index, LastStmt, Prefix, Suffix, Var};

#[derive(Clone, Debug)]
pub struct NumericExpression {
    #[cfg(feature = "serde")]
    source: String,
    #[cfg(feature = "serde")]
    input_names: Vec<String>,
    plan: Node,
    inputs: usize,
    /// Referenced input indices, sorted and deduplicated, for consumer invalidation.
    pub dependencies: Vec<usize>,
}

#[derive(Clone, Debug)]
enum Node {
    Number(f64),
    Boolean(bool),
    Input(usize),
    Unary(String, Box<Node>),
    Binary(String, Box<Node>, Box<Node>),
    If(Box<Node>, Box<Node>, Box<Node>),
    Call(String, Vec<Node>),
}

#[derive(Clone, Copy)]
enum Scalar {
    Number(f64),
    Boolean(bool),
}
impl Scalar {
    fn truthy(self) -> bool {
        !matches!(self, Self::Boolean(false))
    }
    fn number(self) -> Result<f64, &'static str> {
        match self {
            Self::Number(value) => Ok(value),
            Self::Boolean(_) => Err("expected a number"),
        }
    }
}

impl NumericExpression {
    pub fn compile(source: &str, inputs: &[&str]) -> Result<Self, String> {
        let mut unique = std::collections::HashSet::new();
        for name in inputs {
            if name.is_empty() || name.split('.').next() == Some("math") || !unique.insert(*name) {
                return Err(format!("invalid or duplicate numeric input {name:?}"));
            }
        }
        // Also validate with the actual Luau compiler, keeping parser extensions
        // from accidentally accepting a language the VM would reject.
        mlua::Compiler::new()
            .compile(format!("return ({source})"))
            .map_err(|e| e.to_string())?;
        let ast = full_moon::parse(&format!("return ({source})"))
            .map_err(|e| format!("invalid numeric formula: {e:?}"))?;
        let Some(LastStmt::Return(ret)) = ast.nodes().last_stmt() else {
            return Err("expected one numeric expression".into());
        };
        let mut dependencies = Vec::new();
        let mut remaining = 4096;
        let plan = compile(
            ret.returns().iter().next().unwrap(),
            inputs,
            &mut dependencies,
            0,
            &mut remaining,
        )?;
        if plan.boolean() {
            return Err("numeric formula must return a number".into());
        }
        dependencies.sort_unstable();
        dependencies.dedup();
        Ok(Self {
            #[cfg(feature = "serde")]
            source: source.into(),
            #[cfg(feature = "serde")]
            input_names: inputs.iter().map(|name| (*name).into()).collect(),
            plan,
            inputs: inputs.len(),
            dependencies,
        })
    }

    /// No allocations or VM access on success. Nonfinite arithmetic follows Luau;
    /// consumers validate domain-specific output constraints at their boundary.
    pub fn evaluate(&self, inputs: &[f64]) -> Result<f64, &'static str> {
        if inputs.len() != self.inputs {
            return Err("numeric input schema mismatch");
        }
        self.plan.evaluate(inputs)?.number()
    }
}

#[cfg(feature = "serde")]
impl serde::Serialize for NumericExpression {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        (&self.source, &self.input_names).serialize(serializer)
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for NumericExpression {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let (source, inputs) = <(String, Vec<String>)>::deserialize(deserializer)?;
        let inputs: Vec<_> = inputs.iter().map(String::as_str).collect();
        Self::compile(&source, &inputs).map_err(serde::de::Error::custom)
    }
}

fn compile(
    value: &Expression,
    inputs: &[&str],
    dependencies: &mut Vec<usize>,
    depth: usize,
    remaining: &mut usize,
) -> Result<Node, String> {
    *remaining = remaining
        .checked_sub(1)
        .ok_or("numeric formula exceeds 4096 nodes")?;
    if depth >= 64 {
        return Err("numeric formula exceeds 64 expression levels".into());
    }
    let mut child = |value: &Expression| compile(value, inputs, dependencies, depth + 1, remaining);
    let plan = match value {
        Expression::Parentheses { expression, .. } => child(expression)?,
        Expression::Number(token) => Node::Number(
            token
                .token()
                .to_string()
                .trim()
                .parse::<f64>()
                .map_err(|_| "numeric formulas require decimal literals")?,
        ),
        Expression::Symbol(token) => match token.to_string().trim() {
            "true" => Node::Boolean(true),
            "false" => Node::Boolean(false),
            _ => return Err("unsupported numeric literal".into()),
        },
        Expression::Var(var) => {
            let path = match var {
                Var::Name(name) => name.token().to_string(),
                Var::Expression(var) => static_path(var.prefix(), var.suffixes())?,
                _ => return Err("unsupported numeric input".into()),
            };
            let index = inputs
                .iter()
                .position(|input| *input == path)
                .ok_or_else(|| format!("undeclared numeric input {path:?}"))?;
            dependencies.push(index);
            Node::Input(index)
        }
        Expression::UnaryOperator { unop, expression } => {
            let op = unop.to_string().trim().to_owned();
            let value = child(expression)?;
            if op != "not" && (op != "-" || value.boolean()) {
                return Err(format!("unsupported numeric unary operator {op:?}"));
            }
            Node::Unary(op, Box::new(value))
        }
        Expression::BinaryOperator { lhs, binop, rhs } => {
            let op = binop.to_string().trim().to_owned();
            let left = child(lhs)?;
            let right = child(rhs)?;
            if !matches!(
                op.as_str(),
                "+" | "-"
                    | "*"
                    | "/"
                    | "//"
                    | "%"
                    | "^"
                    | "<"
                    | "<="
                    | ">"
                    | ">="
                    | "=="
                    | "~="
                    | "and"
                    | "or"
            ) {
                return Err(format!("unsupported numeric binary operator {op:?}"));
            }
            if matches!(op.as_str(), "and" | "or" | "==" | "~=") {
                if left.boolean() != right.boolean() {
                    return Err("numeric operands must have matching types".into());
                }
            } else if left.boolean() || right.boolean() {
                return Err("arithmetic and ordering require numeric operands".into());
            }
            Node::Binary(op, Box::new(left), Box::new(right))
        }
        Expression::IfExpression(value) => {
            let condition = child(value.condition())?;
            let yes = child(value.if_expression())?;
            let mut no = child(value.else_expression())?;
            if let Some(elseifs) = value.else_if_expressions() {
                if depth + elseifs.len() >= 64 {
                    return Err("numeric conditional exceeds 64 levels".into());
                }
                for branch in elseifs.iter().rev() {
                    let condition = child(branch.condition())?;
                    let branch = child(branch.expression())?;
                    if branch.boolean() != no.boolean() {
                        return Err("numeric conditional branches must have matching types".into());
                    }
                    no = Node::If(Box::new(condition), Box::new(branch), Box::new(no));
                }
            }
            if yes.boolean() != no.boolean() {
                return Err("numeric conditional branches must have matching types".into());
            }
            Node::If(Box::new(condition), Box::new(yes), Box::new(no))
        }
        Expression::FunctionCall(call) => {
            let mut suffixes: Vec<_> = call.suffixes().collect();
            let Some(Suffix::Call(full_moon::ast::Call::AnonymousCall(
                FunctionArgs::Parentheses { arguments, .. },
            ))) = suffixes.pop()
            else {
                return Err("numeric formulas support only pure math calls".into());
            };
            let path = static_path(call.prefix(), suffixes.into_iter())?;
            let name = path
                .strip_prefix("math.")
                .ok_or("numeric formulas support only math calls")?;
            let count = arguments.len();
            let allowed = match name {
                "floor" | "ceil" | "round" | "abs" | "sqrt" | "exp" => count == 1,
                "pow" => count == 2,
                "clamp" => count == 3,
                "min" | "max" => count > 0,
                _ => false,
            };
            if !allowed {
                return Err(format!(
                    "unsupported numeric function or arity: {path}/{count}"
                ));
            }
            let args = arguments
                .iter()
                .map(&mut child)
                .collect::<Result<Vec<_>, _>>()?;
            if args.iter().any(Node::boolean) {
                return Err("numeric math calls require numbers".into());
            }
            Node::Call(name.into(), args)
        }
        _ => return Err("unsupported syntax in numeric formula".into()),
    };
    Ok(plan)
}

fn static_path<'a>(
    prefix: &Prefix,
    suffixes: impl Iterator<Item = &'a Suffix>,
) -> Result<String, String> {
    let Prefix::Name(name) = prefix else {
        return Err("numeric input paths must start with a name".into());
    };
    let mut path = name.token().to_string();
    for suffix in suffixes {
        let Suffix::Index(Index::Dot { name, .. }) = suffix else {
            return Err("numeric input paths require static dot indexing".into());
        };
        path.push('.');
        path.push_str(&name.token().to_string());
    }
    Ok(path)
}

impl Node {
    fn boolean(&self) -> bool {
        match self {
            Self::Boolean(_) => true,
            Self::Unary(op, _) => op == "not",
            Self::Binary(op, left, _) => match op.as_str() {
                "<" | "<=" | ">" | ">=" | "==" | "~=" => true,
                "and" | "or" => left.boolean(),
                _ => false,
            },
            Self::If(_, yes, _) => yes.boolean(),
            _ => false,
        }
    }
    fn evaluate(&self, inputs: &[f64]) -> Result<Scalar, &'static str> {
        use Scalar::{Boolean as B, Number as N};
        Ok(match self {
            Self::Number(n) => N(*n),
            Self::Boolean(b) => B(*b),
            Self::Input(index) => N(inputs[*index]),
            Self::Unary(op, value) => {
                let value = value.evaluate(inputs)?;
                if op == "not" {
                    B(!value.truthy())
                } else {
                    N(-value.number()?)
                }
            }
            Self::If(condition, yes, no) => if condition.evaluate(inputs)?.truthy() {
                yes
            } else {
                no
            }
            .evaluate(inputs)?,
            Self::Binary(op, left, right) => {
                let left = left.evaluate(inputs)?;
                if op == "and" {
                    return if left.truthy() {
                        right.evaluate(inputs)
                    } else {
                        Ok(left)
                    };
                }
                if op == "or" {
                    return if left.truthy() {
                        Ok(left)
                    } else {
                        right.evaluate(inputs)
                    };
                }
                let right = right.evaluate(inputs)?;
                if matches!(op.as_str(), "==" | "~=") {
                    let equal = match (left, right) {
                        (N(a), N(b)) => a == b,
                        (B(a), B(b)) => a == b,
                        _ => false,
                    };
                    B(if op == "==" { equal } else { !equal })
                } else {
                    let (a, b) = (left.number()?, right.number()?);
                    match op.as_str() {
                        "+" => N(a + b),
                        "-" => N(a - b),
                        "*" => N(a * b),
                        "/" => N(a / b),
                        "//" => N((a / b).floor()),
                        "%" => N(a - (a / b).floor() * b),
                        "^" => N(a.powf(b)),
                        "<" => B(a < b),
                        "<=" => B(a <= b),
                        ">" => B(a > b),
                        ">=" => B(a >= b),
                        _ => unreachable!("validated numeric operation"),
                    }
                }
            }
            Self::Call(name, args) => {
                let arg = |i: usize| args[i].evaluate(inputs)?.number();
                N(match name.as_str() {
                    "floor" => arg(0)?.floor(),
                    "ceil" => arg(0)?.ceil(),
                    "round" => arg(0)?.round(),
                    "abs" => arg(0)?.abs(),
                    "sqrt" => arg(0)?.sqrt(),
                    "exp" => arg(0)?.exp(),
                    "pow" => arg(0)?.powf(arg(1)?),
                    "clamp" => {
                        let (value, min, max) = (arg(0)?, arg(1)?, arg(2)?);
                        if min > max || min.is_nan() || max.is_nan() {
                            return Err("invalid clamp bounds");
                        }
                        if value < min {
                            min
                        } else if value > max {
                            max
                        } else {
                            value
                        }
                    }
                    "min" | "max" => {
                        let mut value = arg(0)?;
                        for i in 1..args.len() {
                            let next = arg(i)?;
                            if (name == "min" && next < value) || (name == "max" && next > value) {
                                value = next;
                            }
                        }
                        value
                    }
                    _ => unreachable!("validated numeric intrinsic"),
                })
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn numeric_edge_cases_match_vm_results_and_errors() {
        let lua = mlua::Lua::new();
        for source in [
            "x+y",
            "x-y",
            "x*y",
            "x/y",
            "x//y",
            "x%y",
            "x^y",
            "math.min(x,y)",
            "math.max(x,y)",
            "math.clamp(1,x,y)",
            "math.floor(x)",
            "math.ceil(x)",
            "math.round(x)",
            "math.sqrt(x)",
            "math.exp(x)",
            "if x ~= y then 1 else 0",
        ] {
            let plan = NumericExpression::compile(source, &["x", "y"]).unwrap();
            for (x, y) in [
                (0.0, 0.0),
                (-0.0, 2.0),
                (-7.0, 3.0),
                (7.0, -3.0),
                (f64::INFINITY, 2.0),
                (2.0, f64::INFINITY),
                (f64::NAN, 2.0),
                (2.0, f64::NAN),
                (1e30, 3.0),
            ] {
                lua.globals().set("x", x).unwrap();
                lua.globals().set("y", y).unwrap();
                let expected = lua.load(format!("return ({source})")).eval::<f64>();
                let actual = plan.evaluate(&[x, y]);
                assert_eq!(actual.is_ok(), expected.is_ok(), "{source}: {x},{y}");
                if let (Ok(actual), Ok(expected)) = (actual, expected) {
                    assert!(
                        actual.to_bits() == expected.to_bits()
                            || (actual.is_nan() && expected.is_nan()),
                        "{source}: {x},{y}: {actual} != {expected}"
                    );
                }
            }
        }
    }
    #[test]
    fn numeric_plans_match_luau_and_reject_vm_only_constructs() {
        let lua = mlua::Lua::new();
        lua.globals().set("x", 1.505).unwrap();
        lua.globals().set("y", 3.0).unwrap();
        for source in [
            "100 * x",
            "math.floor(100*x+0.5)",
            "if x > 2 then 4 elseif y == 3 then 5 else 6",
            "if 0 then 4 else 5",
            "if not false then 8 else 9",
            "-7 % y",
            "-7 // y",
            "x^y",
            "math.clamp(x, 1, 2)",
            "math.min(x,y,-2)",
            "math.max(x,y,-2)",
            "math.round(-1.5)",
            "0 and y",
            "0 or y",
        ] {
            let plan = NumericExpression::compile(source, &["x", "y"]).unwrap();
            let expected: f64 = lua.load(format!("return ({source})")).eval().unwrap();
            assert_eq!(plan.evaluate(&[1.505, 3.0]).unwrap(), expected, "{source}");
        }
        for source in [
            "missing + 1",
            "math.random()",
            "{x=1}",
            "(function() return 1 end)()",
            "x[y]",
            "true + 1",
            "if true then 1 else false",
            "math.floor(true)",
        ] {
            assert!(
                NumericExpression::compile(source, &["x", "y"]).is_err(),
                "{source}"
            );
        }
        let plan =
            NumericExpression::compile("tool.quality*100", &["unused", "tool.quality"]).unwrap();
        assert_eq!(plan.dependencies, [1]);
        assert_eq!(plan.evaluate(&[0.0, 2.0]).unwrap(), 200.0);
        assert!(plan.evaluate(&[2.0]).is_err());
    }
}
