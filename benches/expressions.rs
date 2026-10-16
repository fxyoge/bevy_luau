use bevy_luau::{LuauValue as V, compile_readonly};
use std::{collections::BTreeMap, hint::black_box, time::Instant};

fn main() {
    let inputs = BTreeMap::from([("open".into(), V::Boolean(true))]);
    for source in ["open", "not open", "if open then 3 else 8"] {
        let expression = compile_readonly(source).unwrap();
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
                if vm { "VM" } else { "native" },
                start.elapsed() / 100_000
            );
        }
    }
}
