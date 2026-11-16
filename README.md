# wasmtime-dylink

Load LLVM/Emscripten-style **wasm32 PIC shared libraries** in Wasmtime using
[`dylink.0`](https://github.com/WebAssembly/tool-conventions/blob/main/DynamicLinking.md).
Uses Wasmtime's public API; no runtime fork, WASIX runtime, or JavaScript engine.

Early release. Tested with standalone LLVM C libraries and synthetic Wasm modules.
It is a linker, **not a complete Emscripten or WASIX execution environment**.

```toml
[dependencies]
wasmtime-dylink = "0.2"
```

```rust,no_run
use wasmtime_dylink::{Arena, Loader, Symbol, wasmtime::*};

fn main() -> Result<()> {
    let engine = Engine::default();
    let mut store = Store::new(&engine, ());
    let memory = Memory::new(&mut store, MemoryType::new(2, Some(256)))?;
    let table = Table::new(
        &mut store, TableType::new(RefType::FUNCREF, 1, Some(4096)), Ref::Func(None),
    )?;
    let stack = Global::new(
        &mut store, GlobalType::new(ValType::I32, Mutability::Var), Val::I32(65536),
    )?;
    let mut host = Linker::new(&engine);
    host.define(&store, "env", "__stack_pointer", stack)?;
    let mut loader = Loader::new(&mut store, memory, table, Arena {
        memory: 65536..16 * 1024 * 1024,
        table: 1..4096,
    })?;
    loader.register("plugin.so", &std::fs::read("plugin.so")?)?;
    let library = loader.load(&mut store, &host, "plugin.so")?;
    if let Some(Symbol::Function(function)) = library.symbol("answer") {
        let answer = function.typed::<(), i32>(&store)?.call(&mut store, ())?;
        println!("{answer}");
    }
    Ok(())
}
```

For a self-contained, guest-triggered load:

```sh
cargo run --example runtime_load
```

The [example](examples/runtime_load.rs) implements an application import which
loads a plugin while another Wasm instance is running, then returns a function
pointer. Applications can build their `dlopen`/`dlsym` interface on the same API.
For Emscripten's C `dlopen`/`dlsym` API, enable the optional adapter below.
Filesystem lookup, libc, WASI system imports and asynchronous suspension remain
responsibilities of the embedding runtime.

## Memory and symbol ownership

Pass the **same Store** on every call, just as required by Wasmtime handles.
Supply existing main-module memory/table or create them before loading a PIC
main module. `define` exposes main-module functions and already relocated data
addresses. A `Linker` supplies system functions and globals, such as the stack
pointer. `GOT.func` can also reference `env` functions supplied by that linker.

The arena must be reserved exclusively for this loader. It must not overlap the
main module's stack, static data, heap, or another table allocator. This crate
does not coordinate with guest `malloc`. Memory/table grow only when needed,
within the arena and Wasmtime limits. Each new module's memory is zeroed before
instantiation, including BSS. Function pointers use the shared table; existing
entries are reused to preserve identity.

`load_local` isolates a library group from the global symbol scope; `promote`
exposes an already loaded group without running constructors again.

Register dependencies by the exact names in `Metadata::needed`. Registration
parses and compiles; loading instantiates the dependency closure. No filesystem
access or automatic search-path expansion occurs. Runtime paths are available
as metadata for an application-owned resolver.

Dependencies are visited in declaration order, depth first. Existing symbol
bindings stay stable. Within a new load group, the first strong definition wins,
or the first weak definition if no strong one exists. Hidden symbols stay local.
All GOT entries and forward function bindings are resolved before data
relocations; all relocations precede all constructors. Dependencies initialize
before consumers. Cycles use the deterministic DFS order and function forwarding
stubs; constructors in a cycle must not assume each other has already run.

## Lifecycle and limits

- Repeated loads return the **same instance** and preserve state. Constructors
  run once. There is no `dlclose`: Wasmtime retains instances until the Store is
  dropped. Use a new Store for a clean session, rather than repeatedly creating
  loaders in one Store.
- A missing dependency or registration error can be corrected and retried.
  A failure after loading begins poisons the loader. Initializers may have changed
  shared state; discard that Store. The loader does not promise transactional
  rollback of guest writes.
- Supported: ordinary wasm32 memory, `funcref` tables, PIC data/function imports,
  weak imports, dependency cycles, data relocations and constructors.
- LLVM's `__wasm_init_memory` start function and Binaryen's base-relative global/BSS
  initialization are supported. Other starts are rejected. Binaryen starts may
  only assign module-local globals and zero-fill within the module's memory;
  they cannot call functions or read unresolved GOT entries. User constructors
  belong in `__wasm_call_ctors`, after relocation.
- Not supported: memory64, thread-shared memory/TLS, async instantiation,
  ELF/POSIX symbol-version semantics, nested loads from constructors,
  and re-exporting a function whose binding is still unresolved. Unsupported
  metadata/imports fail explicitly. Modules must import their memory/table.
- Modules share memory and authority. They are **not isolated from one another**.
  Arenas prevent allocator overlap, not arbitrary guest writes. Apply Wasmtime
  fuel/epoch/resource limits as appropriate; this loader does not impose them.

The dependency is Wasmtime 47, requiring Rust 1.94 or newer. This repository's
checks use Rust 1.95. No game code, game binaries, or game resources are included.

## Emscripten adapter

Enable `features = ["emscripten"]` on the dependency. The feature is off by
default and adds no dependencies. It supplies `env._dlopen_js` and
`env._dlsym_js` for an initialized, non-PIE wasm32 Emscripten main module.

1. Keep an `Option<Emscripten<AppState>>` in your Wasmtime store data.
2. Call `emscripten::add_to_linker(&mut host, |s| &mut s.dylink)` before
   instantiating the main module. Provide its other system imports yourself.
3. Initialize the main C runtime. Reserve a memory arena using its `malloc`
   and table indices after its existing table entries. Keep that allocation alive.
4. Construct `Emscripten::new(store, main, host, arena, resolver)` and place it
   in the state's option. The resolver receives a store context and an exact
   library name, returning Wasm bytes. It can use application assets, an MR
   reader, or a filesystem chosen by the application.

The main must export `memory`, `__indirect_function_table`, `malloc`, `free`,
and `__dl_seterr`. Export the C functions/data that side modules need, too.
Main exports are made available to side modules automatically; duplicate host
imports are rejected. See [the complete C integration test](tests/emscripten_c.rs)
for setup and the required compiler flags.

The adapter reads the guest `dso`, fills its allocated memory/table fields,
uses embedded `file_data` when supplied, and resolves missing dependency bytes
through the callback. It supports local/global scopes, eager `RTLD_LAZY/NOW`,
`RTLD_NOLOAD`, `RTLD_NODELETE`, `RTLD_DEFAULT`, and `dlopen(NULL)`.
Names are passed unchanged; path aliases/search rules belong to the resolver.
C `dlerror()` receives lookup/resolver errors and keeps its normal read-once
behavior. `dlclose` uses Emscripten's existing retained-instance behavior.
A constructor/linking failure poisons the loader and traps: discard that store.

This is the internal Emscripten wasm32 `dso` ABI with the inline name at byte
36, checked against SDK 6.0.13-git locally; CI is configured for SDK 6.0.12.
Run the C test when changing SDK versions. Main modules built with Asyncify
are tested with synchronous loading; side modules do not need Asyncify.
Asynchronous loading, thread catch-up, `RTLD_NEXT`, and reentrant constructor
loads are unsupported.

```sh
cargo test --all-features
EMCC=emcc cargo test --features emscripten --test emscripten_c -- --ignored
```

## Verification

```sh
cargo test
cargo run --example runtime_load
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

The real-C test requires LLVM `clang` and `wasm-ld` and is run explicitly in CI:

```sh
WASM_CC=clang WASM_LD=wasm-ld cargo test --test clang -- --ignored
```

It compiles the [original fixtures](tests/fixtures) as PIC shared libraries,
then checks dependencies, BSS, data relocations, constructors, direct/indirect
calls, callback identity and repeated loads. The normal test suite uses original
WAT fixtures and does not require a C toolchain.
