use bevy_luau::{EnvironmentSpec, LuauValue as V};
use std::{collections::BTreeMap, hint::black_box, time::Instant};

fn main() {
    let numeric = bevy_luau::numeric::NumericExpression::compile(
        "math.floor((base + bonus) * multiplier + 0.5)",
        &["base", "bonus", "multiplier"],
    )
    .unwrap();
    for count in [100_000, 1_000_000] {
        let start = Instant::now();
        for i in 0..count {
            black_box(
                numeric
                    .evaluate(black_box(&[100.0 + (i % 3) as f64, 5.0, 1.25]))
                    .unwrap(),
            );
        }
        println!(
            "numeric settlement {count}: {:?}, {:?}/evaluation",
            start.elapsed(),
            start.elapsed() / count
        );
    }
    let inputs = BTreeMap::from([("open".into(), V::Boolean(true))]);
    let environment = EnvironmentSpec::builder().read_only(true).build().unwrap();
    for source in [
        "open",
        "not open",
        "if open then 3 else 8",
        "100 * (if open then 2 else 1)",
    ] {
        let expression = environment.compile(source, false).unwrap();
        expression.evaluate_vm(&inputs).unwrap();
        for vm in [false, true] {
            let start = Instant::now();
            for _ in 0..100_000 {
                black_box(if vm {
                    expression.evaluate_vm(black_box(&inputs))
                } else {
                    expression.evaluate(black_box(&inputs))
                })
                .unwrap();
            }
            println!(
                "{source:30} {:6}: {:?}/evaluation",
                if vm {
                    "VM"
                } else if expression.is_native() {
                    "native"
                } else {
                    "auto VM"
                },
                start.elapsed() / 100_000
            );
        }
    }
}
