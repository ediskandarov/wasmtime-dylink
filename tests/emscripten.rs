#![cfg(feature = "emscripten")]
use wasmtime_dylink::{
    Arena,
    emscripten::{self, Emscripten},
    wasmtime::*,
};

#[derive(Default)]
struct State {
    adapter: Option<Emscripten<State>>,
    requests: Vec<String>,
    available: bool,
}
const PLUGIN: &str = r#"(module
  (@dylink.0 (mem-info (memory 16 4) (table 1 0)))
  (import "env" "memory" (memory 1))
  (import "env" "__memory_base" (global $base i32))
  (import "env" "__indirect_function_table" (table 1 funcref))
  (import "env" "__table_base" (global $table i32))
  (data (global.get $base) "\2a\00\00\00")
  (global (export "value") i32 (i32.const 0))
  (func $answer (export "answer") (result i32) global.get $base i32.load)
  (elem (global.get $table) $answer))"#;

fn setup(root: &str) -> (Store<State>, Instance, Memory) {
    let engine = Engine::default();
    let mut store = Store::new(&engine, State::default());
    let mut host = Linker::<State>::new(&engine);
    emscripten::add_to_linker(&mut host, |s| &mut s.adapter).unwrap();
    let bytes = wat::parse_str(r#"(module
      (import "env" "_dlopen_js" (func $open (param i32) (result i32)))
      (import "env" "_dlsym_js" (func $sym (param i32 i32 i32) (result i32)))
      (memory (export "memory") 2 16)
      (table (export "__indirect_function_table") 1 128 funcref)
      (global $heap (mut i32) (i32.const 8192))
      (func (export "malloc") (param $size i32) (result i32) (local $p i32)
        global.get $heap local.tee $p local.get $size i32.add i32.const 15 i32.add i32.const -16 i32.and global.set $heap local.get $p)
      (func (export "free") (param i32))
      (func (export "__dl_seterr") (param $format i32) (param $args i32)
        i32.const 16 local.get $format i32.store i32.const 20 local.get $args i32.store)
      (func (export "open") (param i32) (result i32) local.get 0 call $open)
      (func (export "sym") (param i32 i32 i32) (result i32) local.get 0 local.get 1 local.get 2 call $sym))"#).unwrap();
    let main = host
        .instantiate(&mut store, &Module::from_binary(&engine, &bytes).unwrap())
        .unwrap();
    let memory = main.get_memory(&mut store, "memory").unwrap();
    let root = wat::parse_str(root).unwrap();
    store.data_mut().adapter = Some(
        Emscripten::new(
            &mut store,
            main,
            host,
            Arena {
                memory: 65536..131072,
                table: 1..128,
            },
            move |store, name| {
                store.data_mut().requests.push(name.to_owned());
                match name {
                    "root" => Ok(root.clone()),
                    "dep" => Ok(wat::parse_str(PLUGIN)?),
                    "later" if store.data().available => Ok(wat::parse_str(PLUGIN)?),
                    _ => bail!("missing {name}"),
                }
            },
        )
        .unwrap(),
    );
    (store, main, memory)
}
fn u32_at(store: &Store<State>, memory: Memory, address: usize) -> u32 {
    u32::from_le_bytes(memory.data(store)[address..address + 4].try_into().unwrap())
}
fn write_u32(store: &mut Store<State>, memory: Memory, address: usize, value: u32) {
    memory.write(store, address, &value.to_le_bytes()).unwrap();
}
fn descriptor(store: &mut Store<State>, memory: Memory, name: &str, flags: u32) {
    memory.write(&mut *store, 256, &[0; 36]).unwrap();
    write_u32(store, memory, 260, flags);
    memory
        .write(&mut *store, 292, format!("{name}\0").as_bytes())
        .unwrap();
}
fn open(store: &mut Store<State>, main: Instance, handle: i32) -> Result<i32> {
    main.get_typed_func::<i32, i32>(&mut *store, "open")?
        .call(store, handle)
}
fn sym(
    store: &mut Store<State>,
    main: Instance,
    memory: Memory,
    handle: i32,
    name: &str,
) -> Result<i32> {
    memory.write(&mut *store, 512, format!("{name}\0").as_bytes())?;
    main.get_typed_func::<(i32, i32, i32), i32>(&mut *store, "sym")?
        .call(store, (handle, 512, 600))
}

#[test]
fn fills_dso_fields_and_returns_stable_function_and_data_addresses() {
    let (mut store, main, memory) = setup(PLUGIN);
    descriptor(&mut store, memory, "root", 2);
    assert_eq!(open(&mut store, main, 256).unwrap(), 1);
    assert_eq!(memory.data(&store)[264], 1);
    assert_eq!(u32_at(&store, memory, 268), 65536);
    assert_eq!(u32_at(&store, memory, 272), 16);
    assert_eq!(u32_at(&store, memory, 276), 1);
    assert_eq!(u32_at(&store, memory, 280), 1);
    assert_eq!(sym(&mut store, main, memory, 256, "value").unwrap(), 65536);
    assert_eq!(sym(&mut store, main, memory, 256, "answer").unwrap(), 1);
    assert_eq!(u32_at(&store, memory, 600), u32::MAX);
    assert_eq!(open(&mut store, main, 256).unwrap(), 1);
    assert_eq!(store.data().requests, ["root"]);
    assert_eq!(sym(&mut store, main, memory, 0, "answer").unwrap(), 0);
}

#[test]
fn resolves_dependency_bytes_and_symbols_from_handle() {
    let root = r#"(module (@dylink.0 (mem-info) (needed "dep")))"#;
    let (mut store, main, memory) = setup(root);
    descriptor(&mut store, memory, "root", 258);
    assert_eq!(open(&mut store, main, 256).unwrap(), 1);
    assert_eq!(store.data().requests, ["root", "dep"]);
    assert_ne!(sym(&mut store, main, memory, 256, "answer").unwrap(), 0);
    assert_ne!(sym(&mut store, main, memory, 0, "answer").unwrap(), 0);
}

#[test]
fn inline_file_bytes_bypass_resolver_but_dependencies_use_it() {
    let (mut store, main, memory) = setup(PLUGIN);
    let bytes = wat::parse_str(r#"(module (@dylink.0 (mem-info) (needed "dep")))"#).unwrap();
    descriptor(&mut store, memory, "inline", 2);
    memory.write(&mut store, 32000, &bytes).unwrap();
    write_u32(&mut store, memory, 284, 32000);
    write_u32(&mut store, memory, 288, bytes.len() as u32);
    assert_eq!(open(&mut store, main, 256).unwrap(), 1);
    assert_eq!(store.data().requests, ["dep"]);
    assert_ne!(sym(&mut store, main, memory, 256, "answer").unwrap(), 0);
}

#[test]
fn missing_bytes_are_retryable_and_errors_are_not_format_strings() {
    let (mut store, main, memory) = setup(PLUGIN);
    descriptor(&mut store, memory, "missing%s%n", 2);
    assert_eq!(open(&mut store, main, 256).unwrap(), 0);
    let format = u32_at(&store, memory, 16) as usize;
    assert_eq!(&memory.data(&store)[format..format + 3], b"%s\0");
    let args = u32_at(&store, memory, 20) as usize;
    let message = u32_at(&store, memory, args) as usize;
    let text = &memory.data(&store)[message..];
    let end = text.iter().position(|b| *b == 0).unwrap();
    assert!(
        std::str::from_utf8(&text[..end])
            .unwrap()
            .contains("missing%s%n")
    );
    descriptor(&mut store, memory, "later", 2);
    assert_eq!(open(&mut store, main, 256).unwrap(), 0);
    assert!(
        !store
            .data()
            .adapter
            .as_ref()
            .unwrap()
            .loader()
            .is_poisoned()
    );
    store.data_mut().available = true;
    assert_eq!(open(&mut store, main, 256).unwrap(), 1);
}

#[test]
fn malformed_pointers_flags_and_unknown_handles_fail_cleanly() {
    let (mut store, main, memory) = setup(PLUGIN);
    for pointer in [0, -1, 131071] {
        assert_eq!(open(&mut store, main, pointer).unwrap(), 0);
    }
    for flags in [0, 8, 0x8000_0002, 6] {
        descriptor(&mut store, memory, "root", flags);
        assert_eq!(open(&mut store, main, 256).unwrap(), 0);
    }
    assert!(store.data().requests.is_empty());
    descriptor(&mut store, memory, "root", 2);
    memory.write(&mut store, 292, &[b'a'; 4096]).unwrap();
    assert_eq!(open(&mut store, main, 256).unwrap(), 0);
    assert_eq!(sym(&mut store, main, memory, 123, "answer").unwrap(), 0);
    assert_eq!(sym(&mut store, main, memory, -1, "answer").unwrap(), 0);
    assert!(store.data().adapter.is_some());
}

#[test]
fn constructor_failure_traps_and_preserves_poisoned_adapter_for_inspection() {
    let (mut store, main, memory) =
        setup(r#"(module (@dylink.0 (mem-info)) (func (export "__wasm_call_ctors") unreachable))"#);
    descriptor(&mut store, memory, "root", 2);
    let error = open(&mut store, main, 256).unwrap_err();
    assert!(format!("{error:#}").contains("discard this store"));
    assert!(
        store
            .data()
            .adapter
            .as_ref()
            .unwrap()
            .loader()
            .is_poisoned()
    );
    assert_eq!(memory.data(&store)[264], 0);
}
