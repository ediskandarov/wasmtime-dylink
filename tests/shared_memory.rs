use wasmtime_dylink::{Arena, LinearMemory, Loader, Metadata, wasmtime::*};

// wasm-ld emits the guard outside the dispatch blocks. Binaryen inlines it
// into cmpxchg and may fold the module's global relocations into the start.
fn library(inlined: bool) -> String {
    let addr = "global.get $base i32.const 12 i32.add";
    let relocate = "global.get $base i32.const 4 i32.add global.set $data";
    let (setup, guard) = if inlined {
        (relocate.to_owned(), format!("{addr} local.tee 0"))
    } else {
        (format!("{addr} local.set 0"), "local.get 0".into())
    };
    let start = if inlined {
        "(start $initialize)"
    } else {
        "(func $start call $reloc call $initialize) (start $start)"
    };
    format!(
        r#"(module
        (@dylink.0 (mem-info (memory 16 2)))
        (import "env" "memory" (memory 1 4 shared))
        (import "env" "__memory_base" (global $base i32))
        (global $data (mut i32) (i32.const 0))
        (data $message "GAME")
        (func $reloc {relocate})
        (func $initialize (local i32)
            {setup}
            block block block
                {guard} i32.const 0 i32.const 1 i32.atomic.rmw.cmpxchg
                br_table 0 1 2
            end
                i32.const 4 global.get $base i32.add i32.const 0 i32.const 4 memory.init $message
                global.get $base i32.const 0 i32.const 4 memory.fill
                local.get 0 i32.const 2 i32.atomic.store
                local.get 0 i32.const -1 memory.atomic.notify drop br 1
            end
                local.get 0 i32.const 1 i64.const -1 memory.atomic.wait32 drop
            end
            data.drop $message)
        {start}
        (func (export "read") (result i32) global.get $data i32.load))"#
    )
}

#[test]
fn shared_data_loads_once_with_wrapped_and_inlined_starts() -> Result<()> {
    let mut config = Config::new();
    config.wasm_threads(true).shared_memory(true);
    let engine = Engine::new(&config)?;
    let mut store = Store::new(&engine, ());
    let memory = SharedMemory::new(&engine, MemoryType::shared(1, 4))?;
    let view = LinearMemory::from(memory.clone());
    let table = Table::new(
        &mut store,
        TableType::new(RefType::FUNCREF, 1, Some(8)),
        Ref::Func(None),
    )?;
    let mut loader = Loader::new(
        &mut store,
        memory,
        table,
        Arena {
            memory: 65536..131072,
            table: 1..8,
        },
    )?;
    let private =
        wat::parse_str(r#"(module (@dylink.0 (mem-info)) (import "env" "memory" (memory 1)))"#)?;
    assert!(loader.register("wrapped", &private).is_err());
    let linker = Linker::new(&engine);
    for (name, inline) in [("wrapped", false), ("inline", true)] {
        loader.register(name, &wat::parse_str(library(inline))?)?;
        let lib = loader.load(&mut store, &linker, name)?;
        let base = lib.memory_base as usize;
        assert_eq!(
            lib.instance
                .get_typed_func::<(), i32>(&mut store, "read")?
                .call(&mut store, ())?,
            i32::from_le_bytes(*b"GAME")
        );
        assert_eq!(
            view.copy(&store, base, 16)?,
            b"\0\0\0\0GAME\0\0\0\0\x02\0\0\0"
        );
        view.write(&mut store, base + 4, b"TEST")?;
        assert_eq!(
            loader.load(&mut store, &linker, name)?.memory_base as usize,
            base
        );
        assert_eq!(view.copy(&store, base + 4, 4)?, b"TEST");
    }
    Ok(())
}

#[test]
fn rejects_invalid_shared_initialization_before_loading() {
    let source = library(false);
    for (from, to) in [
        ("br_table 0 1 2", "br_table 0 2 1"),
        ("i32.const 12 i32.add", "i32.const 16 i32.add"),
        ("i32.const 12 i32.add", "i32.const 11 i32.add"),
        ("(memory 16 2)", "(memory 16 0)"),
        (
            "i32.const 4 global.get $base",
            "i32.const 12 global.get $base",
        ),
        ("i32.const 4 memory.init", "i32.const 5 memory.init"),
        ("i32.const 4 memory.fill", "i32.const 17 memory.fill"),
        (
            "local.get 0 i32.const 2 i32.atomic.store",
            "global.get $base i32.const 2 i32.atomic.store",
        ),
        (
            "call $reloc call $initialize",
            "call $initialize call $reloc",
        ),
        (
            "(start $start)",
            "(func $other (export \"__wasm_init_memory\") nop) (start $other)",
        ),
        (
            "(data $message \"GAME\")",
            "(data $message (global.get $base) \"GAME\")",
        ),
    ] {
        assert!(source.contains(from));
        let bytes = wat::parse_str(source.replace(from, to)).unwrap();
        assert!(Metadata::parse(&bytes).is_err(), "accepted {to}");
    }
}

#[test]
fn optimized_zero_stores_obey_the_bounds_of_the_initialization_region() {
    let source = library(true);
    let fill = "global.get $base i32.const 0 i32.const 4 memory.fill";
    assert!(source.contains(fill));
    for (store, accepted) in [
        ("i32.const 0 i32.store offset=8 align=1", true),
        ("i64.const 0 i64.store offset=4 align=1", true),
        ("i32.const 1 i32.store offset=8", false),
        ("i32.const 0 i32.store offset=9", false),
        ("i64.const 0 i64.store offset=24", false),
    ] {
        let bytes =
            wat::parse_str(source.replace(fill, &format!("global.get $base {store}"))).unwrap();
        assert_eq!(Metadata::parse(&bytes).is_ok(), accepted, "{store}");
    }
}
