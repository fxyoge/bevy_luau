//! Compile-once portable Luau expressions, with equivalent native read plans.
//! This crate has no Bevy dependency and retains no VM handles in compiled assets.
mod native;

pub use mlua::Error;

/// Root names supplied by the pure presentation environment when not shadowed
/// by an explicit input binding.
pub fn is_presentation_builtin(name: &str) -> bool {
    matches!(
        name,
        "assert"
            | "error"
            | "ipairs"
            | "pairs"
            | "next"
            | "select"
            | "tonumber"
            | "tostring"
            | "type"
            | "typeof"
            | "math"
            | "string"
            | "table"
            | "utf8"
            | "bit32"
            | "buffer"
    )
}
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, HashMap, HashSet},
    rc::Rc,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};

use full_moon::visitors::Visitor;
use mlua::{ChunkMode, Compiler, Function, Lua, Table, Value};

const FUNCTION_CACHE_CAPACITY: usize = 256;

static NEXT_EXPRESSION_ID: AtomicU64 = AtomicU64::new(1);

/// A portable value shared by expression consumers.
///
/// Values never retain handles into a Luau VM and may move freely between
/// worker threads.
#[derive(Clone, Debug, PartialEq)]
pub enum LuauValue {
    Nil,
    Boolean(bool),
    Integer(i64),
    Number(f64),
    String(String),
    Sequence(Vec<LuauValue>),
    Record(BTreeMap<String, LuauValue>),
}

#[derive(Clone, Debug)]
pub struct CompiledLuauExpression {
    identity: u64,
    pub bytecode: Box<[u8]>,
    /// Conservative root dependencies; includes names used in nested expressions.
    pub dependencies: Vec<String>,
    native: Option<native::Plan>,
    readonly: bool,
}

impl CompiledLuauExpression {
    pub fn is_native(&self) -> bool {
        self.native.is_some()
    }

    /// Read-only expressions use an immutable input snapshot and pure builtins.
    pub fn evaluate(&self, inputs: &BTreeMap<String, LuauValue>) -> Result<LuauValue, Error> {
        let names: Vec<Box<str>> = inputs.keys().map(|name| name.as_str().into()).collect();
        let values: Vec<_> = inputs.values().cloned().collect();
        evaluate(self, &names, &values, None)
    }

    /// Useful for parity checks and measuring the native execution strategy.
    pub fn evaluate_vm(&self, inputs: &BTreeMap<String, LuauValue>) -> Result<LuauValue, Error> {
        let names: Vec<Box<str>> = inputs.keys().map(|name| name.as_str().into()).collect();
        let values: Vec<_> = inputs.values().cloned().collect();
        WORKER.with_borrow_mut(|worker| worker.evaluate(self, &names, &values, None))
    }
}

/// Compile a presentation expression. Inputs are deeply immutable; only pure
/// standard functions are exposed. No owner state or VM state can be retained.
pub fn compile_readonly(source: &str) -> Result<CompiledLuauExpression, Error> {
    let mut expression = compile_expression(source, false)?;
    expression.readonly = true;
    Ok(expression)
}

/// Bounded, shared compilation cache for authored expression strings. Compiled
/// expressions are VM-independent and can be evaluated on any worker thread.
pub fn cached_readonly(source: &str) -> Result<Arc<CompiledLuauExpression>, Error> {
    static CACHE: OnceLock<Mutex<HashMap<String, Arc<CompiledLuauExpression>>>> = OnceLock::new();
    let mut cache = CACHE.get_or_init(Default::default).lock().unwrap();
    if let Some(expression) = cache.get(source) {
        return Ok(expression.clone());
    }
    let expression = Arc::new(compile_readonly(source)?);
    if cache.len() >= 1024 {
        cache.clear();
    }
    cache.insert(source.into(), expression.clone());
    Ok(expression)
}

pub fn compile_expression(
    expression: &str,
    boolean: bool,
) -> Result<CompiledLuauExpression, Error> {
    let source = if boolean {
        format!("return not not ({expression})")
    } else {
        format!("return ({expression})")
    };
    let bytecode = Compiler::new().compile(&source)?;
    let ast = full_moon::parse(&source).map_err(|e| Error::runtime(format!("{e:?}")))?;
    let mut dependencies = native::Dependencies::default();
    dependencies.visit_ast(&ast);
    Ok(CompiledLuauExpression {
        identity: NEXT_EXPRESSION_ID.fetch_add(1, Ordering::Relaxed),
        bytecode: bytecode.into_boxed_slice(),
        dependencies: dependencies.roots.into_iter().collect(),
        native: native::compile(&ast),
        readonly: false,
    })
}

thread_local! {
    static WORKER: RefCell<LuauWorker> = RefCell::new(LuauWorker::new());
}

/// Evaluate named inputs with an optional contextual binding. The contextual
/// binding shadows an ordinary input with the same name; its root is immutable.
pub fn evaluate(
    expression: &CompiledLuauExpression,
    names: &[Box<str>],
    values: &[LuauValue],
    extra: Option<(&str, &LuauValue)>,
) -> Result<LuauValue, Error> {
    if names.len() != values.len() {
        return Err(Error::runtime(
            "expression input names and values must have equal lengths",
        ));
    }
    if let Some(plan) = &expression.native {
        // Native plans only resolve declared roots. Undeclared roots may refer
        // to sandbox globals, so those expressions stay on the VM.
        if plan.roots_declared(names, extra.map(|(name, _)| name)) {
            let value = plan.evaluate(names, values, extra)?;
            return native::normalize(value);
        }
    }
    WORKER.with_borrow_mut(|worker| worker.evaluate(expression, names, values, extra))
}

struct LuauWorker {
    lua: Lua,
    interrupts: Rc<Cell<usize>>,
    functions: HashMap<u64, CachedFunction>,
    cache_access: u64,
}

struct CachedFunction {
    function: Function,
    environment: Table,
    last_access: u64,
    names: Vec<Box<str>>,
}

impl LuauWorker {
    fn new() -> Self {
        let lua = Lua::new();
        lua.sandbox(true)
            .expect("Luau sandbox initialization failed");
        lua.set_memory_limit(64 * 1024 * 1024)
            .expect("Luau memory limit initialization failed");
        let interrupts = Rc::new(Cell::new(0));
        let callback_interrupts = Rc::clone(&interrupts);
        lua.set_interrupt(move |_| {
            let count = callback_interrupts.get() + 1;
            callback_interrupts.set(count);
            if count > 10_000 {
                return Err(Error::runtime("Luau execution limit exceeded"));
            }
            Ok(mlua::VmState::Continue)
        });
        Self {
            lua,
            interrupts,
            functions: HashMap::new(),
            cache_access: 0,
        }
    }

    fn evaluate(
        &mut self,
        expression: &CompiledLuauExpression,
        names: &[Box<str>],
        values: &[LuauValue],
        extra: Option<(&str, &LuauValue)>,
    ) -> Result<LuauValue, Error> {
        self.interrupts.set(0);
        let contextual_names;
        let cache_names = if let Some((name, _)) = extra {
            contextual_names = names
                .iter()
                .cloned()
                .chain(std::iter::once(name.into()))
                .collect::<Vec<_>>();
            &contextual_names
        } else {
            names
        };
        let (function, environment) = self.function(expression, cache_names)?;
        environment.set_readonly(false);
        environment.clear()?;

        for (name, value) in names.iter().zip(values) {
            if !expression.readonly
                || expression
                    .dependencies
                    .iter()
                    .any(|root| root == name.as_ref())
            {
                let value = to_lua(&self.lua, value)?;
                if expression.readonly {
                    freeze(&value)?;
                }
                environment.raw_set(name.as_ref(), value)?;
            }
        }
        if let Some((name, value)) = extra {
            let value = to_lua(&self.lua, value)?;
            if expression.readonly {
                freeze(&value)?;
            } else if let Value::Table(table) = &value {
                table.set_readonly(true);
            }
            environment.raw_set(name, value)?;
        }

        if expression.readonly {
            environment.set_readonly(true);
        }
        let value = function.call(())?;
        from_lua(value, &mut Conversion::new(), 0)
    }

    fn function(
        &mut self,
        expression: &CompiledLuauExpression,
        names: &[Box<str>],
    ) -> Result<(Function, Table), Error> {
        self.cache_access = self.cache_access.wrapping_add(1);
        if let Some(cached) = self.functions.get_mut(&expression.identity) {
            cached.last_access = self.cache_access;
            if cached.names == names {
                return Ok((cached.function.clone(), cached.environment.clone()));
            }
        }
        self.functions.remove(&expression.identity);

        if self.functions.len() == FUNCTION_CACHE_CAPACITY {
            let oldest = self
                .functions
                .iter()
                .min_by_key(|(_, cached)| cached.last_access)
                .map(|(identity, _)| *identity)
                .expect("a full function cache is not empty");
            self.functions.remove(&oldest);
        }
        let function = self
            .lua
            .load(expression.bytecode.as_ref())
            .set_mode(ChunkMode::Binary)
            .into_function()?;
        let environment = if expression.readonly {
            self.readonly_environment(names)?
        } else {
            self.environment(names)?
        };
        function.set_environment(environment.clone())?;
        self.functions.insert(
            expression.identity,
            CachedFunction {
                function: function.clone(),
                environment: environment.clone(),
                last_access: self.cache_access,
                names: names.to_vec(),
            },
        );
        Ok((function, environment))
    }

    fn environment(&self, names: &[Box<str>]) -> Result<Table, Error> {
        let environment = self.lua.create_table()?;
        let metatable = self.lua.create_table()?;
        let declared: HashSet<Box<str>> = names.iter().cloned().collect();
        let globals = self.lua.globals();
        metatable.set(
            "__index",
            self.lua
                .create_function(move |_, (_, key): (Table, Value)| {
                    if let Value::String(name) = &key
                        && declared.contains(name.to_str()?.as_ref())
                    {
                        return Ok(Value::Nil);
                    }
                    globals.get::<Value>(key)
                })?,
        )?;
        environment.set_metatable(Some(metatable))?;
        Ok(environment)
    }

    fn readonly_environment(&self, names: &[Box<str>]) -> Result<Table, Error> {
        let builtins = self.lua.create_table()?;
        let globals = self.lua.globals();
        for name in [
            "assert", "error", "ipairs", "pairs", "next", "select", "tonumber", "tostring", "type",
            "typeof",
        ] {
            if names.iter().any(|declared| declared.as_ref() == name) {
                continue;
            }
            builtins.set(name, globals.get::<Value>(name)?)?;
        }
        for name in ["math", "string", "table", "utf8", "bit32", "buffer"] {
            if names.iter().any(|declared| declared.as_ref() == name) {
                continue;
            }
            let original: Table = globals.get(name)?;
            let pure = self.lua.create_table()?;
            for pair in original.pairs::<String, Value>() {
                let (key, value) = pair?;
                if !matches!(key.as_str(), "random" | "randomseed") {
                    pure.set(key, value)?;
                }
            }
            pure.set_readonly(true);
            builtins.set(name, pure)?;
        }
        builtins.set_readonly(true);
        let environment = self.lua.create_table()?;
        let metatable = self.lua.create_table()?;
        metatable.set("__index", builtins)?;
        // Evaluation populates the environment via raw_set. Authored globals
        // are disallowed even inside immediately invoked helper functions.
        metatable.set(
            "__newindex",
            self.lua.create_function(|_, _: (Value, Value, Value)| {
                Err::<(), _>(Error::runtime(
                    "presentation expressions cannot assign globals",
                ))
            })?,
        )?;
        metatable.set("__metatable", false)?;
        environment.set_metatable(Some(metatable))?;
        Ok(environment)
    }
}

fn freeze(value: &Value) -> Result<(), Error> {
    if let Value::Table(table) = value {
        for pair in table.pairs::<Value, Value>() {
            freeze(&pair?.1)?;
        }
        table.set_readonly(true);
    }
    Ok(())
}

// Strings have Luau's standard string-library __index behavior. Delegate this
// uncommon read to the VM rather than approximating its metatable in Rust.
fn string_index(key: &LuauValue) -> Result<native::Evaluated, Error> {
    WORKER.with_borrow(|worker| {
        let library: Table = worker.lua.globals().get("string")?;
        let value = library.raw_get::<Value>(to_lua(&worker.lua, key)?)?;
        if matches!(value, Value::Function(_)) {
            Ok(native::Evaluated::Function)
        } else {
            from_lua(value, &mut Conversion::new(), 0).map(native::Evaluated::Portable)
        }
    })
}

fn to_lua(lua: &Lua, value: &LuauValue) -> Result<Value, Error> {
    Ok(match value {
        LuauValue::Nil => Value::Nil,
        LuauValue::Boolean(value) => Value::Boolean(*value),
        LuauValue::Integer(value) => Value::Integer(*value),
        LuauValue::Number(value) => Value::Number(*value),
        LuauValue::String(value) => Value::String(lua.create_string(value.as_bytes())?),
        LuauValue::Sequence(values) => {
            let table = lua.create_table_with_capacity(values.len(), 0)?;
            for (index, value) in values.iter().enumerate() {
                table.set(index + 1, to_lua(lua, value)?)?;
            }
            Value::Table(table)
        }
        LuauValue::Record(values) => {
            let table = lua.create_table_with_capacity(0, values.len())?;
            for (key, value) in values {
                table.set(key.as_str(), to_lua(lua, value)?)?;
            }
            Value::Table(table)
        }
    })
}

struct Conversion {
    seen_tables: HashSet<usize>,
    remaining_values: usize,
}

impl Conversion {
    fn new() -> Self {
        Self {
            seen_tables: HashSet::new(),
            remaining_values: 100_000,
        }
    }

    fn consume(&mut self) -> Result<(), Error> {
        self.remaining_values = self
            .remaining_values
            .checked_sub(1)
            .ok_or_else(|| Error::runtime("Luau value exceeds the persistence size limit"))?;
        Ok(())
    }
}

fn from_lua(value: Value, conversion: &mut Conversion, depth: usize) -> Result<LuauValue, Error> {
    conversion.consume()?;
    if depth == 64 {
        return Err(Error::runtime("Luau value nesting exceeds 64 levels"));
    }
    Ok(match value {
        Value::Nil => LuauValue::Nil,
        Value::Boolean(value) => LuauValue::Boolean(value),
        Value::Integer(value) => LuauValue::Integer(value),
        Value::Number(value) => LuauValue::Number(value),
        Value::String(value) => {
            let value = value.to_str()?;
            LuauValue::String(value.to_string())
        }
        Value::Table(table) => from_table(table, conversion, depth + 1)?,
        other => {
            return Err(Error::runtime(format!(
                "Luau value type `{}` cannot be persisted",
                other.type_name()
            )));
        }
    })
}

fn from_table(table: Table, conversion: &mut Conversion, depth: usize) -> Result<LuauValue, Error> {
    if !conversion.seen_tables.insert(table.to_pointer() as usize) {
        return Err(Error::runtime(
            "shared or cyclic Luau tables cannot be persisted",
        ));
    }
    let mut sequence = BTreeMap::new();
    let mut record = BTreeMap::new();
    for pair in table.pairs::<Value, Value>() {
        let (key, value) = pair?;
        match key {
            Value::Integer(index) if index > 0 => {
                sequence.insert(index as usize, from_lua(value, conversion, depth)?);
            }
            Value::String(key) => {
                let key = key.to_str()?;
                record.insert(key.to_string(), from_lua(value, conversion, depth)?);
            }
            _ => {
                return Err(Error::runtime(
                    "Luau tables require string or positive integer keys",
                ));
            }
        }
    }
    if !sequence.is_empty() && !record.is_empty() {
        return Err(Error::runtime(
            "mixed sequence and record Luau tables cannot be persisted",
        ));
    }
    if !record.is_empty() {
        return Ok(LuauValue::Record(record));
    }
    let expected: Vec<_> = (1..=sequence.len()).collect();
    if sequence.keys().copied().collect::<Vec<_>>() != expected {
        return Err(Error::runtime("sparse Luau sequences cannot be persisted"));
    }
    Ok(LuauValue::Sequence(sequence.into_values().collect()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persists_portable_sequences_and_records() {
        let expression =
            compile_expression("{ values = { 1, 2, 3 }, enabled = true }", false).unwrap();

        let value = evaluate(&expression, &[], &[], None).unwrap();

        assert_eq!(
            value,
            LuauValue::Record(BTreeMap::from([
                ("enabled".into(), LuauValue::Boolean(true)),
                (
                    "values".into(),
                    LuauValue::Sequence(vec![
                        LuauValue::Integer(1),
                        LuauValue::Integer(2),
                        LuauValue::Integer(3),
                    ]),
                ),
            ]))
        );
    }

    #[test]
    fn evaluates_standard_string_functions() {
        let expression = compile_expression("string.upper('ready')", false).unwrap();

        assert_eq!(
            evaluate(&expression, &[], &[], None).unwrap(),
            LuauValue::String("READY".into())
        );
    }

    #[test]
    fn evaluation_environments_do_not_share_authored_globals() {
        let mutate = compile_expression(
            "(function() accidental_global = 42 return accidental_global end)()",
            false,
        )
        .unwrap();
        let inspect = compile_expression("accidental_global", false).unwrap();

        assert_eq!(
            evaluate(&mutate, &[], &[], None).unwrap(),
            LuauValue::Integer(42)
        );
        assert_eq!(evaluate(&inspect, &[], &[], None).unwrap(), LuauValue::Nil);
    }

    #[test]
    fn cached_evaluation_environments_are_cleared() {
        let expression = compile_expression(
            "(function() local result = accidental_global or (context and context.name); accidental_global = 42 return result end)()",
            false,
        )
        .unwrap();

        let context = LuauValue::Record(BTreeMap::from([(
            "name".into(),
            LuauValue::String("tick".into()),
        )]));
        assert_eq!(
            evaluate(&expression, &[], &[], Some(("context", &context))).unwrap(),
            LuauValue::String("tick".into())
        );
        assert_eq!(
            evaluate(&expression, &[], &[], None).unwrap(),
            LuauValue::Nil
        );
    }

    #[test]
    fn declared_nil_values_shadow_sandbox_globals() {
        let expression = compile_expression("math", false).unwrap();

        assert_eq!(
            evaluate(&expression, &[Box::from("math")], &[LuauValue::Nil], None,).unwrap(),
            LuauValue::Nil
        );
    }

    #[test]
    fn interrupts_unbounded_expressions() {
        let expression =
            compile_expression("(function() while true do end return true end)()", false).unwrap();

        let error = evaluate(&expression, &[], &[], None).unwrap_err();

        assert!(error.to_string().contains("Luau execution limit exceeded"));
    }

    #[test]
    fn rejects_shared_table_graphs_without_expanding_them() {
        let expression = compile_expression(
            "(function() local shared = { value = 1 } return { a = shared, b = shared } end)()",
            false,
        )
        .unwrap();

        let error = evaluate(&expression, &[], &[], None).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("shared or cyclic Luau tables cannot be persisted")
        );
    }
}
