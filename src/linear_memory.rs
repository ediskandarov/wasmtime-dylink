// Access guest memory through checked copies, including when its type permits sharing.
use wasmtime::{
    AsContext, AsContextMut, Extern, Instance, Memory, MemoryType, Result, SharedMemory, ensure,
    error::Context,
};
use wiggle::{GuestMemory, GuestPtr};

/// The wasm32 memory shared by the main instance and its dynamic libraries.
#[derive(Clone)]
pub enum LinearMemory {
    Private(Memory),
    Shared(SharedMemory),
}

impl From<Memory> for LinearMemory {
    fn from(memory: Memory) -> Self {
        Self::Private(memory)
    }
}

impl From<SharedMemory> for LinearMemory {
    fn from(memory: SharedMemory) -> Self {
        Self::Shared(memory)
    }
}

impl LinearMemory {
    pub fn exported(mut store: impl AsContextMut, main: Instance) -> Result<Self> {
        match main.get_export(&mut store, "memory") {
            Some(Extern::Memory(memory)) => Ok(Self::Private(memory)),
            Some(Extern::SharedMemory(memory)) => Ok(Self::Shared(memory)),
            _ => wasmtime::bail!("missing exported linear memory"),
        }
    }
    pub(crate) fn import(&self) -> Extern {
        match self {
            Self::Private(m) => Extern::Memory(*m),
            Self::Shared(m) => Extern::SharedMemory(m.clone()),
        }
    }
    pub(crate) fn ty(&self, store: impl AsContext) -> MemoryType {
        match self {
            Self::Private(m) => m.ty(store),
            Self::Shared(m) => m.ty(),
        }
    }
    pub(crate) fn size(&self, store: impl AsContext) -> u64 {
        match self {
            Self::Private(m) => m.size(store),
            Self::Shared(m) => m.size(),
        }
    }
    pub(crate) fn grow(&self, store: impl AsContextMut, pages: u64) -> Result<u64> {
        match self {
            Self::Private(m) => m.grow(store, pages),
            Self::Shared(m) => m.grow(pages),
        }
    }
    fn len(&self, store: impl AsContext) -> usize {
        match self {
            Self::Private(m) => m.data_size(store),
            Self::Shared(m) => m.data().len(),
        }
    }
    pub fn copy(&self, store: impl AsContext, offset: usize, size: usize) -> Result<Vec<u8>> {
        let end = offset.checked_add(size).context("address overflow")?;
        ensure!(end <= self.len(&store), "out-of-bounds read");
        match self {
            Self::Private(m) => Ok(m.data(&store)[offset..end].to_vec()),
            Self::Shared(m) => Ok(GuestMemory::Shared(m.data())
                .to_vec(GuestPtr::new((offset.try_into()?, size.try_into()?)))?),
        }
    }
    pub fn read(&self, store: impl AsContext, offset: usize, bytes: &mut [u8]) -> Result<()> {
        match self {
            Self::Private(m) => m.read(store, offset, bytes)?,
            Self::Shared(_) => bytes.copy_from_slice(&self.copy(store, offset, bytes.len())?),
        }
        Ok(())
    }
    pub fn write(&self, store: impl AsContextMut, offset: usize, bytes: &[u8]) -> Result<()> {
        match self {
            Self::Private(m) => m.write(store, offset, bytes)?,
            Self::Shared(m) => GuestMemory::Shared(m.data()).copy_from_slice(
                bytes,
                GuestPtr::new((offset.try_into()?, bytes.len().try_into()?)),
            )?,
        }
        Ok(())
    }
    pub fn string(&self, store: impl AsContext, address: usize, limit: usize) -> Result<String> {
        ensure!(address != 0, "null string pointer");
        let size = self
            .len(&store)
            .checked_sub(address)
            .context("invalid string pointer")?
            .min(limit);
        let bytes = self.copy(store, address, size)?;
        let end = bytes
            .iter()
            .position(|b| *b == 0)
            .context("unterminated string")?;
        Ok(std::str::from_utf8(&bytes[..end])?.to_owned())
    }
}
