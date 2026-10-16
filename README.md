# bevy_luau

Portable, compile-once Luau expressions shared by `bevy_state_tree` and
`bevy_kui`. No Bevy dependency; compiled expressions and portable values contain
no VM handles and can move between worker threads.

```rust
use std::collections::BTreeMap;
use bevy_luau::{compile_readonly, LuauValue};

let expression = compile_readonly(
    "if installed then {x=0, y=0, z=3} else {x=-28, y=205, z=8}"
)?;
let value = expression.evaluate(&BTreeMap::from([
    ("installed".into(), LuauValue::Boolean(true)),
]))?;
# Ok::<(), bevy_luau::Error>(())
```

## Execution contract

The language is Luau, including one-based sequence indexing and Luau truthiness.
The compiler selects native plans for simple literals, reads/indexing, `not`,
and supported `if` expressions. Other expressions use compiled Luau bytecode.
Native plans preserve Luau's double-based numeric and portable-table semantics.
They do not retry evaluation after an error. `evaluate_vm` forces VM execution
for parity checks; `is_native` describes the compiled plan. Undeclared roots
that might address builtins are evaluated by the VM.

Expressions expose conservative root `dependencies`, including computed keys
and reads inside functions. Lexical locals, function parameters, loop bindings,
and repeat-until scopes are excluded. The bounded shared
`cached_readonly` cache avoids recompiling authored expressions across mounts.

## Evaluation policies

`compile_readonly` creates presentation expressions. Input records/sequences and
the evaluation environment are deeply read-only. Only pure standard libraries
and basic functions are exposed; random functions, environment access, module
loading, and application effects are unavailable. Expressions may construct
and mutate local scratch tables and call immediately invoked local functions.
No state survives an evaluation. Only portable results are returned.

`compile_expression` / `evaluate` provide the datamodel policy, including
evaluation-local mutation used to implement ordered nested assignments. A
generic optional contextual binding can be supplied without copying all input
values. The state tree owns persistence, event bindings/projection, assignment
policy, and authored source locations; none are built into this engine. Both policies share VM workers,
bytecode/function caching, execution limits, and native plans.

VM workers are thread-local. Each uses a 64 MiB memory limit, interrupt budget,
and a bounded function cache. Portable results are limited to 64 nesting levels
and 100,000 values. Functions, userdata, sparse/mixed tables, and shared/cyclic
table graphs cannot be returned. Empty Lua tables become empty sequences.

## Checks

```sh
cargo test
cargo bench --bench expressions
```
