use bevy_luau::{LuauValue as V, compile_readonly};
use std::collections::BTreeMap;

fn inputs() -> BTreeMap<String, V> {
    BTreeMap::from([
        ("open".into(), V::Boolean(true)),
        ("selected".into(), V::String("oak".into())),
        (
            "labels".into(),
            V::Record(BTreeMap::from([("oak".into(), V::String("Oak".into()))])),
        ),
        (
            "items".into(),
            V::Sequence(vec![V::Integer(12), V::Boolean(false)]),
        ),
        ("nothing".into(), V::Nil),
        ("large".into(), V::Integer(9_007_199_254_740_993)),
        ("negative_zero".into(), V::Number(-0.0)),
    ])
}

#[test]
fn native_reads_match_vm_semantics() {
    for source in [
        "true",
        "0",
        "1.25",
        "'ready'",
        "not open",
        "not not items[2]",
        "labels[selected]",
        "labels.missing",
        "items[1]",
        "items[0]",
        "items[99]",
        "if open then items[1] else nothing.bad",
        "large",
        "negative_zero",
        "items",
        "labels",
        "nothing",
        "('abc').missing",
        "('abc')[1]",
        "not ('abc').upper",
        "if ('abc').upper then 3 else 8",
    ] {
        let expression = compile_readonly(source).unwrap();
        assert!(expression.is_native(), "{source}");
        assert_eq!(
            expression.evaluate(&inputs()).unwrap(),
            expression.evaluate_vm(&inputs()).unwrap(),
            "{source}"
        );
    }
}

#[test]
fn vm_fallback_supports_records_helpers_and_pure_functions() {
    for source in [
        "if open then {x=0,y=0,z=3} else {x=-28,y=205,z=8}",
        "math.clamp(items[1] / 10, 0, 1)",
        "string.upper(labels[selected])",
        "(function() local result = {} for i, v in ipairs(items) do result[i] = tostring(v) end return result end)()",
    ] {
        let expression = compile_readonly(source).unwrap();
        assert!(!expression.is_native(), "{source}");
        assert_eq!(
            expression.evaluate(&inputs()).unwrap(),
            expression.evaluate_vm(&inputs()).unwrap()
        );
    }
}

#[test]
fn presentation_inputs_and_globals_are_deeply_immutable() {
    for source in [
        "(function() labels.oak = 'Changed' return true end)()",
        "(function() table.insert(items, 5) return true end)()",
        "(function() open = false return true end)()",
        "(function() leaked = true return true end)()",
        "(function() math.clamp = nil return true end)()",
        "math.random()",
        "getfenv()",
        "require('anything')",
    ] {
        let expression = compile_readonly(source).unwrap();
        assert!(expression.evaluate(&inputs()).is_err(), "{source}");
        assert!(
            expression.evaluate(&inputs()).is_err(),
            "cached recovery: {source}"
        );
    }
    assert_eq!(
        compile_readonly("labels.oak")
            .unwrap()
            .evaluate(&inputs())
            .unwrap(),
        V::String("Oak".into())
    );
}

#[test]
fn scopes_and_nil_shadow_builtins_even_with_cached_functions() {
    let expression = compile_readonly("type(math)").unwrap();
    assert_eq!(
        expression.evaluate(&BTreeMap::new()).unwrap(),
        V::String("table".into())
    );
    assert_eq!(
        expression
            .evaluate(&BTreeMap::from([("math".into(), V::Nil)]))
            .unwrap(),
        V::String("nil".into())
    );
    assert_eq!(
        expression.evaluate(&BTreeMap::new()).unwrap(),
        V::String("table".into())
    );
}

#[test]
fn dependencies_include_keys_and_nested_function_reads() {
    let expression =
        compile_readonly("(function() return labels[selected] .. tostring(items[1]) end)()")
            .unwrap();
    assert_eq!(
        expression.dependencies,
        ["items", "labels", "selected", "tostring"]
    );
}

#[test]
fn dependencies_respect_locals_parameters_loops_and_repeat_conditions() {
    for (source, expected) in [
        (
            "(function(x) local y=x return y + outside end)(input)",
            vec!["input", "outside"],
        ),
        ("(function() local x=x return x end)()", vec!["x"]),
        (
            "(function() local t={} for i,v in ipairs(items) do t[i]=v end return t end)()",
            vec!["ipairs", "items"],
        ),
        (
            "(function() for i=first,last do local x=i end return i end)()",
            vec!["first", "i", "last"],
        ),
        (
            "(function() repeat local x=source until x return source end)()",
            vec!["source"],
        ),
        (
            "(function() local function recurse(n) if n == 0 then return source end return recurse(n-1) end return recurse(2) end)()",
            vec!["source"],
        ),
        (
            "(function() local t={value=source} function t:get() return self.value end return t:get() end)()",
            vec!["source"],
        ),
    ] {
        let expression = compile_readonly(source).unwrap();
        assert_eq!(expression.dependencies, expected, "{source}");
    }
}

#[test]
fn compiled_expressions_can_move_between_workers() {
    let expression = compile_readonly("string.upper(labels[selected])").unwrap();
    let expected = expression.evaluate(&inputs()).unwrap();
    assert_eq!(
        std::thread::spawn(move || expression.evaluate(&inputs()).unwrap())
            .join()
            .unwrap(),
        expected
    );
}
