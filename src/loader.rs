use std::{
    collections::{HashMap, HashSet},
    ops::Range,
    sync::{Arc, OnceLock},
};

use wasmtime::wasmparser::SymbolFlags;
use wasmtime::{
    AsContextMut, Engine, Extern, ExternType, Func, Global, GlobalType, Instance, Linker, Module,
    Mutability, Ref, StoreContextMut, Table, Val, ValType,
};
use wasmtime::{Result, bail, ensure, error::Context, format_err as anyhow};

use crate::{LinearMemory, Metadata};

/// Regions reserved exclusively for the loader. They must not overlap the guest
/// heap, stack, static data, or function-table allocations made by other code.
/// Address/table index zero is reserved for null pointers.
#[derive(Clone, Debug)]
pub struct Arena {
    pub memory: Range<u32>,
    pub table: Range<u32>,
}

/// A relocated symbol. Data values are byte offsets, function values are Wasmtime
/// handles. Use [`Loader::function_pointer`] to obtain a C function pointer.
#[derive(Clone, Copy, Debug)]
pub enum Symbol {
    Function(Func),
    Data(u32),
}

/// A library retained for the lifetime of its store. Reopening a name returns
/// this same instance; it does not reset memory or run constructors again.
#[derive(Clone, Debug)]
pub struct Library {
    pub instance: Instance,
    pub memory_base: u32,
    pub table_base: u32,
    symbols: HashMap<String, Symbol>,
}

impl Library {
    /// Look up this library's own exported symbol, including hidden exports.
    pub fn symbol(&self, name: &str) -> Option<Symbol> {
        self.symbols.get(name).copied()
    }
}

struct Definition {
    module: Module,
    metadata: Metadata,
}

#[derive(Clone, Copy)]
struct Binding {
    symbol: Symbol,
    weak: bool,
}

struct PendingFunction {
    owner: String,
    name: String,
    weak: bool,
    target: Arc<OnceLock<Func>>,
    expected: wasmtime::FuncType,
}

struct PendingAddress {
    owner: String,
    name: String,
    weak: bool,
    function: bool,
    global: Global,
}

/// A store-local loader for wasm32 PIC libraries.
///
/// All operations must use the store owning the supplied memory/table, as with
/// Wasmtime's own handles. Libraries share a trust domain: this is not isolation
/// between plugins. Supply a bounded memory/table and Wasmtime resource/fuel
/// limits when loading untrusted binaries.
///
/// A failure after instantiation begins poisons the loader: guest initializers
/// may already have modified shared state. Drop the store instead of retrying
/// in a partly initialized address space. Registration/preflight errors do not
/// poison it. Individual unloading is intentionally not exposed because Wasmtime
/// retains instances until their store is dropped.
pub struct Loader {
    engine: Engine,
    memory: LinearMemory,
    table: Table,
    arena: Arena,
    next_memory: u32,
    next_table: u32,
    definitions: HashMap<String, Definition>,
    loaded: HashMap<String, Library>,
    symbols: HashMap<String, Binding>,
    poisoned: bool,
}

impl Loader {
    /// Construct a loader over existing private or shared wasm32
    /// memory and a wasm32 `funcref` table. The arena is not allocated until load.
    pub fn new(
        mut store: impl AsContextMut,
        memory: impl Into<LinearMemory>,
        table: Table,
        arena: Arena,
    ) -> Result<Self> {
        let store = store.as_context_mut();
        let memory = memory.into();
        let memory_type = memory.ty(&store);
        let table_type = table.ty(&store);
        ensure!(!memory_type.is_64(), "only wasm32 memory is supported");
        ensure!(
            !table_type.is_64()
                && wasmtime::RefType::eq(table_type.element(), &wasmtime::RefType::FUNCREF),
            "only wasm32 funcref tables are supported"
        );
        ensure!(
            arena.memory.start > 0 && arena.memory.start <= arena.memory.end,
            "invalid memory arena"
        );
        ensure!(
            arena.table.start > 0 && arena.table.start <= arena.table.end,
            "invalid table arena"
        );
        Ok(Self {
            engine: store.engine().clone(),
            memory,
            table,
            next_memory: arena.memory.start,
            next_table: arena.table.start,
            arena,
            definitions: HashMap::new(),
            loaded: HashMap::new(),
            symbols: HashMap::new(),
            poisoned: false,
        })
    }

    /// Parse and compile a library. Dependency names are resolved solely against
    /// this explicit registry; loading never follows paths supplied by a module.
    pub fn register(&mut self, name: impl Into<String>, bytes: &[u8]) -> Result<()> {
        ensure!(!self.poisoned, "loader is poisoned");
        let name = name.into();
        ensure!(
            !name.is_empty() && !self.definitions.contains_key(&name),
            "empty or duplicate library name: {name}"
        );
        let metadata = Metadata::parse(bytes).with_context(|| format!("metadata for {name}"))?;
        if let Some(shared) = metadata.shared_memory {
            ensure!(
                shared == matches!(self.memory, LinearMemory::Shared(_)),
                "env.memory sharedness mismatch for {name}"
            );
        }
        let module = Module::from_binary(&self.engine, bytes)
            .with_context(|| format!("compiling {name}"))?;
        self.definitions
            .insert(name, Definition { module, metadata });
        Ok(())
    }

    /// Expose an already relocated main-module or host symbol. Host system
    /// functions can alternatively be supplied through the `Linker` at load.
    /// Define symbols before loading any libraries, so bindings stay stable.
    pub fn define(&mut self, name: impl Into<String>, symbol: Symbol) -> Result<()> {
        ensure!(
            !self.poisoned && self.loaded.is_empty(),
            "define symbols before loading"
        );
        let name = name.into();
        ensure!(
            !self.symbols.contains_key(&name),
            "duplicate host symbol: {name}"
        );
        self.symbols.insert(
            name,
            Binding {
                symbol,
                weak: false,
            },
        );
        Ok(())
    }

    /// Inspect the metadata of a registered library.
    pub fn metadata(&self, name: &str) -> Option<&Metadata> {
        self.definitions.get(name).map(|d| &d.metadata)
    }

    /// Look up the process-wide symbol scope.
    pub fn symbol(&self, name: &str) -> Option<Symbol> {
        self.symbols.get(name).map(|b| b.symbol)
    }

    /// Whether an incomplete load requires discarding this loader and its store.
    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    /// Load a library and its dependency closure. All modules are instantiated,
    /// all symbols bound, and all data relocations applied before constructors
    /// run. Dependency constructors run before their consumers; cycles use a
    /// deterministic depth-first order. Constructors run once per library.
    ///
    /// `host` supplies imports such as `env.__stack_pointer` and system calls.
    /// Dynamic memory/table/base imports are always supplied by the loader.
    pub fn load<T: 'static>(
        &mut self,
        mut store: impl AsContextMut<Data = T>,
        host: &Linker<T>,
        name: &str,
    ) -> Result<Library> {
        ensure!(!self.poisoned, "loader is poisoned; discard its store");
        if let Some(library) = self.loaded.get(name) {
            return Ok(library.clone());
        }
        let mut order = Vec::new();
        self.visit(name, &mut HashSet::new(), &mut order, false)?;
        let mut store = store.as_context_mut();
        let result = self.load_group(&mut store, host, &order);
        if result.is_err() {
            self.poisoned = true;
        }
        result?;
        Ok(self.loaded[name].clone())
    }

    /// Load with a local symbol scope. Dependencies can see each other, but
    /// symbols introduced by this group are not added to the global scope.
    /// Previously loaded dependencies remain available to this load group.
    pub fn load_local<T: 'static>(
        &mut self,
        mut store: impl AsContextMut<Data = T>,
        host: &Linker<T>,
        name: &str,
    ) -> Result<Library> {
        ensure!(!self.poisoned, "loader is poisoned; discard its store");
        let saved = self.symbols.clone();
        self.expose_dependencies(name)?;
        let result = self.load(&mut store, host, name);
        self.symbols = saved;
        result
    }

    /// Publish the exports of an already loaded library and its dependencies.
    /// Existing global bindings keep their addresses.
    pub fn promote(&mut self, name: &str) -> Result<()> {
        ensure!(!self.poisoned, "loader is poisoned");
        ensure!(
            self.loaded.contains_key(name),
            "library is not loaded: {name}"
        );
        self.expose_dependencies(name)?;
        Ok(())
    }

    /// Whether this library has already been instantiated successfully.
    pub fn is_loaded(&self, name: &str) -> bool {
        !self.poisoned && self.loaded.contains_key(name)
    }

    /// Look up a library's own exports, then its dependencies in breadth-first order.
    pub fn symbol_in(&self, library: &str, name: &str) -> Option<Symbol> {
        self.dependency_names(library)
            .iter()
            .find_map(|library| self.loaded.get(library)?.symbol(name))
    }

    #[cfg(feature = "emscripten")]
    pub(crate) fn memory(&self) -> LinearMemory {
        self.memory.clone()
    }

    fn dependency_names(&self, name: &str) -> Vec<String> {
        let mut queue = std::collections::VecDeque::from([name.to_owned()]);
        let mut seen = HashSet::new();
        let mut names = Vec::new();
        while let Some(name) = queue.pop_front() {
            if !seen.insert(name.clone()) {
                continue;
            }
            if let Some(definition) = self.definitions.get(&name) {
                queue.extend(definition.metadata.needed.iter().cloned());
            }
            names.push(name);
        }
        names
    }

    fn expose_dependencies(&mut self, name: &str) -> Result<()> {
        let mut order = Vec::new();
        self.visit(name, &mut HashSet::new(), &mut order, true)?;
        let existing: HashSet<_> = self.symbols.keys().cloned().collect();
        for name in order {
            let Some(library) = self.loaded.get(&name) else {
                continue;
            };
            for (symbol, value) in &library.symbols {
                let flags = self.definitions[&name].metadata.export_flags(symbol);
                if symbol.starts_with("__wasm_")
                    || symbol == "__dso_handle"
                    || symbol == "__tls_base"
                    || flags.contains(SymbolFlags::VISIBILITY_HIDDEN)
                    || existing.contains(symbol)
                {
                    continue;
                }
                let weak = flags.contains(SymbolFlags::BINDING_WEAK);
                if self
                    .symbols
                    .get(symbol)
                    .is_none_or(|binding| binding.weak && !weak)
                {
                    self.symbols.insert(
                        symbol.clone(),
                        Binding {
                            symbol: *value,
                            weak,
                        },
                    );
                }
            }
        }
        Ok(())
    }

    fn visit(
        &self,
        name: &str,
        visited: &mut HashSet<String>,
        order: &mut Vec<String>,
        include_loaded: bool,
    ) -> Result<()> {
        let mut stack = vec![(name, false)];
        while let Some((name, finish)) = stack.pop() {
            if finish {
                order.push(name.to_owned());
                continue;
            }
            if (!include_loaded && self.loaded.contains_key(name))
                || !visited.insert(name.to_owned())
            {
                continue;
            }
            let definition = self
                .definitions
                .get(name)
                .with_context(|| format!("unregistered dependency: {name}"))?;
            stack.push((name, true));
            for dependency in definition.metadata.needed.iter().rev() {
                stack.push((dependency, false));
            }
        }
        Ok(())
    }

    fn load_group<T: 'static>(
        &mut self,
        store: &mut StoreContextMut<'_, T>,
        host: &Linker<T>,
        order: &[String],
    ) -> Result<()> {
        let mut pending_functions = Vec::new();
        let mut pending_addresses = Vec::new();
        // Existing bindings remain stable. Within this batch the first strong
        // definition wins, or the first weak definition if there is no strong one.
        let existing: HashSet<_> = self.symbols.keys().cloned().collect();
        for name in order {
            let definition = &self.definitions[name];
            let metadata = definition.metadata.clone();
            let module = definition.module.clone();
            let memory_base = reserve(
                &mut self.next_memory,
                metadata.layout.memory_size,
                metadata.layout.memory_alignment,
                self.arena.memory.end,
            )?;
            let table_base = reserve(
                &mut self.next_table,
                metadata.layout.table_size,
                metadata.layout.table_alignment,
                self.arena.table.end,
            )?;
            self.prepare_regions(
                store,
                memory_base,
                metadata.layout.memory_size,
                table_base,
                metadata.layout.table_size,
            )?;
            let mut imports = Vec::new();
            for import in module.imports() {
                let namespace = import.module();
                let field = import.name();
                let value = match (namespace, field) {
                    ("env", "memory") => self.memory.import(),
                    ("env", "__indirect_function_table") => Extern::Table(self.table),
                    ("env", "__memory_base") => base_global(store, memory_base)?.into(),
                    ("env", "__table_base") => base_global(store, table_base)?.into(),
                    ("GOT.mem" | "GOT.func", _) => {
                        ensure!(
                            matches!(import.ty(), ExternType::Global(ref ty) if ValType::eq(ty.content(), &ValType::I32) && ty.mutability() == Mutability::Var),
                            "{name}: {namespace}.{field} must be mutable i32"
                        );
                        let global = Global::new(
                            &mut *store,
                            GlobalType::new(ValType::I32, Mutability::Var),
                            Val::I32(0),
                        )?;
                        pending_addresses.push(PendingAddress {
                            owner: name.clone(),
                            name: field.to_owned(),
                            weak: metadata.weak_import(namespace, field),
                            function: namespace == "GOT.func",
                            global,
                        });
                        global.into()
                    }
                    _ => {
                        if let Some(value) = if metadata
                            .export_flags(field)
                            .contains(SymbolFlags::VISIBILITY_HIDDEN)
                        {
                            None
                        } else {
                            host_import(store, host, namespace, field)?
                        } {
                            value
                        } else if namespace == "env" {
                            if let Some(Binding {
                                symbol: Symbol::Function(function),
                                weak: false,
                            }) = self.symbols.get(field)
                                && !metadata
                                    .export_flags(field)
                                    .contains(SymbolFlags::VISIBILITY_HIDDEN)
                            {
                                imports.push((*function).into());
                                continue;
                            }
                            let ExternType::Func(expected) = import.ty() else {
                                bail!("{name}: unresolved import {namespace}.{field}");
                            };
                            let target = Arc::new(OnceLock::<Func>::new());
                            let target_for_call = target.clone();
                            let label = format!("{name}: unresolved function {field}");
                            let stub = Func::new(
                                &mut *store,
                                expected.clone(),
                                move |mut caller, params, results| {
                                    let function =
                                        target_for_call.get().ok_or_else(|| anyhow!("{label}"))?;
                                    function.call(&mut caller, params, results)
                                },
                            );
                            pending_functions.push(PendingFunction {
                                owner: name.clone(),
                                name: field.to_owned(),
                                weak: metadata.weak_import(namespace, field),
                                target,
                                expected,
                            });
                            stub.into()
                        } else {
                            bail!("{name}: unresolved import {namespace}.{field}");
                        }
                    }
                };
                imports.push(value);
            }
            let instance = Instance::new(&mut *store, &module, &imports)
                .with_context(|| format!("instantiating {name}"))?;
            let exports: Vec<_> = instance
                .exports(&mut *store)
                .map(|export| (export.name().to_owned(), export.into_extern()))
                .collect();
            let mut library = Library {
                instance,
                memory_base,
                table_base,
                symbols: HashMap::new(),
            };
            for (field, value) in exports {
                if field.starts_with("__wasm_") || field == "__tls_base" {
                    continue;
                }
                if let Some((namespace, imported)) = metadata.function_reexports.get(&field) {
                    ensure!(
                        namespace != "env"
                            || !pending_functions
                                .iter()
                                .any(|pending| pending.owner == *name && pending.name == *imported),
                        "{name}: re-export of unresolved function {field} is unsupported"
                    );
                }
                let symbol = match value {
                    Extern::Func(function) => Symbol::Function(function),
                    Extern::Global(global)
                        if ValType::eq(global.ty(&*store).content(), &ValType::I32)
                            && global.ty(&*store).mutability() == Mutability::Const =>
                    {
                        let offset = global.get(&mut *store).i32().unwrap() as u32;
                        Symbol::Data(
                            memory_base
                                .checked_add(offset)
                                .context("data export address overflow")?,
                        )
                    }
                    _ => continue,
                };
                library.symbols.insert(field.clone(), symbol);
                // Per-library runtime helpers must never interpose on other DSOs.
                if field.starts_with("__wasm_") || field == "__dso_handle" || field == "__tls_base"
                {
                    continue;
                }
                let flags = metadata.export_flags(&field);
                if flags.contains(SymbolFlags::VISIBILITY_HIDDEN) || existing.contains(&field) {
                    continue;
                }
                let weak = flags.contains(SymbolFlags::BINDING_WEAK);
                if self
                    .symbols
                    .get(&field)
                    .is_none_or(|binding| binding.weak && !weak)
                {
                    self.symbols.insert(field, Binding { symbol, weak });
                }
            }
            self.loaded.insert(name.clone(), library);
        }
        for pending in pending_functions {
            match self.resolve(&pending.owner, &pending.name) {
                Some(Symbol::Function(function)) => {
                    ensure!(
                        function.matches_ty(&*store, &pending.expected),
                        "{}: function type mismatch for {}",
                        pending.owner,
                        pending.name
                    );
                    pending
                        .target
                        .set(function)
                        .map_err(|_| anyhow!("function bound twice"))?;
                }
                None if pending.weak => (),
                _ => bail!("{}: unresolved function {}", pending.owner, pending.name),
            }
        }
        for pending in pending_addresses {
            let mut symbol = self.resolve(&pending.owner, &pending.name);
            if symbol.is_none()
                && pending.function
                && let Some(Extern::Func(function)) =
                    host_import(store, host, "env", &pending.name)?
            {
                symbol = Some(Symbol::Function(function));
                self.symbols.insert(
                    pending.name.clone(),
                    Binding {
                        symbol: Symbol::Function(function),
                        weak: false,
                    },
                );
            }
            let address = match (symbol, pending.function) {
                (Some(Symbol::Function(function)), true) => {
                    self.function_pointer(&mut *store, function)?
                }
                (Some(Symbol::Data(address)), false) => address,
                (None, _) if pending.weak => 0,
                _ => bail!(
                    "{}: unresolved {} address {}",
                    pending.owner,
                    if pending.function { "function" } else { "data" },
                    pending.name
                ),
            };
            pending.global.set(&mut *store, Val::I32(address as i32))?;
        }
        for helper in ["__wasm_apply_data_relocs", "__wasm_call_ctors"] {
            for name in order {
                let instance = self.loaded[name].instance;
                if let Some(function) = instance.get_func(&mut *store, helper) {
                    function
                        .typed::<(), ()>(&*store)?
                        .call(&mut *store, ())
                        .with_context(|| format!("{name}: {helper}"))?;
                }
            }
        }
        Ok(())
    }

    fn resolve(&self, owner: &str, name: &str) -> Option<Symbol> {
        if name == "__dso_handle" || name == "__tls_base" {
            return self.loaded[owner].symbol(name);
        }
        if self.definitions[owner]
            .metadata
            .export_flags(name)
            .contains(SymbolFlags::VISIBILITY_HIDDEN)
        {
            return self.loaded[owner].symbol(name);
        }
        self.symbol(name)
            .or_else(|| self.loaded[owner].symbol(name))
    }

    fn prepare_regions<T>(
        &self,
        store: &mut StoreContextMut<'_, T>,
        memory_base: u32,
        memory_size: u32,
        table_base: u32,
        table_size: u32,
    ) -> Result<()> {
        let end = u64::from(memory_base) + u64::from(memory_size);
        let page_size = self.memory.ty(&*store).page_size();
        let pages = end.div_ceil(page_size);
        let current = self.memory.size(&*store);
        if pages > current {
            self.memory.grow(&mut *store, pages - current)?;
        }
        let zeros = [0; 4096];
        for at in (memory_base as usize..end as usize).step_by(zeros.len()) {
            self.memory.write(
                &mut *store,
                at,
                &zeros[..zeros.len().min(end as usize - at)],
            )?;
        }
        let table_end = u64::from(table_base) + u64::from(table_size);
        let current = self.table.size(&*store);
        if table_end > current {
            self.table
                .grow(&mut *store, table_end - current, Ref::Func(None))?;
        }
        for index in u64::from(table_base)..table_end {
            self.table.set(&mut *store, index, Ref::Func(None))?;
        }
        Ok(())
    }

    /// Return a stable C function pointer (table index). Reuses an existing table
    /// entry when possible, including entries initialized by Wasm element segments.
    /// This method must use the same store as the loader.
    pub fn function_pointer(
        &mut self,
        mut store: impl AsContextMut,
        function: Func,
    ) -> Result<u32> {
        ensure!(!self.poisoned, "loader is poisoned");
        let mut store = store.as_context_mut();
        let identity = function.to_raw(&mut store);
        for index in 1..self.table.size(&store) {
            if let Some(Ref::Func(Some(other))) = self.table.get(&mut store, index)
                && other.to_raw(&mut store) == identity
            {
                return u32::try_from(index).context("function pointer exceeds wasm32");
            }
        }
        let index = reserve(&mut self.next_table, 1, 0, self.arena.table.end)?;
        let size = self.table.size(&store);
        if u64::from(index) >= size {
            self.table
                .grow(&mut store, u64::from(index) + 1 - size, Ref::Func(None))?;
        }
        self.table
            .set(&mut store, u64::from(index), Ref::Func(Some(function)))?;
        Ok(index)
    }
}

fn base_global<T>(store: &mut StoreContextMut<'_, T>, value: u32) -> Result<Global> {
    Global::new(
        store,
        GlobalType::new(ValType::I32, Mutability::Const),
        Val::I32(value as i32),
    )
}

fn reserve(cursor: &mut u32, size: u32, alignment: u32, limit: u32) -> Result<u32> {
    let mask = 1u32.checked_shl(alignment).context("invalid alignment")? - 1;
    let start = cursor.checked_add(mask).context("allocation overflow")? & !mask;
    let end = start.checked_add(size).context("allocation overflow")?;
    ensure!(end <= limit, "dynamic linking arena exhausted");
    *cursor = end;
    Ok(start)
}

fn host_import<T: 'static>(
    store: &mut StoreContextMut<'_, T>,
    host: &Linker<T>,
    namespace: &str,
    name: &str,
) -> Result<Option<Extern>> {
    match host.get(store, namespace, name) {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.is::<wasmtime::OutOfMemory>() => Err(error),
        Err(_) => Ok(None),
    }
}
