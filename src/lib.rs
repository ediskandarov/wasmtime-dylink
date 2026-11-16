#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]

mod arena;
mod loader;
pub use arena::ArenaRequirements;
mod metadata;

pub use loader::{Arena, Library, Loader, Symbol};
pub use metadata::{Layout, Metadata};
pub use wasmtime;

#[cfg(feature = "emscripten")]
pub mod emscripten;
