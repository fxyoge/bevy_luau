//! Consumer-defined, VM-independent expression namespaces.
use mlua::{Error, Function, Lua, Table, Value};
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

static NEXT_ENVIRONMENT_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
enum Export {
    Global,
    Library(Vec<String>),
    Map,
}

/// Immutable namespace and input policy. Clones share an identity, but contain no
/// VM handles. Each evaluating thread materializes its own shared builtin tables.
#[derive(Clone, Debug)]
pub struct EnvironmentSpec(Arc<Definition>);

#[cfg(feature = "serde")]
impl serde::Serialize for EnvironmentSpec {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        (&self.0.exports, self.0.read_only).serialize(serializer)
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for EnvironmentSpec {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use std::sync::{Mutex, OnceLock, Weak};
        type Key = (BTreeMap<String, Export>, bool);
        static RESTORED: OnceLock<Mutex<BTreeMap<Key, Weak<Definition>>>> = OnceLock::new();
        let (exports, read_only) = <(BTreeMap<String, Export>, bool)>::deserialize(deserializer)?;
        let key = (exports.clone(), read_only);
        let mut restored = RESTORED.get_or_init(Default::default).lock().unwrap();
        if let Some(definition) = restored.get(&key).and_then(Weak::upgrade) {
            return Ok(Self(definition));
        }
        let definition = Arc::new(Definition {
            identity: NEXT_ENVIRONMENT_ID.fetch_add(1, Ordering::Relaxed),
            exports,
            read_only,
        });
        restored.retain(|_, value| value.strong_count() > 0);
        restored.insert(key, Arc::downgrade(&definition));
        Ok(Self(definition))
    }
}

#[derive(Debug)]
struct Definition {
    identity: u64,
    exports: BTreeMap<String, Export>,
    read_only: bool,
}

/// Builds an explicit namespace. No standard globals are exposed implicitly.
#[derive(Default)]
pub struct EnvironmentBuilder {
    exports: BTreeMap<String, Export>,
    read_only: bool,
    duplicate: Option<String>,
}

impl EnvironmentSpec {
    pub fn builder() -> EnvironmentBuilder {
        EnvironmentBuilder::default()
    }

    pub fn is_builtin(&self, name: &str) -> bool {
        self.0.exports.contains_key(name)
    }

    /// Root inputs cannot hide exposed builtins, including when their value is nil.
    /// Nested record fields and lexical locals are unaffected.
    pub fn validate_input_name(&self, name: &str) -> Result<(), Error> {
        if self.is_builtin(name) {
            return Err(Error::runtime(format!(
                "input name `{name}` is reserved for a builtin"
            )));
        }
        Ok(())
    }

    pub(crate) fn identity(&self) -> u64 {
        self.0.identity
    }
    pub(crate) fn read_only(&self) -> bool {
        self.0.read_only
    }

    pub(crate) fn materialize(&self, lua: &Lua) -> Result<Table, Error> {
        let builtins = lua.create_table()?;
        let globals = lua.globals();
        for (name, export) in &self.0.exports {
            if matches!(export, Export::Map) {
                builtins.raw_set(name.as_str(), map_function(lua)?)?;
                continue;
            }
            let value: Value = globals.get(name.as_str())?;
            if value.is_nil() {
                return Err(Error::runtime(format!("unknown builtin `{name}`")));
            }
            match export {
                Export::Global => {
                    builtins.raw_set(name.as_str(), value)?;
                }
                Export::Library(excluded) => {
                    let Value::Table(original) = value else {
                        return Err(Error::runtime(format!(
                            "builtin `{name}` is not a library table"
                        )));
                    };
                    let library = lua.create_table()?;
                    for pair in original.pairs::<String, Value>() {
                        let (key, value) = pair?;
                        if !excluded.contains(&key) {
                            library.raw_set(key, value)?;
                        }
                    }
                    library.set_readonly(true);
                    builtins.raw_set(name.as_str(), library)?;
                }
                Export::Map => unreachable!(),
            }
        }
        builtins.set_readonly(true);
        let metatable = lua.create_table()?;
        metatable.raw_set("__index", builtins)?;
        if self.read_only() {
            metatable.raw_set(
                "__newindex",
                lua.create_function(|_, _: (Value, Value, Value)| {
                    Err::<(), _>(Error::runtime(
                        "expression environment cannot assign globals",
                    ))
                })?,
            )?;
        }
        metatable.raw_set("__metatable", false)?;
        metatable.set_readonly(true);
        Ok(metatable)
    }
}

impl EnvironmentBuilder {
    /// Expose `map(sequence, callback)`. Each element is transformed in order;
    /// nil results are omitted, while false and zero are retained. The callback
    /// receives the element, and the result is always a dense sequence.
    pub fn map(mut self) -> Self {
        self.insert("map".into(), Export::Map);
        self
    }

    fn insert(&mut self, name: String, export: Export) {
        if self.exports.insert(name.clone(), export).is_some() {
            self.duplicate = Some(name);
        }
    }

    /// Expose named VM globals. Library tables should use `library` instead when
    /// individual exports need to be omitted.
    pub fn globals(mut self, names: impl IntoIterator<Item = impl Into<String>>) -> Self {
        for name in names {
            self.insert(name.into(), Export::Global);
        }
        self
    }

    /// Copy a standard library once per VM/specification, omitting named entries.
    pub fn library(mut self, name: impl Into<String>, excluded: &[&str]) -> Self {
        self.insert(
            name.into(),
            Export::Library(excluded.iter().map(|s| (*s).into()).collect()),
        );
        self
    }

    /// Freeze input records/sequences and reject authored global assignments.
    pub fn read_only(mut self, value: bool) -> Self {
        self.read_only = value;
        self
    }

    pub fn build(self) -> Result<EnvironmentSpec, Error> {
        if let Some(name) = self.duplicate {
            return Err(Error::runtime(format!("duplicate builtin `{name}`")));
        }
        Ok(EnvironmentSpec(Arc::new(Definition {
            identity: NEXT_ENVIRONMENT_ID.fetch_add(1, Ordering::Relaxed),
            exports: self.exports,
            read_only: self.read_only,
        })))
    }
}

fn map_function(lua: &Lua) -> Result<Function, Error> {
    // Keep traversal in Luau so it shares the expression's instruction budget,
    // including callbacks that return native reads or constants.
    lua.load(
        "return function(values, transform)
            if type(values) ~= 'table' then error('map expects a dense sequence') end
            if type(transform) ~= 'function' then error('map expects a callback') end
            local length = #values
            local entries = 0
            for key in values do
                if type(key) ~= 'number' or key % 1 ~= 0 or key < 1 or key > length then
                    error('map expects a dense sequence')
                end
                entries += 1
            end
            if entries ~= length then error('map expects a dense sequence') end
            local result = {}
            for i = 1, length do
                local value = transform(values[i])
                if value ~= nil then result[#result + 1] = value end
            end
            return result
        end",
    )
    .eval()
}
