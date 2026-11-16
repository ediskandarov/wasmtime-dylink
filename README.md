# wasmtime-dylink

Load LLVM/Emscripten-style **wasm32 PIC shared libraries** in Wasmtime using
[`dylink.0`](https://github.com/WebAssembly/tool-conventions/blob/main/DynamicLinking.md).
Uses Wasmtime's public API; no runtime fork, WASIX runtime, or JavaScript engine.

Early release. Tested with standalone LLVM C libraries and synthetic Wasm modules.
It is a linker, **not a complete Emscripten or WASIX execution environment**.

```toml
[dependencies]
wasmtime-dylink = "0.1"
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
Emscripten's `_dlopen_js` protocol, filesystem lookup, Asyncify, libc, and WASI
system imports remain responsibilities of the embedding runtime.

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
- LLVM's start function is accepted only when also exported as
  `__wasm_init_memory`. It executes during instantiation and must only initialize
  that module's memory. User constructors belong in `__wasm_call_ctors`.
- Not supported: memory64, thread-shared memory/TLS, async instantiation,
  ELF/POSIX symbol-version or `RTLD_*` semantics, nested loads from constructors,
  and re-exporting a function whose binding is still unresolved. Unsupported
  metadata/imports fail explicitly. Modules must import their memory/table.
- Modules share memory and authority. They are **not isolated from one another**.
  Arenas prevent allocator overlap, not arbitrary guest writes. Apply Wasmtime
  fuel/epoch/resource limits as appropriate; this loader does not impose them.

The dependency is Wasmtime 47, requiring Rust 1.94 or newer. This repository's
checks use Rust 1.95. No game code, game binaries, or game resources are included.

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
