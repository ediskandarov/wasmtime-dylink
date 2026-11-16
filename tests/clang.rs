//! Real LLVM PIC output, kept separate from the toolchain-independent WAT suite.
use std::{env, fs, path::Path, process::Command};
use wasmtime_dylink::{Arena, Loader, Symbol, wasmtime::*};

fn run(command: &mut Command) {
    let output = command
        .output()
        .expect("install clang and wasm-ld, or set WASM_CC and WASM_LD");
    assert!(
        output.status.success(),
        "{command:?}\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn compile(root: &Path, output: &Path, name: &str) {
    run(
        Command::new(env::var_os("WASM_CC").unwrap_or("clang".into()))
            .args(["--target=wasm32-unknown-unknown", "-fPIC", "-O1", "-c"])
            .arg(root.join(format!("tests/fixtures/{name}.c")))
            .arg("-o")
            .arg(output.join(format!("{name}.o"))),
    );
    let mut link = Command::new(env::var_os("WASM_LD").unwrap_or("wasm-ld".into()));
    link.args(["--shared", "--export-all", "--allow-undefined"])
        .arg(output.join(format!("{name}.o")))
        .arg("-o")
        .arg(output.join(format!("lib{name}.so")));
    if name == "consumer" {
        link.arg("-L").arg(output).arg("-lprovider");
    }
    run(&mut link);
}

#[test]
#[ignore = "requires clang and wasm-ld; CI runs this explicitly"]
fn llvm_c_libraries_share_state_and_callbacks() -> Result<()> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let output = root.join("target/clang-fixtures");
    fs::create_dir_all(&output)?;
    compile(root, &output, "provider");
    compile(root, &output, "consumer");
    let engine = Engine::default();
    let mut store = Store::new(&engine, ());
    let memory = Memory::new(&mut store, MemoryType::new(4, Some(16)))?;
    let table = Table::new(
        &mut store,
        TableType::new(RefType::FUNCREF, 1, Some(128)),
        Ref::Func(None),
    )?;
    let stack = Global::new(
        &mut store,
        GlobalType::new(ValType::I32, Mutability::Var),
        Val::I32(65536),
    )?;
    let mut host = Linker::new(&engine);
    host.define(&store, "env", "__stack_pointer", stack)?;
    // Dirty the entire arena: allocating libraries must initialize their BSS.
    memory.data_mut(&mut store)[65536..].fill(0xa5);
    let mut loader = Loader::new(
        &mut store,
        memory,
        table,
        Arena {
            memory: 65536..16 * 65536,
            table: 1..128,
        },
    )?;
    for name in ["provider", "consumer"] {
        loader.register(
            format!("lib{name}.so"),
            &fs::read(output.join(format!("lib{name}.so")))?,
        )?;
    }
    assert_eq!(
        loader.metadata("libconsumer.so").unwrap().needed,
        ["libprovider.so"]
    );
    let library = loader.load(&mut store, &host, "libconsumer.so")?;
    for (function, expected) in [
        ("direct", 42),
        ("indirect", 42),
        ("relocated_data", 10),
        ("pointer_identity", 1),
    ] {
        assert_eq!(
            library
                .instance
                .get_typed_func::<(), i32>(&mut store, function)?
                .call(&mut store, ())?,
            expected,
            "{function}"
        );
    }
    let provider = loader.load(&mut store, &host, "libprovider.so")?;
    assert_eq!(
        provider
            .instance
            .get_typed_func::<(), i32>(&mut store, "bss_is_zero")?
            .call(&mut store, ())?,
        1
    );
    let callback = Func::wrap(&mut store, |value: i32| value * 2);
    let pointer = loader.function_pointer(&mut store, callback)?;
    assert_eq!(
        library
            .instance
            .get_typed_func::<(i32, i32), i32>(&mut store, "callback")?
            .call(&mut store, (pointer as i32, 21))?,
        42
    );
    let Symbol::Data(address) = provider.symbol("counter").unwrap() else {
        panic!()
    };
    memory.write(&mut store, address as usize, &20i32.to_le_bytes())?;
    for _ in 0..20 {
        let reopened = loader.load(&mut store, &host, "libconsumer.so")?;
        assert_eq!(
            reopened
                .instance
                .get_typed_func::<(), i32>(&mut store, "direct")?
                .call(&mut store, ())?,
            52
        );
    }
    Ok(())
}
