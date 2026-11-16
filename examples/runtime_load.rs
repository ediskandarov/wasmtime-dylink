//! A guest-triggered load using an application-owned import adapter.
use wasmtime_dylink::{Arena, Loader, Symbol, wasmtime::*};

#[derive(Default)]
struct State {
    loader: Option<Loader>,
    host: Option<Linker<State>>,
}

fn main() -> Result<()> {
    let engine = Engine::default();
    let mut store = Store::new(&engine, State::default());
    let memory = Memory::new(&mut store, MemoryType::new(2, Some(16)))?;
    let table = Table::new(
        &mut store,
        TableType::new(RefType::FUNCREF, 1, Some(64)),
        Ref::Func(None),
    )?;
    let mut loader = Loader::new(
        &mut store,
        memory,
        table,
        Arena {
            memory: 65536..16 * 65536,
            table: 1..64,
        },
    )?;
    loader.register(
        "plugin",
        &wat::parse_str(
            r#"(module
      (@dylink.0 (mem-info))
      (func (export "answer") (result i32) i32.const 42))"#,
        )?,
    )?;

    let mut host = Linker::<State>::new(&engine);
    host.define(&store, "env", "__indirect_function_table", table)?;
    host.func_wrap(
        "app",
        "load_plugin",
        |mut caller: Caller<'_, State>| -> Result<i32> {
            // Temporarily take ownership: loading needs mutable access to the Caller
            // and to the loader. A constructor recursively requesting another load
            // receives an error instead of borrowing the loader twice.
            let mut loader = caller
                .data_mut()
                .loader
                .take()
                .ok_or_else(|| format_err!("reentrant load"))?;
            let host = caller.data().host.as_ref().unwrap().clone();
            let result = (|| {
                let library = loader.load(&mut caller, &host, "plugin")?;
                let Some(Symbol::Function(function)) = library.symbol("answer") else {
                    bail!("missing plugin entry point");
                };
                loader.function_pointer(&mut caller, function)
            })();
            caller.data_mut().loader = Some(loader);
            result.map(|pointer| pointer as i32)
        },
    )?;
    store.data_mut().loader = Some(loader);
    store.data_mut().host = Some(host.clone());

    let main = Module::from_binary(
        &engine,
        &wat::parse_str(
            r#"(module
      (type $getter (func (result i32)))
      (import "env" "__indirect_function_table" (table 1 funcref))
      (import "app" "load_plugin" (func $load (result i32)))
      (func (export "run") (result i32) call $load call_indirect (type $getter)))"#,
        )?,
    )?;
    let instance = host.instantiate(&mut store, &main)?;
    let run = instance.get_typed_func::<(), i32>(&mut store, "run")?;
    assert_eq!(run.call(&mut store, ())?, 42);
    assert_eq!(run.call(&mut store, ())?, 42);
    println!("Guest loaded its plugin and called it through the shared table: 42");
    Ok(())
}
