use std::collections::HashMap;

use wasmtime::wasmparser::{Dylink0Subsection, KnownCustom, Parser, Payload, SymbolFlags};
use wasmtime::{Result, bail, ensure, error::Context};

/// The wasm32 memory and function-table reservation requested by a library.
/// Alignments are powers of two, as encoded in `dylink.0`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Layout {
    pub memory_size: u32,
    pub memory_alignment: u32,
    pub table_size: u32,
    pub table_alignment: u32,
}

/// Parsed dynamic-linking metadata. The loader resolves dependency names exactly;
/// it does not read the filesystem or expand runtime search paths.
#[derive(Clone, Debug, Default)]
pub struct Metadata {
    pub layout: Layout,
    pub needed: Vec<String>,
    pub runtime_paths: Vec<String>,
    pub(crate) function_reexports: HashMap<String, (String, String)>,
    pub(crate) exports: HashMap<String, SymbolFlags>,
    pub(crate) imports: HashMap<(String, String), SymbolFlags>,
}

impl Metadata {
    /// Read and validate the subset of the ABI supported by this loader.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let mut metadata = Self::default();
        let mut found = false;
        let mut layout_found = false;
        let mut target_found = false;
        let mut start = None;
        let mut memory_initializer = None;
        let mut function_imports = Vec::new();
        let mut imported_globals = 0u32;
        let mut bases = Vec::new();
        let mut local_globals = Vec::new();
        let mut code_index = 0u32;
        let mut start_body = None;
        for payload in Parser::new(0).parse_all(bytes) {
            match payload.context("invalid WebAssembly binary")? {
                Payload::Version { encoding, .. } => {
                    ensure!(
                        encoding == wasmtime::wasmparser::Encoding::Module,
                        "components are not dylink modules"
                    );
                }
                Payload::MemorySection(_) | Payload::TableSection(_) => {
                    bail!("dynamic libraries must import their memory and table");
                }
                Payload::ImportSection(imports) => {
                    for import in imports.into_imports() {
                        let import = import?;
                        match import.ty {
                            wasmtime::wasmparser::TypeRef::Memory(ty) => {
                                ensure!(
                                    !ty.memory64 && !ty.shared,
                                    "only non-shared wasm32 memory is supported"
                                );
                                ensure!(
                                    import.module == "env" && import.name == "memory",
                                    "memory must be imported as env.memory"
                                );
                            }
                            wasmtime::wasmparser::TypeRef::Table(ty) => {
                                ensure!(!ty.table64, "only wasm32 tables are supported");
                                ensure!(
                                    import.module == "env"
                                        && import.name == "__indirect_function_table",
                                    "table must be imported as env.__indirect_function_table"
                                );
                            }
                            _ => (),
                        }
                        if let wasmtime::wasmparser::TypeRef::Global(ty) = import.ty {
                            if import.module == "env"
                                && matches!(import.name, "__memory_base" | "__table_base")
                                && !ty.mutable
                                && ty.content_type == wasmtime::wasmparser::ValType::I32
                            {
                                bases.push((imported_globals, import.name == "__memory_base"));
                            }
                            imported_globals += 1;
                        }
                        if matches!(import.ty, wasmtime::wasmparser::TypeRef::Func(_)) {
                            function_imports
                                .push((import.module.to_owned(), import.name.to_owned()));
                        }
                    }
                }
                Payload::StartSection { func, .. } => start = Some(func),
                Payload::GlobalSection(globals) => {
                    for global in globals {
                        let ty = global?.ty;
                        local_globals.push(
                            ty.mutable && ty.content_type == wasmtime::wasmparser::ValType::I32,
                        );
                    }
                }
                Payload::CodeSectionEntry(body) => {
                    if start == Some(function_imports.len() as u32 + code_index) {
                        start_body = Some(body);
                    }
                    code_index += 1;
                }
                Payload::ExportSection(exports) => {
                    for export in exports {
                        let export = export?;
                        if export.kind == wasmtime::wasmparser::ExternalKind::Func
                            && let Some(import) = function_imports.get(export.index as usize)
                        {
                            metadata
                                .function_reexports
                                .insert(export.name.to_owned(), import.clone());
                        }
                        if export.name == "__wasm_init_memory"
                            && export.kind == wasmtime::wasmparser::ExternalKind::Func
                        {
                            memory_initializer = Some(export.index);
                        }
                    }
                }
                Payload::CustomSection(section) if section.name() == "dylink.0" => {
                    ensure!(!found, "duplicate dylink.0 section");
                    found = true;
                    let KnownCustom::Dylink0(reader) = section.as_known() else {
                        bail!("invalid dylink.0 section");
                    };
                    for subsection in reader {
                        match subsection? {
                            Dylink0Subsection::MemInfo(info) => {
                                ensure!(!layout_found, "duplicate dylink memory layout");
                                layout_found = true;
                                ensure!(
                                    info.memory_alignment < 32 && info.table_alignment < 32,
                                    "alignment exceeds wasm32 address space"
                                );
                                metadata.layout = Layout {
                                    memory_size: info.memory_size,
                                    memory_alignment: info.memory_alignment,
                                    table_size: info.table_size,
                                    table_alignment: info.table_alignment,
                                };
                            }
                            Dylink0Subsection::Needed(names) => {
                                metadata.needed.extend(names.into_iter().map(str::to_owned))
                            }
                            Dylink0Subsection::RuntimePath(paths) => metadata
                                .runtime_paths
                                .extend(paths.into_iter().map(str::to_owned)),
                            Dylink0Subsection::ExportInfo(exports) => {
                                for export in exports {
                                    validate_flags(export.flags)?;
                                    ensure!(
                                        metadata
                                            .exports
                                            .insert(export.name.to_owned(), export.flags)
                                            .is_none(),
                                        "duplicate export metadata: {}",
                                        export.name
                                    );
                                }
                            }
                            Dylink0Subsection::ImportInfo(imports) => {
                                for import in imports {
                                    validate_flags(import.flags)?;
                                    ensure!(
                                        metadata
                                            .imports
                                            .insert(
                                                (import.module.to_owned(), import.field.to_owned()),
                                                import.flags
                                            )
                                            .is_none(),
                                        "duplicate import metadata: {}.{}",
                                        import.module,
                                        import.field
                                    );
                                }
                            }
                            Dylink0Subsection::Unknown { ty: 6, data, .. } => {
                                ensure!(!target_found, "duplicate target architecture");
                                target_found = true;
                                let mut reader = wasmtime::wasmparser::BinaryReader::new(data, 0);
                                ensure!(
                                    reader.read_string()? == "wasm32",
                                    "only wasm32 is supported"
                                );
                                ensure!(reader.eof(), "trailing target architecture bytes");
                            }
                            Dylink0Subsection::Unknown { ty, .. } => {
                                bail!("unsupported dylink subsection {ty}")
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        ensure!(found, "missing dylink.0 section");
        ensure!(
            start.is_none()
                || start == memory_initializer
                || start_body.is_some_and(|body| module_initializer(
                    body,
                    &bases,
                    imported_globals,
                    &local_globals,
                    metadata.layout.memory_size
                )),
            "start must initialize memory or module-local base globals; constructors run after relocation"
        );
        Ok(metadata)
    }

    pub(crate) fn weak_import(&self, module: &str, name: &str) -> bool {
        self.imports
            .get(&(module.to_owned(), name.to_owned()))
            .is_some_and(|f| f.contains(SymbolFlags::BINDING_WEAK))
    }

    pub(crate) fn export_flags(&self, name: &str) -> SymbolFlags {
        self.exports
            .get(name)
            .copied()
            .unwrap_or_else(SymbolFlags::empty)
    }
}

fn validate_flags(flags: SymbolFlags) -> Result<()> {
    ensure!(
        !flags.intersects(SymbolFlags::TLS | SymbolFlags::ABSOLUTE | SymbolFlags::BINDING_LOCAL),
        "unsupported symbol flags: {flags:?}"
    );
    Ok(())
}

// Binaryen combines base-relative globals and BSS clearing in a start function.
// Evaluate only address arithmetic, local assignments and bounded zero-fill.
// Calls, GOT reads and writes outside the module must wait for relocation.
fn module_initializer(
    body: wasmtime::wasmparser::FunctionBody<'_>,
    bases: &[(u32, bool)],
    imported_globals: u32,
    globals: &[bool],
    memory_size: u32,
) -> bool {
    use wasmtime::wasmparser::{Operator, ValType};
    #[derive(Clone, Copy)]
    struct Value {
        base: Option<u32>,
        offset: u32,
    }
    let Ok(declarations) = body.get_locals_reader() else {
        return false;
    };
    let mut local_count = 0u32;
    for declaration in declarations {
        let Ok((count, ValType::I32)) = declaration else {
            return false;
        };
        let Some(total) = local_count.checked_add(count) else {
            return false;
        };
        local_count = total;
    }
    let mut locals = HashMap::new();
    let mut stack: Vec<Value> = Vec::new();
    let Ok(mut ops) = body.get_operators_reader() else {
        return false;
    };
    let mut initialized = false;
    while let Ok(op) = ops.read() {
        match op {
            Operator::End => return initialized && stack.is_empty() && ops.eof(),
            Operator::GlobalGet { global_index }
                if bases.iter().any(|(index, _)| *index == global_index) =>
            {
                stack.push(Value {
                    base: Some(global_index),
                    offset: 0,
                })
            }
            Operator::I32Const { value } => stack.push(Value {
                base: None,
                offset: value as u32,
            }),
            Operator::I64Const { value: 0 } => stack.push(Value {
                base: None,
                offset: 0,
            }),
            Operator::V128Const { value } if value.i128() == 0 => stack.push(Value {
                base: None,
                offset: 0,
            }),
            Operator::I32Store { memarg }
            | Operator::I32Store8 { memarg }
            | Operator::I32Store16 { memarg }
            | Operator::I64Store { memarg }
            | Operator::I64Store8 { memarg }
            | Operator::I64Store16 { memarg }
            | Operator::I64Store32 { memarg }
            | Operator::V128Store { memarg } => {
                let width = match op {
                    Operator::I32Store8 { .. } | Operator::I64Store8 { .. } => 1,
                    Operator::I32Store16 { .. } | Operator::I64Store16 { .. } => 2,
                    Operator::I32Store { .. } | Operator::I64Store32 { .. } => 4,
                    Operator::I64Store { .. } => 8,
                    _ => 16,
                };
                let (Some(value), Some(destination)) = (stack.pop(), stack.pop()) else {
                    return false;
                };
                if memarg.memory != 0
                    || value.base.is_some()
                    || value.offset != 0
                    || !bases
                        .iter()
                        .any(|(index, memory)| *memory && destination.base == Some(*index))
                    || u64::from(destination.offset)
                        .checked_add(memarg.offset)
                        .and_then(|n| n.checked_add(width))
                        .is_none_or(|end| end > u64::from(memory_size))
                {
                    return false;
                }
                initialized = true;
            }
            Operator::I32Add => {
                let (Some(right), Some(left)) = (stack.pop(), stack.pop()) else {
                    return false;
                };
                if right.base.is_some() && left.base.is_some() {
                    return false;
                }
                let Some(offset) = left.offset.checked_add(right.offset) else {
                    return false;
                };
                stack.push(Value {
                    base: left.base.or(right.base),
                    offset,
                });
            }
            Operator::LocalGet { local_index } if local_index < local_count => {
                stack.push(*locals.get(&local_index).unwrap_or(&Value {
                    base: None,
                    offset: 0,
                }))
            }
            Operator::LocalSet { local_index } | Operator::LocalTee { local_index }
                if local_index < local_count =>
            {
                let Some(value) = stack.pop() else {
                    return false;
                };
                locals.insert(local_index, value);
                if matches!(op, Operator::LocalTee { .. }) {
                    stack.push(value);
                }
            }
            Operator::GlobalSet { global_index } => {
                let Some(index) = global_index.checked_sub(imported_globals) else {
                    return false;
                };
                if globals.get(index as usize) != Some(&true) || stack.pop().is_none() {
                    return false;
                }
                initialized = true;
            }
            Operator::MemoryFill { mem: 0 } => {
                let (Some(length), Some(byte), Some(destination)) =
                    (stack.pop(), stack.pop(), stack.pop())
                else {
                    return false;
                };
                if length.base.is_some()
                    || byte.base.is_some()
                    || byte.offset != 0
                    || !bases
                        .iter()
                        .any(|(index, memory)| *memory && destination.base == Some(*index))
                    || destination
                        .offset
                        .checked_add(length.offset)
                        .is_none_or(|end| end > memory_size)
                {
                    return false;
                }
                initialized = true;
            }
            _ => return false,
        }
    }
    false
}
