//! Synchronous, single-threaded Emscripten wasm32 `dlopen`/`dlsym` imports.
//!
//! Instantiate the non-PIE main module first, initialize its C runtime, reserve
//! an arena (for example with its `malloc`), then construct [`Emscripten`]. The
//! caller supplies system imports and a module-byte resolver; no filesystem or
//! application resource format is built into this adapter.

use crate::{Arena, Loader, Symbol};
use std::collections::{HashMap, HashSet, VecDeque};
use wasmtime::{
    AsContextMut, Caller, Extern, Instance, Linker, Mutability, Result, StoreContextMut, TypedFunc,
    ValType, ensure, error::Context,
};

// Emscripten wasm32 struct dso in system/lib/libc/musl/src/internal/dynlink.h.
// Keep the C integration test on the installed SDK: this is an internal ABI.
const FLAGS: usize = 4;
const ALLOCATED: usize = 8;
const MEMORY_ADDRESS: usize = 12;
const MEMORY_SIZE: usize = 16;
const TABLE_ADDRESS: usize = 20;
const TABLE_SIZE: usize = 24;
const FILE_DATA: usize = 28;
const FILE_SIZE: usize = 32;
const NAME: usize = 36;
const MAX_STRING: usize = 4096;
const RTLD_LAZY: u32 = 1;
const RTLD_NOW: u32 = 2;
const RTLD_NOLOAD: u32 = 4;
const RTLD_GLOBAL: u32 = 256;
const RTLD_NODELETE: u32 = 4096;

type Resolver<T> = Box<dyn FnMut(&mut StoreContextMut<'_, T>, &str) -> Result<Vec<u8>> + Send>;

/// Application-owned state for the Emscripten imports. Keep it in an `Option` in
/// the store's data; [`add_to_linker`] temporarily takes it while linking.
/// Reentrant loading from constructors is rejected with a trap.
pub struct Emscripten<T: 'static> {
    loader: Loader,
    host: Linker<T>,
    resolver: Resolver<T>,
    handles: HashMap<u32, String>,
    malloc: TypedFunc<i32, i32>,
    free: TypedFunc<i32, ()>,
    set_error: TypedFunc<(i32, i32), ()>,
}

impl<T: 'static> Emscripten<T> {
    /// Attach to an initialized Emscripten main instance with exported `memory`,
    /// `__indirect_function_table`, `malloc`, `free` and `__dl_seterr`.
    /// Main exports become imports available to side modules. Conflicting host
    /// definitions are rejected rather than silently replacing runtime symbols.
    /// The resolver receives exact library/dependency names, without implicit IO.
    pub fn new(
        mut store: impl AsContextMut<Data = T>,
        main: Instance,
        mut host: Linker<T>,
        arena: Arena,
        resolver: impl FnMut(&mut StoreContextMut<'_, T>, &str) -> Result<Vec<u8>> + Send + 'static,
    ) -> Result<Self> {
        let mut store = store.as_context_mut();
        let memory = main
            .get_memory(&mut store, "memory")
            .context("main must export memory")?;
        let table = main
            .get_table(&mut store, "__indirect_function_table")
            .context("main must export __indirect_function_table")?;
        let malloc = main.get_typed_func(&mut store, "malloc")?;
        let free = main.get_typed_func(&mut store, "free")?;
        let set_error = main.get_typed_func(&mut store, "__dl_seterr")?;
        let mut loader = Loader::new(&mut store, memory, table, arena)?;
        let exports: Vec<_> = main
            .exports(&mut store)
            .map(|e| (e.name().to_owned(), e.into_extern()))
            .collect();
        // Avoid inheriting a permissive shadowing policy from the embedding app.
        host.allow_shadowing(false);
        for (name, value) in exports {
            if name.starts_with("__wasm_") {
                continue;
            }
            host.define(&store, "env", &name, value.clone())?;
            let symbol = match value {
                Extern::Func(function) => Some(Symbol::Function(function)),
                Extern::Global(global)
                    if global.ty(&store).mutability() == Mutability::Const
                        && ValType::eq(global.ty(&store).content(), &ValType::I32) =>
                {
                    Some(Symbol::Data(global.get(&mut store).i32().unwrap() as u32))
                }
                _ => None,
            };
            if let Some(symbol) = symbol {
                loader.define(name, symbol)?;
            }
        }
        Ok(Self {
            loader,
            host,
            resolver: Box::new(resolver),
            handles: HashMap::new(),
            malloc,
            free,
            set_error,
        })
    }

    /// Inspect loader state, including whether a fatal initialization error has
    /// poisoned it. Do not continue executing a poisoned store.
    pub fn loader(&self) -> &Loader {
        &self.loader
    }

    fn open(&mut self, store: &mut StoreContextMut<'_, T>, handle: u32) -> Result<i32> {
        ensure!(handle != 0, "null dso descriptor");
        let memory = self.loader.memory();
        let bytes = memory.data(&*store);
        let base = handle as usize;
        let header = bytes
            .get(base..base.checked_add(NAME).context("dso overflow")?)
            .context("dso descriptor out of bounds")?;
        let flags = read_u32(header, FLAGS)?;
        ensure!(
            flags & (RTLD_LAZY | RTLD_NOW) != 0,
            "dlopen requires RTLD_NOW or RTLD_LAZY"
        );
        ensure!(
            flags & !(RTLD_LAZY | RTLD_NOW | RTLD_NOLOAD | RTLD_GLOBAL | RTLD_NODELETE) == 0,
            "unsupported dlopen flags: {flags:#x}"
        );
        let name = c_string(bytes, base + NAME)?.to_owned();
        ensure!(!name.is_empty(), "empty library name");
        if let Some(previous) = self.handles.get(&handle) {
            ensure!(previous == &name, "dso descriptor changed its library name");
        }
        if flags & RTLD_NOLOAD != 0 {
            ensure!(
                self.loader.is_loaded(&name),
                "library is not loaded: {name}"
            );
        }
        let data = read_u32(header, FILE_DATA)? as usize;
        let size = read_u32(header, FILE_SIZE)? as usize;
        let inline = if data != 0 && self.loader.metadata(&name).is_none() {
            Some(
                bytes
                    .get(data..data.checked_add(size).context("file data overflow")?)
                    .context("dso file data out of bounds")?
                    .to_vec(),
            )
        } else {
            None
        };
        self.register_closure(store, &name, inline)?;
        let library = if flags & RTLD_GLOBAL != 0 {
            // Include dependencies previously loaded with RTLD_LOCAL.
            let library = self.loader.load_local(&mut *store, &self.host, &name)?;
            self.loader.promote(&name)?;
            library
        } else {
            self.loader.load_local(&mut *store, &self.host, &name)?
        };
        let layout = &self.loader.metadata(&name).unwrap().layout;
        let sizes = (layout.memory_size, layout.table_size);
        let header = &mut memory.data_mut(store)[base..base + NAME];
        write_u32(header, MEMORY_ADDRESS, library.memory_base)?;
        write_u32(header, MEMORY_SIZE, sizes.0)?;
        write_u32(header, TABLE_ADDRESS, library.table_base)?;
        write_u32(header, TABLE_SIZE, sizes.1)?;
        header[ALLOCATED] = 1;
        self.handles.insert(handle, name);
        Ok(1)
    }

    fn register_closure(
        &mut self,
        store: &mut StoreContextMut<'_, T>,
        name: &str,
        mut inline: Option<Vec<u8>>,
    ) -> Result<()> {
        let mut queue = VecDeque::from([name.to_owned()]);
        let mut seen = HashSet::new();
        while let Some(current) = queue.pop_front() {
            if !seen.insert(current.clone()) {
                continue;
            }
            if self.loader.metadata(&current).is_none() {
                let bytes = if current == name { inline.take() } else { None };
                let bytes = match bytes {
                    Some(bytes) => bytes,
                    None => (self.resolver)(store, &current)?,
                };
                self.loader.register(&current, &bytes)?;
            }
            queue.extend(
                self.loader
                    .metadata(&current)
                    .unwrap()
                    .needed
                    .iter()
                    .cloned(),
            );
        }
        Ok(())
    }

    fn lookup(
        &mut self,
        store: &mut StoreContextMut<'_, T>,
        handle: u32,
        name: u32,
        index: u32,
    ) -> Result<i32> {
        ensure!(handle != u32::MAX, "RTLD_NEXT is unsupported");
        let memory = self.loader.memory();
        let name = c_string(memory.data(&*store), name as usize)?.to_owned();
        ensure!(index != 0, "null symbol-index pointer");
        write_u32(memory.data_mut(&mut *store), index as usize, u32::MAX)?;
        let symbol = if handle == 0 {
            self.loader.symbol(&name)
        } else {
            let library = self.handles.get(&handle).context("unknown dso handle")?;
            self.loader.symbol_in(library, &name)
        }
        .with_context(|| format!("unknown dynamic symbol: {name}"))?;
        match symbol {
            Symbol::Data(address) => Ok(address as i32),
            Symbol::Function(function) => Ok(self.loader.function_pointer(store, function)? as i32),
        }
        // Single-threaded: keep sym_index at -1. No thread catch-up event is needed.
    }

    fn report_error(
        &self,
        store: &mut StoreContextMut<'_, T>,
        error: &wasmtime::Error,
    ) -> Result<()> {
        // __dl_seterr is variadic: use a constant "%s" format and an aligned
        // wasm32 va_list. Library names containing '%' must never become formats.
        let message = format!("{error:#}").replace('\0', "?");
        let message = &message.as_bytes()[..message.len().min(8192)];
        let arguments = (message.len() + 1 + 3) & !3;
        let format = arguments + 4;
        let size = format + 3;
        let pointer = self.malloc.call(&mut *store, size as i32)? as u32;
        ensure!(pointer != 0, "cannot allocate dlerror message");
        let result = (|| {
            let memory = self.loader.memory();
            memory.write(&mut *store, pointer as usize, message)?;
            memory.write(&mut *store, pointer as usize + message.len(), &[0])?;
            memory.write(
                &mut *store,
                pointer as usize + arguments,
                &pointer.to_le_bytes(),
            )?;
            memory.write(&mut *store, pointer as usize + format, b"%s\0")?;
            self.set_error.call(
                &mut *store,
                (
                    (pointer as usize + format) as i32,
                    (pointer as usize + arguments) as i32,
                ),
            )
        })();
        let freed = self.free.call(&mut *store, pointer as i32);
        result.and(freed)
    }
}

/// Add `_dlopen_js` and `_dlsym_js` imports to `env`. The main instance can be
/// created before the adapter is attached; invoking these imports before attach
/// or recursively while linking traps with an explicit diagnostic.
pub fn add_to_linker<T: 'static>(
    linker: &mut Linker<T>,
    access: fn(&mut T) -> &mut Option<Emscripten<T>>,
) -> Result<()> {
    linker.func_wrap(
        "env",
        "_dlopen_js",
        move |mut caller: Caller<'_, T>, handle: i32| {
            with_adapter(&mut caller, access, |adapter, store| {
                adapter.open(store, handle as u32)
            })
        },
    )?;
    linker.func_wrap(
        "env",
        "_dlsym_js",
        move |mut caller: Caller<'_, T>, handle: i32, symbol: i32, index: i32| {
            with_adapter(&mut caller, access, |adapter, store| {
                adapter.lookup(store, handle as u32, symbol as u32, index as u32)
            })
        },
    )?;
    Ok(())
}

fn with_adapter<T: 'static>(
    caller: &mut Caller<'_, T>,
    access: fn(&mut T) -> &mut Option<Emscripten<T>>,
    operation: impl FnOnce(&mut Emscripten<T>, &mut StoreContextMut<'_, T>) -> Result<i32>,
) -> Result<i32> {
    let mut adapter = access(caller.data_mut())
        .take()
        .context("Emscripten dylink adapter is not attached or a load is reentrant")?;
    let result = operation(&mut adapter, &mut caller.as_context_mut());
    let result = match result {
        Ok(value) => Ok(value),
        Err(error) if adapter.loader.is_poisoned() => {
            Err(error.context("dynamic initialization failed; discard this store"))
        }
        Err(error) => adapter
            .report_error(&mut caller.as_context_mut(), &error)
            .map(|()| 0),
    };
    *access(caller.data_mut()) = Some(adapter);
    result
}

fn c_string(bytes: &[u8], start: usize) -> Result<&str> {
    ensure!(start != 0, "null string pointer");
    let bytes = bytes.get(start..).context("string pointer out of bounds")?;
    let end = bytes
        .iter()
        .take(MAX_STRING)
        .position(|byte| *byte == 0)
        .context("unterminated or oversized string")?;
    Ok(std::str::from_utf8(&bytes[..end])?)
}
fn read_u32(bytes: &[u8], start: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(
        bytes
            .get(start..start.checked_add(4).context("address overflow")?)
            .context("read out of bounds")?
            .try_into()
            .unwrap(),
    ))
}
fn write_u32(bytes: &mut [u8], start: usize, value: u32) -> Result<()> {
    bytes
        .get_mut(start..start.checked_add(4).context("address overflow")?)
        .context("write out of bounds")?
        .copy_from_slice(&value.to_le_bytes());
    Ok(())
}
