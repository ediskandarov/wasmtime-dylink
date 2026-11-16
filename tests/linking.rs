use wasmtime_dylink::{Arena, Loader, Metadata, Symbol, wasmtime::*};

fn setup() -> (Store<()>, Linker<()>, Loader, Memory, Table) {
    let engine = Engine::default();
    let mut store = Store::new(&engine, ());
    let memory = Memory::new(&mut store, MemoryType::new(1, Some(16))).unwrap();
    let table = Table::new(
        &mut store,
        TableType::new(RefType::FUNCREF, 1, Some(128)),
        Ref::Func(None),
    )
    .unwrap();
    let loader = Loader::new(
        &mut store,
        memory,
        table,
        Arena {
            memory: 4096..16 * 65536,
            table: 1..128,
        },
    )
    .unwrap();
    (store, Linker::new(&engine), loader, memory, table)
}

fn binary(text: &str) -> Vec<u8> {
    wat::parse_str(text).unwrap()
}

fn call(store: &mut Store<()>, library: &wasmtime_dylink::Library, name: &str) -> i32 {
    library
        .instance
        .get_typed_func::<(), i32>(&mut *store, name)
        .unwrap()
        .call(store, ())
        .unwrap()
}

#[test]
fn relocates_data_zeroes_bss_and_runs_constructor_once() {
    let (mut store, host, mut loader, memory, _) = setup();
    memory.data_mut(&mut store)[4096..8192].fill(0xa5);
    let bytes = binary(
        r#"(module
      (@dylink.0 (mem-info (memory 32 4)))
      (import "env" "memory" (memory 1))
      (import "env" "__memory_base" (global $base i32))
      (import "GOT.mem" "counter" (global $counter (mut i32)))
      (global (export "counter") i32 (i32.const 0))
      (data (global.get $base) "\07\00\00\00")
      (func (export "__wasm_apply_data_relocs")
        global.get $base i32.const 8 i32.add global.get $counter i32.store)
      (func (export "__wasm_call_ctors")
        global.get $counter global.get $counter i32.load i32.const 1 i32.add i32.store)
      (func (export "read") (result i32) global.get $counter i32.load)
      (func (export "bss") (result i32) global.get $base i32.const 16 i32.add i32.load)
      (func (export "relocated") (result i32) global.get $base i32.const 8 i32.add i32.load i32.load))"#,
    );
    loader.register("counter", &bytes).unwrap();
    let library = loader.load(&mut store, &host, "counter").unwrap();
    assert_eq!(library.memory_base % 16, 0);
    assert_eq!(call(&mut store, &library, "read"), 8);
    assert_eq!(call(&mut store, &library, "bss"), 0);
    assert_eq!(call(&mut store, &library, "relocated"), 8);
    let second = loader.load(&mut store, &host, "counter").unwrap();
    assert_eq!(call(&mut store, &second, "read"), 8);
    assert_eq!(second.memory_base, library.memory_base);
    assert_eq!(memory.data(&store)[4095], 0);
    assert_eq!(memory.data(&store)[4128], 0xa5);
}

#[test]
fn dependencies_share_data_direct_calls_and_function_pointers() {
    let (mut store, host, mut loader, _, table) = setup();
    loader
        .register(
            "provider",
            &binary(
                r#"(module
      (@dylink.0 (mem-info (memory 4 2) (table 1 0)))
      (import "env" "memory" (memory 1))
      (import "env" "__memory_base" (global $base i32))
      (import "env" "__table_base" (global $table i32))
      (import "env" "__indirect_function_table" (table 1 funcref))
      (global (export "value") i32 (i32.const 0))
      (data (global.get $base) "\29\00\00\00")
      (func $answer (export "answer") (result i32) i32.const 42)
      (elem (global.get $table) $answer))"#,
            ),
        )
        .unwrap();
    loader
        .register(
            "consumer",
            &binary(
                r#"(module
      (@dylink.0 (mem-info (memory 1 4)) (needed "provider"))
      (type $getter (func (result i32)))
      (import "env" "memory" (memory 1))
      (import "env" "__indirect_function_table" (table 1 funcref))
      (import "env" "answer" (func $answer (result i32)))
      (import "GOT.func" "answer" (global $address (mut i32)))
      (import "GOT.mem" "value" (global $value (mut i32)))
      (func (export "direct") (result i32) call $answer)
      (func (export "indirect") (result i32) global.get $address call_indirect (type $getter))
      (func (export "data") (result i32) global.get $value i32.load))"#,
            ),
        )
        .unwrap();
    let library = loader.load(&mut store, &host, "consumer").unwrap();
    assert_eq!(call(&mut store, &library, "direct"), 42);
    assert_eq!(call(&mut store, &library, "indirect"), 42);
    assert_eq!(call(&mut store, &library, "data"), 41);
    let provider = loader.load(&mut store, &host, "provider").unwrap();
    let Symbol::Function(function) = provider.symbol("answer").unwrap() else {
        panic!()
    };
    assert_eq!(
        loader.function_pointer(&mut store, function).unwrap(),
        provider.table_base
    );
    assert_eq!(table.size(&store), 2);
}

#[test]
fn cyclic_dependencies_bind_before_initialization() {
    let (mut store, host, mut loader, _, _) = setup();
    loader
        .register(
            "a",
            &binary(
                r#"(module
      (@dylink.0 (mem-info) (needed "b"))
      (import "env" "b" (func $b (result i32)))
      (func (export "a") (result i32) i32.const 41)
      (func (export "run") (result i32) call $b))"#,
            ),
        )
        .unwrap();
    loader
        .register(
            "b",
            &binary(
                r#"(module
      (@dylink.0 (mem-info) (needed "a"))
      (import "env" "a" (func $a (result i32)))
      (func (export "b") (result i32) call $a i32.const 1 i32.add))"#,
            ),
        )
        .unwrap();
    let library = loader.load(&mut store, &host, "a").unwrap();
    assert_eq!(call(&mut store, &library, "run"), 42);
}

#[test]
fn all_relocations_precede_any_constructor() {
    let (mut store, host, mut loader, _, _) = setup();
    loader
        .register(
            "dep",
            &binary(
                r#"(module
      (@dylink.0 (mem-info) (needed "root"))
      (import "env" "read_relocated" (func $read (result i32)))
      (global $result (mut i32) (i32.const 0))
      (func (export "__wasm_call_ctors") call $read global.set $result)
      (func (export "get_result") (result i32) global.get $result))"#,
            ),
        )
        .unwrap();
    loader
        .register(
            "root",
            &binary(
                r#"(module
      (@dylink.0 (mem-info) (needed "dep"))
      (import "env" "get_result" (func $get (result i32)))
      (global $value (mut i32) (i32.const 0))
      (func (export "__wasm_apply_data_relocs") i32.const 42 global.set $value)
      (func (export "read_relocated") (result i32) global.get $value)
      (func (export "run") (result i32) call $get))"#,
            ),
        )
        .unwrap();
    let library = loader.load(&mut store, &host, "root").unwrap();
    assert_eq!(call(&mut store, &library, "run"), 42);
}

#[test]
fn unresolved_weak_addresses_are_null_and_calls_trap() {
    let (mut store, host, mut loader, _, _) = setup();
    loader
        .register(
            "weak",
            &binary(
                r#"(module
      (@dylink.0 (mem-info)
        (import-info "env" "missing" binding-weak undefined)
        (import-info "GOT.func" "missing" binding-weak undefined)
        (import-info "GOT.mem" "missing_data" binding-weak undefined))
      (import "env" "missing" (func $missing))
      (import "GOT.func" "missing" (global $f (mut i32)))
      (import "GOT.mem" "missing_data" (global $d (mut i32)))
      (func (export "run") (result i32) global.get $f global.get $d i32.or)
      (func (export "trap") call $missing))"#,
            ),
        )
        .unwrap();
    let library = loader.load(&mut store, &host, "weak").unwrap();
    assert_eq!(call(&mut store, &library, "run"), 0);
    assert!(
        library
            .instance
            .get_typed_func::<(), ()>(&mut store, "trap")
            .unwrap()
            .call(&mut store, ())
            .is_err()
    );
}

#[test]
fn missing_dependency_can_be_registered_and_retried() {
    let (mut store, host, mut loader, _, _) = setup();
    loader
        .register(
            "root",
            &binary(r#"(module (@dylink.0 (mem-info) (needed "dep")))"#),
        )
        .unwrap();
    assert!(
        loader
            .load(&mut store, &host, "root")
            .unwrap_err()
            .to_string()
            .contains("unregistered dependency")
    );
    assert!(!loader.is_poisoned());
    loader
        .register("dep", &binary("(module (@dylink.0 (mem-info)))"))
        .unwrap();
    loader.load(&mut store, &host, "root").unwrap();
}

#[test]
fn missing_strong_symbols_and_failed_ctors_poison_loader() {
    for module in [
        r#"(module (@dylink.0 (mem-info)) (import "env" "missing" (func)))"#,
        r#"(module (@dylink.0 (mem-info)) (func (export "__wasm_call_ctors") unreachable))"#,
        r#"(module (@dylink.0 (mem-info (memory 2000000 0))))"#,
    ] {
        let (mut store, host, mut loader, _, _) = setup();
        loader.register("bad", &binary(module)).unwrap();
        assert!(loader.load(&mut store, &host, "bad").is_err());
        assert!(loader.is_poisoned());
        assert!(
            loader
                .load(&mut store, &host, "bad")
                .unwrap_err()
                .to_string()
                .contains("poisoned")
        );
    }
}

#[test]
fn rejects_unsupported_or_malformed_modules() {
    for module in [
        "(module)",
        "(module (@dylink.0 (mem-info (memory 1 32))))",
        "(module (@dylink.0 (mem-info)) (memory 1))",
        "(module (@dylink.0 (mem-info)) (func $start) (start $start))",
        r#"(module (@dylink.0 (mem-info) (export-info "tls" tls)))"#,
    ] {
        assert!(Metadata::parse(&binary(module)).is_err(), "{module}");
    }
    let bytes = binary("(module (@dylink.0 (mem-info (memory 8 2))))");
    for length in 0..bytes.len() {
        assert!(Metadata::parse(&bytes[..length]).is_err());
    }
}

#[test]
fn host_callbacks_work_through_got() {
    let (mut store, mut host, mut loader, _, _) = setup();
    let function = Func::wrap(&mut store, || 42i32);
    host.define(&store, "env", "answer", function).unwrap();
    loader
        .register(
            "host",
            &binary(
                r#"(module
      (@dylink.0 (mem-info))
      (type $getter (func (result i32)))
      (import "env" "__indirect_function_table" (table 1 funcref))
      (import "GOT.func" "answer" (global $f (mut i32)))
      (func (export "run") (result i32) global.get $f call_indirect (type $getter)))"#,
            ),
        )
        .unwrap();
    let library = loader.load(&mut store, &host, "host").unwrap();
    assert_eq!(call(&mut store, &library, "run"), 42);
}

#[test]
fn host_data_addresses_are_not_rebased() {
    let (mut store, host, mut loader, memory, _) = setup();
    memory.write(&mut store, 32, &42i32.to_le_bytes()).unwrap();
    loader.define("value", Symbol::Data(32)).unwrap();
    loader
        .register(
            "host",
            &binary(
                r#"(module
      (@dylink.0 (mem-info))
      (import "env" "memory" (memory 1))
      (import "GOT.mem" "value" (global $value (mut i32)))
      (func (export "run") (result i32) global.get $value i32.load))"#,
            ),
        )
        .unwrap();
    let library = loader.load(&mut store, &host, "host").unwrap();
    assert_eq!(call(&mut store, &library, "run"), 42);
}

#[test]
fn strong_symbol_supersedes_weak_within_load_group() {
    let (mut store, host, mut loader, _, _) = setup();
    for (name, value, flags) in [
        ("weak", 7, "(export-info \"answer\" binding-weak)"),
        ("strong", 42, ""),
    ] {
        loader
            .register(
                name,
                &binary(&format!(
                    r#"(module (@dylink.0 (mem-info) {flags})
          (func (export "answer") (result i32) i32.const {value}))"#
                )),
            )
            .unwrap();
    }
    loader
        .register(
            "root",
            &binary(
                r#"(module
      (@dylink.0 (mem-info) (needed "weak" "strong"))
      (import "env" "answer" (func $answer (result i32)))
      (func (export "run") (result i32) call $answer))"#,
            ),
        )
        .unwrap();
    let library = loader.load(&mut store, &host, "root").unwrap();
    assert_eq!(call(&mut store, &library, "run"), 42);
}

#[test]
fn hidden_symbol_does_not_replace_global_scope() {
    let (mut store, host, mut loader, _, _) = setup();
    loader
        .register(
            "dep",
            &binary(
                r#"(module (@dylink.0 (mem-info))
      (func (export "answer") (result i32) i32.const 7))"#,
            ),
        )
        .unwrap();
    loader
        .register(
            "root",
            &binary(
                r#"(module
      (@dylink.0 (mem-info) (needed "dep") (export-info "answer" visibility-hidden))
      (import "env" "answer" (func $answer (result i32)))
      (func (export "answer") (result i32) i32.const 42)
      (func (export "run") (result i32) call $answer))"#,
            ),
        )
        .unwrap();
    let library = loader.load(&mut store, &host, "root").unwrap();
    assert_eq!(call(&mut store, &library, "run"), 42);
    let Symbol::Function(global) = loader.symbol("answer").unwrap() else {
        panic!()
    };
    assert_eq!(
        global
            .typed::<(), i32>(&store)
            .unwrap()
            .call(&mut store, ())
            .unwrap(),
        7
    );
}

#[test]
fn unresolved_function_reexport_fails_instead_of_creating_recursive_stub() {
    let (mut store, host, mut loader, _, _) = setup();
    loader
        .register(
            "bad",
            &binary(
                r#"(module (@dylink.0 (mem-info))
      (import "env" "missing" (func $missing)) (export "missing" (func $missing)))"#,
            ),
        )
        .unwrap();
    assert!(
        loader
            .load(&mut store, &host, "bad")
            .unwrap_err()
            .to_string()
            .contains("re-export")
    );
}

#[test]
fn direct_function_types_are_checked() {
    let (mut store, host, mut loader, _, _) = setup();
    let function = Func::wrap(&mut store, |x: i64| x);
    loader
        .define("wrong_type", Symbol::Function(function))
        .unwrap();
    loader
        .register(
            "bad",
            &binary(
                r#"(module (@dylink.0 (mem-info))
      (import "env" "wrong_type" (func (param i32) (result i32))))"#,
            ),
        )
        .unwrap();
    assert!(loader.load(&mut store, &host, "bad").is_err());
}

#[test]
fn local_dependencies_can_be_reused_without_leaking_into_global_scope() {
    let (mut store, host, mut loader, _, _) = setup();
    loader.register("dep", &binary(r#"(module (@dylink.0 (mem-info)) (func (export "answer") (result i32) i32.const 42))"#)).unwrap();
    loader.register("root", &binary(r#"(module (@dylink.0 (mem-info) (needed "dep")) (import "env" "answer" (func $answer (result i32))) (func (export "run") (result i32) call $answer))"#)).unwrap();
    loader.load_local(&mut store, &host, "dep").unwrap();
    assert!(loader.symbol("answer").is_none());
    let root = loader.load_local(&mut store, &host, "root").unwrap();
    assert_eq!(call(&mut store, &root, "run"), 42);
    assert!(loader.symbol("answer").is_none());
    assert!(loader.symbol("run").is_none());
    loader.promote("root").unwrap();
    assert!(loader.symbol("answer").is_some());
    assert!(loader.symbol("run").is_some());
}

#[test]
fn optimized_base_globals_initialize_before_constructors() {
    let (mut store, host, mut loader, _, _) = setup();
    let bytes = binary(
        r#"(module
      (@dylink.0 (mem-info (memory 32 4)))
      (import "env" "__memory_base" (global $base i32))
      (global $address (mut i32) (i32.const 12))
      (func $init global.get $base i32.const 12 i32.add global.set $address)
      (start $init)
      (func (export "read") (result i32) global.get $address))"#,
    );
    loader.register("optimized", &bytes).unwrap();
    let library = loader.load(&mut store, &host, "optimized").unwrap();
    assert_eq!(
        call(&mut store, &library, "read"),
        library.memory_base as i32 + 12
    );
}

#[test]
fn base_initializer_cannot_touch_imported_state_or_call_code() {
    for (source, instructions) in [
        (
            "env",
            "global.get $base i32.const 12 i32.add global.set $external",
        ),
        (
            "GOT.mem",
            "global.get $base i32.const 12 i32.add global.set $local",
        ),
        ("env", "call $callback"),
    ] {
        let bytes = binary(&format!(
            r#"(module
          (@dylink.0 (mem-info (memory 32 4)))
          (import "{source}" "__memory_base" (global $base i32))
          (import "env" "external" (global $external (mut i32)))
          (import "env" "callback" (func $callback))
          (global $local (mut i32) (i32.const 0))
          (func $init {instructions}) (start $init))"#
        ));
        assert!(Metadata::parse(&bytes).is_err());
    }
}

#[test]
fn optimized_bss_start_stays_inside_module_memory() {
    for (offset, length, byte, accepted) in [
        (16, 16, 0, true),
        (16, 17, 0, false),
        (-1, 1, 0, false),
        (0, 1, 1, false),
    ] {
        let bytes = binary(&format!(
            r#"(module
          (@dylink.0 (mem-info (memory 32 4)))
          (import "env" "memory" (memory 1))
          (import "env" "__memory_base" (global $base i32))
          (global $address (mut i32) (i32.const 0))
          (func $init (local $address i32)
            global.get $base i32.const {offset} i32.add local.tee $address global.set $address
            local.get $address i32.const {byte} i32.const {length} memory.fill)
          (start $init))"#
        ));
        assert_eq!(Metadata::parse(&bytes).is_ok(), accepted);
    }
}

#[test]
fn optimized_zero_stores_are_bounded_like_bulk_fill() {
    for (instruction, value, offset, accepted) in [
        ("i32.store", "i32.const 0", 28, true),
        ("i64.store", "i64.const 0", 24, true),
        ("v128.store", "v128.const i32x4 0 0 0 0", 16, true),
        ("i32.store", "i32.const 0", 29, false),
        ("i64.store", "i64.const 0", 25, false),
        ("v128.store", "v128.const i32x4 0 0 0 0", 17, false),
        ("i32.store", "i32.const 1", 0, false),
    ] {
        let bytes = binary(&format!(
            r#"(module
          (@dylink.0 (mem-info (memory 32 4)))
          (import "env" "memory" (memory 1))
          (import "env" "__memory_base" (global $base i32))
          (func $init global.get $base {value} {instruction} offset={offset})
          (start $init))"#
        ));
        assert_eq!(Metadata::parse(&bytes).is_ok(), accepted);
    }
}
