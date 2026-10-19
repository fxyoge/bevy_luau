//! Portable values and expression source; never VM bytecode or runtime identities.
use super::*;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Serialize, Deserialize)]
enum Value {
    Nil,
    Boolean(bool),
    Integer(i64),
    Number(u64),
    String(String),
    Sequence(Vec<LuauValue>),
    Record(BTreeMap<String, LuauValue>),
}

impl Serialize for LuauValue {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // Match borrowed variants to avoid cloning potentially large datamodels.
        #[derive(Serialize)]
        enum Borrowed<'a> {
            Nil,
            Boolean(bool),
            Integer(i64),
            Number(u64),
            String(&'a str),
            Sequence(&'a [LuauValue]),
            Record(&'a BTreeMap<String, LuauValue>),
        }
        match self {
            Self::Nil => Borrowed::Nil,
            Self::Boolean(v) => Borrowed::Boolean(*v),
            Self::Integer(v) => Borrowed::Integer(*v),
            Self::Number(v) => Borrowed::Number(v.to_bits()),
            Self::String(v) => Borrowed::String(v),
            Self::Sequence(v) => Borrowed::Sequence(v),
            Self::Record(v) => Borrowed::Record(v),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for LuauValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match Value::deserialize(deserializer)? {
            Value::Nil => Self::Nil,
            Value::Boolean(v) => Self::Boolean(v),
            Value::Integer(v) => Self::Integer(v),
            Value::Number(v) => Self::Number(f64::from_bits(v)),
            Value::String(v) => Self::String(v),
            Value::Sequence(v) => Self::Sequence(v),
            Value::Record(v) => Self::Record(v),
        })
    }
}

impl Serialize for CompiledLuauExpression {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        (&self.source, self.boolean, &self.environment).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for CompiledLuauExpression {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let (source, boolean, environment) =
            <(String, bool, EnvironmentSpec)>::deserialize(deserializer)?;
        environment
            .cached_expression(&source, boolean)
            .map(|compiled| (*compiled).clone())
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_preserve_numeric_bits_and_nested_types() {
        for value in [
            0.0,
            -0.0,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::from_bits(0x7ff8_0000_0000_0123),
        ] {
            let saved = serde_json::to_vec(&LuauValue::Sequence(vec![
                LuauValue::Number(value),
                LuauValue::Integer(2),
            ]))
            .unwrap();
            let LuauValue::Sequence(restored) = serde_json::from_slice(&saved).unwrap() else {
                panic!()
            };
            let LuauValue::Number(number) = restored[0] else {
                panic!()
            };
            assert_eq!(number.to_bits(), value.to_bits());
            assert_eq!(restored[1], LuauValue::Integer(2));
        }
    }

    #[test]
    fn expressions_recompile_with_their_original_environment_and_shared_identity() {
        let environment = EnvironmentSpec::builder()
            .library("math", &["random"])
            .read_only(true)
            .build()
            .unwrap();
        let expression = environment
            .compile("math.floor(input.x) + 1", false)
            .unwrap();
        let saved = serde_json::to_vec(&expression).unwrap();
        let first: CompiledLuauExpression = serde_json::from_slice(&saved).unwrap();
        let second: CompiledLuauExpression = serde_json::from_slice(&saved).unwrap();
        assert_eq!(first.identity, second.identity);
        assert_ne!(first.identity, expression.identity);
        let input = [(
            "input".into(),
            LuauValue::Record([("x".into(), LuauValue::Number(2.5))].into()),
        )]
        .into();
        assert_eq!(
            first.evaluate(&input).unwrap(),
            expression.evaluate(&input).unwrap()
        );
        assert_eq!(
            first.evaluate_vm(&input).unwrap(),
            expression.evaluate_vm(&input).unwrap()
        );
        let numeric = numeric::NumericExpression::compile("tier * 2", &["tier"]).unwrap();
        let numeric: numeric::NumericExpression =
            serde_json::from_slice(&serde_json::to_vec(&numeric).unwrap()).unwrap();
        assert_eq!(numeric.evaluate(&[4.0]).unwrap(), 8.0);
    }
}
