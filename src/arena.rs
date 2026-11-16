use crate::{Arena, Metadata};
use wasmtime::{Result, ensure, error::Context};

/// Conservative reservation for a known set of libraries, in any load order.
/// Includes worst-case alignment padding, exported function pointers (including
/// dlsym) and GOT.func bindings. Count every dependency once. Reopening a library
/// does not require another reservation. Symbols already in element segments or
/// shared across libraries may make actual consumption smaller.
///
/// Arbitrary additional host functions passed to Loader::function_pointer are
/// outside this inventory; reserve extra table entries for them explicitly.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ArenaRequirements {
    pub memory_bytes: u32,
    pub table_entries: u32,
}

impl ArenaRequirements {
    /// Add one library's requirements without compiling it. On overflow, leaves
    /// the accumulated reservation unchanged.
    pub fn add_module(&mut self, metadata: &Metadata) -> Result<()> {
        fn size(total: u32, bytes: u32, alignment: u32) -> Result<u32> {
            let padding = 1u32
                .checked_shl(alignment)
                .context("arena alignment overflow")?
                - 1;
            total
                .checked_add(bytes)
                .and_then(|v| v.checked_add(padding))
                .context("arena size overflow")
        }
        let memory_bytes = size(
            self.memory_bytes,
            metadata.layout.memory_size,
            metadata.layout.memory_alignment,
        )?;
        let table_entries = size(
            self.table_entries,
            metadata.layout.table_size,
            metadata.layout.table_alignment,
        )?
        .checked_add(metadata.pointer_slots)
        .context("function-table size overflow")?;
        *self = Self {
            memory_bytes,
            table_entries,
        };
        Ok(())
    }

    /// Place the reservation at caller-owned bases. Allocate memory through the
    /// guest allocator first when sharing a heap; this method does not allocate.
    pub fn at(self, memory_base: u32, table_base: u32) -> Result<Arena> {
        ensure!(
            memory_base != 0 && table_base != 0,
            "arena bases must be nonzero"
        );
        Ok(Arena {
            memory: memory_base
                ..memory_base
                    .checked_add(self.memory_bytes)
                    .context("memory arena overflow")?,
            table: table_base
                ..table_base
                    .checked_add(self.table_entries)
                    .context("table arena overflow")?,
        })
    }
}
