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
                        if matches!(import.ty, wasmtime::wasmparser::TypeRef::Func(_)) {
                            function_imports
                                .push((import.module.to_owned(), import.name.to_owned()));
                        }
                    }
                }
                Payload::StartSection { func, .. } => start = Some(func),
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
            start.is_none() || start == memory_initializer,
            "start must be the exported __wasm_init_memory; constructors run after relocation"
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
