use bevy_luau::{EnvironmentSpec, LuauValue as V};
use std::collections::BTreeMap;

fn environment() -> EnvironmentSpec {
    EnvironmentSpec::builder()
        .map()
        .read_only(true)
        .build()
        .unwrap()
}

#[test]
fn map_transforms_filters_and_preserves_order_false_zero_and_nested_results() {
    let env = environment();
    for (source, expected) in [
        ("map({}, function(v) return v end)", V::Sequence(vec![])),
        (
            "map({1,2,3,4}, function(v) return if v % 2 == 0 then v * 3 else nil end)",
            V::Sequence(vec![V::Integer(6), V::Integer(12)]),
        ),
        (
            "map({1,2}, function(v) return if v == 1 then false else 0 end)",
            V::Sequence(vec![V::Boolean(false), V::Integer(0)]),
        ),
        (
            "map({1,2}, function(v) return nil end)",
            V::Sequence(vec![]),
        ),
        (
            "map({{1,2},{3}}, function(v) return map(v, function(n) return n + 1 end) end)",
            V::Sequence(vec![
                V::Sequence(vec![V::Integer(2), V::Integer(3)]),
                V::Sequence(vec![V::Integer(4)]),
            ]),
        ),
    ] {
        let expression = env.compile(source, false).unwrap();
        assert_eq!(
            expression.evaluate(&BTreeMap::new()).unwrap(),
            expected,
            "{source}"
        );
        assert_eq!(expression.evaluate_vm(&BTreeMap::new()).unwrap(), expected);
    }
}

#[test]
fn map_callbacks_capture_inputs_without_mutating_them_and_errors_recover() {
    let env = environment();
    let input = BTreeMap::from([
        (
            "values".into(),
            V::Sequence(vec![V::Integer(2), V::Integer(4)]),
        ),
        ("factor".into(), V::Integer(3)),
    ]);
    let expression = env
        .compile("map(values, function(v) return v * factor end)", false)
        .unwrap();
    assert!(expression.dependencies.contains(&"factor".into()));
    assert_eq!(
        expression.evaluate(&input).unwrap(),
        V::Sequence(vec![V::Integer(6), V::Integer(12)])
    );
    for source in [
        "map(values, function(v) values[1] = 99 return v end)",
        "map({name=1}, function(v) return v end)",
        "map({[1]=1,[3]=3}, function(v) return v end)",
        "map(1, function(v) return v end)",
        "map({1}, 42)",
        "map({1}, function(v) return v.missing end)",
        "map({1}, function(v) while true do end end)",
    ] {
        assert!(
            env.compile(source, false)
                .unwrap()
                .evaluate(&input)
                .is_err(),
            "{source}"
        );
        assert_eq!(
            expression.evaluate(&input).unwrap(),
            V::Sequence(vec![V::Integer(6), V::Integer(12)])
        );
    }
    assert_eq!(
        input["values"],
        V::Sequence(vec![V::Integer(2), V::Integer(4)])
    );
}

#[test]
fn map_is_explicit_reserved_and_portable_between_threads() {
    let env = environment();
    assert!(EnvironmentSpec::builder().map().map().build().is_err());
    assert!(
        EnvironmentSpec::builder()
            .build()
            .unwrap()
            .compile("map({}, function(v) return v end)", false)
            .unwrap()
            .evaluate(&BTreeMap::new())
            .is_err()
    );
    let expression = env
        .compile("map({1,2}, function(v) return v end)", false)
        .unwrap();
    assert!(
        expression
            .evaluate(&BTreeMap::from([("map".into(), V::Nil)]))
            .is_err()
    );
    std::thread::spawn(move || {
        assert_eq!(
            expression.evaluate(&BTreeMap::new()).unwrap(),
            V::Sequence(vec![V::Integer(1), V::Integer(2)])
        )
    })
    .join()
    .unwrap();
}

#[cfg(feature = "serde")]
#[test]
fn map_environment_survives_serialization() {
    let expression = environment()
        .compile("map({1,2}, function(v) return v * 2 end)", false)
        .unwrap();
    let json = serde_json::to_vec(&expression).unwrap();
    let restored: bevy_luau::CompiledLuauExpression = serde_json::from_slice(&json).unwrap();
    assert_eq!(
        restored.evaluate(&BTreeMap::new()).unwrap(),
        V::Sequence(vec![V::Integer(2), V::Integer(4)])
    );
}
