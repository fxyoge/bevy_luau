//! Compile-once portable Luau expressions, with equivalent native read plans.
//! This crate has no Bevy dependency and retains no VM handles in compiled assets.
mod environment;
mod native;
pub mod numeric;

pub use environment::{EnvironmentBuilder, EnvironmentSpec};
pub use mlua::Error;
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
    environment: EnvironmentSpec,
}

impl CompiledLuauExpression {
    pub fn is_native(&self) -> bool {
        self.native.is_some()
    }

    /// Evaluate with this expression's configured namespace and input policy.
    pub fn evaluate(&self, inputs: &BTreeMap<String, LuauValue>) -> Result<LuauValue, Error> {
        let names: Vec<Box<str>> = inputs.keys().map(|name| name.as_str().into()).collect();
        let values: Vec<_> = inputs.values().cloned().collect();
        evaluate(self, &names, &values, None)
    }

    /// Useful for parity checks and measuring the native execution strategy.
    pub fn evaluate_vm(&self, inputs: &BTreeMap<String, LuauValue>) -> Result<LuauValue, Error> {
        let names: Vec<Box<str>> = inputs.keys().map(|name| name.as_str().into()).collect();
        let values: Vec<_> = inputs.values().cloned().collect();
        validate_inputs(self, &names, &values, None)?;
        WORKER.with_borrow_mut(|worker| worker.evaluate(self, &names, &values, None))
    }
}

impl EnvironmentSpec {
    /// Bounded shared compilation cache, isolated by environment identity and boolean mode.
    pub fn cached_expression(
        &self,
        source: &str,
        boolean: bool,
    ) -> Result<Arc<CompiledLuauExpression>, Error> {
        type Key = (u64, String, bool);
        static CACHE: OnceLock<Mutex<HashMap<Key, Arc<CompiledLuauExpression>>>> = OnceLock::new();
        let mut cache = CACHE.get_or_init(Default::default).lock().unwrap();
        let key = (self.identity(), source.to_owned(), boolean);
        if let Some(expression) = cache.get(&key) {
            return Ok(expression.clone());
        }
        let expression = Arc::new(self.compile(source, boolean)?);
        if cache.len() >= 1024 {
            cache.clear();
        }
        cache.insert(key, expression.clone());
        Ok(expression)
    }

    /// Compile bytecode and an optional native plan, retaining this namespace.
    pub fn compile(
        &self,
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
            environment: self.clone(),
        })
    }
    /// Prepare this specification on the calling thread without executing expressions.
    pub fn prepare_current_thread(&self) -> Result<(), Error> {
        WORKER.with_borrow_mut(|worker| worker.prepare(self).map(|_| ()))
    }
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
    validate_inputs(expression, names, values, extra)?;
    if let Some(plan) = &expression.native {
        // Undeclared roots may address builtins and must use the VM.
        if plan.roots_declared(names, extra.map(|(name, _)| name)) {
            let value = plan.evaluate(names, values, extra)?;
            return native::normalize(value);
        }
    }
    WORKER.with_borrow_mut(|worker| worker.evaluate(expression, names, values, extra))
}

fn validate_inputs(
    expression: &CompiledLuauExpression,
    names: &[Box<str>],
    values: &[LuauValue],
    extra: Option<(&str, &LuauValue)>,
) -> Result<(), Error> {
    if names.len() != values.len() {
        return Err(Error::runtime(
            "expression input names and values must have equal lengths",
        ));
    }
    for name in names {
        expression.environment.validate_input_name(name)?;
    }
    if let Some((name, _)) = extra {
        expression.environment.validate_input_name(name)?;
    }
    Ok(())
}

struct LuauWorker {
    lua: Lua,
    interrupts: Rc<Cell<usize>>,
    functions: HashMap<u64, CachedFunction>,
    environments: HashMap<u64, Table>,
    cache_access: u64,
}

struct CachedFunction {
    function: Function,
    environment: Table,
    last_access: u64,
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
            environments: HashMap::new(),
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
        let (function, environment) = self.function(expression)?;
        environment.set_readonly(false);
        environment.clear()?;

        for (name, value) in names.iter().zip(values) {
            if !expression.environment.read_only()
                || expression
                    .dependencies
                    .iter()
                    .any(|root| root == name.as_ref())
            {
                let value = to_lua(&self.lua, value)?;
                if expression.environment.read_only() {
                    freeze(&value)?;
                }
                environment.raw_set(name.as_ref(), value)?;
            }
        }
        if let Some((name, value)) = extra {
            let value = to_lua(&self.lua, value)?;
            if expression.environment.read_only() {
                freeze(&value)?;
            } else if let Value::Table(table) = &value {
                table.set_readonly(true);
            }
            environment.raw_set(name, value)?;
        }

        if expression.environment.read_only() {
            environment.set_readonly(true);
        }
        let value = function.call(())?;
        from_lua(value, &mut Conversion::new(), 0)
    }

    fn function(
        &mut self,
        expression: &CompiledLuauExpression,
    ) -> Result<(Function, Table), Error> {
        self.cache_access = self.cache_access.wrapping_add(1);
        if let Some(cached) = self.functions.get_mut(&expression.identity) {
            cached.last_access = self.cache_access;
            return Ok((cached.function.clone(), cached.environment.clone()));
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
        let metatable = self.prepare(&expression.environment)?;
        let environment = self.lua.create_table()?;
        environment.set_metatable(Some(metatable))?;
        function.set_environment(environment.clone())?;
        self.functions.insert(
            expression.identity,
            CachedFunction {
                function: function.clone(),
                environment: environment.clone(),
                last_access: self.cache_access,
            },
        );
        Ok((function, environment))
    }

    fn prepare(&mut self, spec: &EnvironmentSpec) -> Result<Table, Error> {
        if let Some(metatable) = self.environments.get(&spec.identity()) {
            return Ok(metatable.clone());
        }
        let metatable = spec.materialize(&self.lua)?;
        self.environments.insert(spec.identity(), metatable.clone());
        Ok(metatable)
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
    fn shared_namespace_survives_function_eviction_without_sharing_input_tables() {
        let spec = EnvironmentSpec::builder()
            .library("math", &["random", "randomseed"])
            .read_only(true)
            .build()
            .unwrap();
        let mut worker = LuauWorker::new();
        let shared = worker.prepare(&spec).unwrap();
        let first = spec.compile("math.floor(value)", false).unwrap();
        let names = [Box::from("value")];
        assert_eq!(
            worker
                .evaluate(&first, &names, &[LuauValue::Number(1.5)], None)
                .unwrap(),
            LuauValue::Integer(1)
        );
        let first_environment = worker.functions[&first.identity].environment.clone();
        for index in 0..FUNCTION_CACHE_CAPACITY {
            let other = spec
                .compile(&format!("math.floor({index} + 0.5)"), false)
                .unwrap();
            worker.evaluate(&other, &[], &[], None).unwrap();
            let environment = &worker.functions[&other.identity].environment;
            assert_ne!(environment.to_pointer(), first_environment.to_pointer());
            assert_eq!(
                environment.metatable().unwrap().to_pointer(),
                shared.to_pointer()
            );
        }
        assert!(!worker.functions.contains_key(&first.identity));
        assert_eq!(
            worker
                .evaluate(&first, &names, &[LuauValue::Number(2.5)], None)
                .unwrap(),
            LuauValue::Integer(2)
        );
        assert_eq!(worker.functions.len(), FUNCTION_CACHE_CAPACITY);
        assert_eq!(worker.environments.len(), 1);
        assert_eq!(
            worker.prepare(&spec).unwrap().to_pointer(),
            shared.to_pointer()
        );
    }

    #[test]
    fn specifications_isolate_compilation_and_vm_namespaces() {
        let allowed = EnvironmentSpec::builder()
            .library("math", &[])
            .build()
            .unwrap();
        let omitted = EnvironmentSpec::builder()
            .library("math", &["floor"])
            .build()
            .unwrap();
        let a = allowed.cached_expression("math.floor(1.5)", false).unwrap();
        let b = omitted.cached_expression("math.floor(1.5)", false).unwrap();
        assert_ne!(a.identity, b.identity);
        assert_eq!(
            a.identity,
            allowed
                .clone()
                .cached_expression("math.floor(1.5)", false)
                .unwrap()
                .identity
        );
        assert_eq!(a.evaluate(&BTreeMap::new()).unwrap(), LuauValue::Integer(1));
        assert!(b.evaluate(&BTreeMap::new()).is_err());
        assert!(
            allowed.cached_expression("true", true).unwrap().identity
                != allowed.cached_expression("true", false).unwrap().identity
        );
        let empty = EnvironmentSpec::builder().build().unwrap();
        assert_eq!(
            empty
                .compile("math", false)
                .unwrap()
                .evaluate(&BTreeMap::new())
                .unwrap(),
            LuauValue::Nil
        );
    }

    fn compile_expression(source: &str, boolean: bool) -> Result<CompiledLuauExpression, Error> {
        static SPEC: OnceLock<EnvironmentSpec> = OnceLock::new();
        SPEC.get_or_init(|| {
            EnvironmentSpec::builder()
                .globals(["string", "math"])
                .build()
                .unwrap()
        })
        .compile(source, boolean)
    }

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
    fn contextual_inputs_cannot_shadow_builtins() {
        let expression = compile_expression("math", false).unwrap();

        assert!(evaluate(&expression, &[], &[], Some(("math", &LuauValue::Nil))).is_err());
        assert!(evaluate(&expression, &[Box::from("math")], &[LuauValue::Nil], None).is_err());
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
