# bevy_luau

Portable, compile-once Luau expressions with consumer-defined environments.
No Bevy dependency; compiled expressions, environment specifications, and portable
values contain no VM handles and can move between worker threads.

```rust
use std::collections::BTreeMap;
use bevy_luau::{EnvironmentSpec, LuauValue};

let environment = EnvironmentSpec::builder()
    .globals(["type", "tostring"])
    .library("math", &["random", "randomseed"])
    .read_only(true)
    .build()?;
let expression = environment.compile(
    "if installed then {x=0, y=0, z=3} else {x=-28, y=205, z=8}", false
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
`EnvironmentSpec::cached_expression` cache avoids recompiling expressions across
users of the same specification. Its key includes specification identity and
boolean-result mode; distinct namespaces never share compiled cache entries.

## Environment ownership and sharing

Consumers define and retain an immutable `EnvironmentSpec`, selecting exposed VM
globals and library exports. There is no implicit fallback to all VM globals and
no process-wide registration order. Cloning a specification retains its identity;
building another creates an independent namespace. Consumer libraries should own
their defaults so applications need no registration or setup calls.

Root input names matching exposed builtins are rejected, even when their value is
nil, before both native and VM evaluation. `validate_input_name` supports early
authoring diagnostics. Nested fields such as `printer.type` and lexical locals
remain unrestricted. The engine does not reserve names absent from a specification.

`read_only(true)` deeply freezes input records/sequences and rejects global
assignments; local scratch tables may still be mutated. The default mutable mode
supports evaluation-local assignments. Environments are cleared before each call;
persistence and contextual bindings belong to consumers. `evaluate` accepts an
optional contextual input, which may replace an ordinary input of the same name
but cannot replace a builtin.

Within each worker VM, a specification's library copies, builtin namespace, and
protected fallback metatable are constructed once and shared by its cached
functions. Each function retains its own input table. These shared objects are
independent of the 256-entry function cache, so eviction does not recopy libraries.
Independent VMs never share Lua objects. `prepare_current_thread` optionally
materializes a namespace without executing expressions; normal evaluation does
this automatically. Preparing one thread does not prepare other workers.

VM workers are thread-local. Each uses a 64 MiB memory limit, interrupt budget,
and a bounded function cache. Portable results are limited to 64 nesting levels
and 100,000 values. Functions, userdata, sparse/mixed tables, and shared/cyclic
table graphs cannot be returned. Empty Lua tables become empty sequences.

## Checks

```sh
cargo test
cargo bench --bench expressions
```
