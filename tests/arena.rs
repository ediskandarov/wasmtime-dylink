use wasmtime_dylink::{ArenaRequirements, Loader, Metadata, Symbol, wasmtime::*};

fn binary(text: &str) -> Vec<u8> {
    wat::parse_str(text).unwrap()
}

#[test]
fn reservation_loads_every_library_and_resolves_export_and_got_pointers() -> Result<()> {
    let bytes = binary(
        r#"(module
      (@dylink.0 (mem-info (memory 16 4) (table 1 2)))
      (import "env" "__indirect_function_table" (table 1 funcref))
      (import "env" "__table_base" (global $base i32))
      (import "GOT.func" "host" (global $host (mut i32)))
      (func $resident)
      (elem (global.get $base) $resident)
      (func (export "entry") (result i32) global.get $host))"#,
    );
    let metadata = Metadata::parse(&bytes)?;
    let mut requirements = ArenaRequirements::default();
    for _ in 0..37 {
        requirements.add_module(&metadata)?;
    }
    let engine = Engine::default();
    let mut store = Store::new(&engine, ());
    let memory = Memory::new(&mut store, MemoryType::new(1, None))?;
    let table = Table::new(
        &mut store,
        TableType::new(RefType::FUNCREF, 1, None),
        Ref::Func(None),
    )?;
    // Deliberately unaligned bases: callers need not know module alignments.
    let mut loader = Loader::new(&mut store, memory, table, requirements.at(4097, 1)?)?;
    let host_function = Func::wrap(&mut store, || 123i32);
    loader.define("host", Symbol::Function(host_function))?;
    let linker = Linker::new(&engine);
    for i in 0..37 {
        loader.register(format!("module{i}"), &bytes)?;
    }
    for i in (0..37).rev() {
        let name = format!("module{i}");
        let library = loader.load_local(&mut store, &linker, &name)?;
        assert_eq!(library.memory_base % 16, 0);
        assert_eq!(library.table_base % 4, 0);
        let Symbol::Function(entry) = library.symbol("entry").unwrap() else {
            panic!()
        };
        let pointer = loader.function_pointer(&mut store, entry)?;
        assert_ne!(pointer, 0);
        let got = entry.typed::<(), u32>(&store)?.call(&mut store, ())?;
        assert_eq!(got, loader.function_pointer(&mut store, host_function)?);
        let reopened = loader.load_local(&mut store, &linker, &name)?;
        assert_eq!(reopened.table_base, library.table_base);
        assert_eq!(pointer, loader.function_pointer(&mut store, entry)?);
    }
    Ok(())
}

#[test]
fn zero_segment_modules_still_reserve_exported_function_pointers() -> Result<()> {
    let metadata = Metadata::parse(&binary(
        r#"(module (@dylink.0 (mem-info))
      (func (export "entry")))"#,
    ))?;
    let mut requirements = ArenaRequirements::default();
    requirements.add_module(&metadata)?;
    assert_eq!(requirements.memory_bytes, 0);
    assert_eq!(requirements.table_entries, 1);
    Ok(())
}

#[test]
fn invalid_or_overflowing_reservations_fail_without_partial_updates() -> Result<()> {
    let mut metadata = Metadata::default();
    metadata.layout.memory_size = 16;
    metadata.layout.table_size = 1;
    let mut requirements = ArenaRequirements {
        memory_bytes: 0,
        table_entries: u32::MAX,
    };
    let before = requirements;
    assert!(requirements.add_module(&metadata).is_err());
    assert_eq!(requirements, before);
    requirements = ArenaRequirements::default();
    metadata.layout.memory_alignment = 32;
    assert!(requirements.add_module(&metadata).is_err());
    assert_eq!(requirements, ArenaRequirements::default());
    let requirements = ArenaRequirements {
        memory_bytes: 16,
        table_entries: 16,
    };
    assert!(requirements.at(u32::MAX, 1).is_err());
    assert!(requirements.at(1, u32::MAX).is_err());
    assert!(requirements.at(0, 1).is_err());
    assert!(requirements.at(1, 0).is_err());
    Ok(())
}
