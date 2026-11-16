#![cfg(feature = "emscripten")]
use std::{env, fs, path::Path, process::Command};
use wasmtime_dylink::{
    Arena,
    emscripten::{self, Emscripten},
    wasmtime::*,
};

#[derive(Default)]
struct State {
    dylink: Option<Emscripten<State>>,
    requests: Vec<String>,
}
fn run(command: &mut Command) {
    let output = command.output().expect("install Emscripten, or set EMCC");
    assert!(
        output.status.success(),
        "{command:?}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
fn empty_environment(mut caller: Caller<'_, State>, count: i32, bytes: i32) -> Result<i32> {
    let memory = caller
        .get_export("memory")
        .and_then(Extern::into_memory)
        .unwrap();
    memory.write(&mut caller, count as u32 as usize, &[0; 4])?;
    memory.write(&mut caller, bytes as u32 as usize, &[0; 4])?;
    Ok(0)
}

#[test]
#[ignore = "requires Emscripten; run with --features emscripten --test emscripten_c -- --ignored"]
fn real_emscripten_dlopen_dlsym_and_dlerror() -> Result<()> {
    run_fixture(false)?;
    run_fixture(true)
}

fn run_fixture(asyncify: bool) -> Result<()> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let output = root.join(if asyncify {
        "target/emscripten-fixtures-asyncify"
    } else {
        "target/emscripten-fixtures"
    });
    fs::create_dir_all(&output)?;
    let compiler = env::var_os("EMCC").unwrap_or("emcc".into());
    run(Command::new(&compiler)
        .arg(root.join("tests/fixtures/emscripten_plugin.c"))
        .args(["-O1", "-sSIDE_MODULE=1", "-o"])
        .arg(output.join("plugin.so")));
    run(Command::new(&compiler).arg(root.join("tests/fixtures/emscripten_main.c"))
        .args(["-O1", if asyncify { "-sASYNCIFY=1" } else { "-sASYNCIFY=0" }, "-sMAIN_MODULE=2", "-sALLOW_TABLE_GROWTH=1", "-sALLOW_MEMORY_GROWTH=1",
        "-sEXPORTED_FUNCTIONS=[\"_check_local\",\"_check_global\",\"_check_errors\",\"_check_main\",\"_host_value\",\"_malloc\",\"_free\",\"___dl_seterr\"]", "-o"])
        .arg(output.join("main.js")));
    let engine = Engine::default();
    let module = Module::from_binary(&engine, &fs::read(output.join("main.wasm"))?)?;
    assert_eq!(
        module
            .exports()
            .any(|e| e.name() == "asyncify_start_unwind"),
        asyncify
    );
    let mut store = Store::new(&engine, State::default());
    let mut host = Linker::new(&engine);
    emscripten::add_to_linker(&mut host, |state: &mut State| &mut state.dylink)?;
    host.func_wrap(
        "wasi_snapshot_preview1",
        "environ_sizes_get",
        empty_environment,
    )?;
    host.func_wrap("wasi_snapshot_preview1", "environ_get", |_: i32, _: i32| {
        0i32
    })?;
    // No filesystem exists in this fixture. libc sees ENOENT and delegates bytes
    // to our resolver. Any unexpected IO/heap growth traps rather than succeeding.
    for import in module.imports() {
        if host.get(&mut store, import.module(), import.name()).is_ok() {
            continue;
        }
        let ExternType::Func(ty) = import.ty() else {
            bail!("unexpected import {import:?}");
        };
        let name = import.name().to_owned();
        let namespace = import.module().to_owned();
        host.func_new(
            import.module(),
            import.name(),
            ty,
            move |_, _, results| match (namespace.as_str(), name.as_str()) {
                (
                    "env",
                    "__syscall_stat64"
                    | "__syscall_fstat64"
                    | "__syscall_lstat64"
                    | "__syscall_newfstatat"
                    | "__syscall_openat",
                ) => {
                    results[0] = Val::I32(-44);
                    Ok(())
                }
                _ => bail!("unexpected fixture syscall {namespace}.{name}"),
            },
        )?;
    }
    let main = host.instantiate(&mut store, &module)?;
    main.get_typed_func::<(), ()>(&mut store, "__wasm_call_ctors")?
        .call(&mut store, ())?;
    let malloc = main.get_typed_func::<i32, i32>(&mut store, "malloc")?;
    let base = malloc.call(&mut store, 1024 * 1024)? as u32;
    assert_ne!(base, 0);
    let table = main
        .get_table(&mut store, "__indirect_function_table")
        .unwrap();
    let table_base = table.size(&store) as u32;
    let plugin = fs::read(output.join("plugin.so"))?;
    let adapter = Emscripten::new(
        &mut store,
        main,
        host,
        Arena {
            memory: base..base + 1024 * 1024,
            table: table_base..table_base + 128,
        },
        move |store, name| {
            store.data_mut().requests.push(name.to_owned());
            match name {
                "plugin.so" | "global.so" => Ok(plugin.clone()),
                _ => bail!("missing module: {name}"),
            }
        },
    )?;
    store.data_mut().dylink = Some(adapter);
    for name in ["check_local", "check_errors", "check_main", "check_global"] {
        assert_eq!(
            main.get_typed_func::<(), i32>(&mut store, name)?
                .call(&mut store, ())?,
            0,
            "{name}"
        );
    }
    assert_eq!(
        store.data().requests,
        ["plugin.so", "missing%lib.so", "global.so"]
    );
    Ok(())
}
