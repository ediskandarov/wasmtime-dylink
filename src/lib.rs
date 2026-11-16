#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]

mod loader;
mod metadata;

pub use loader::{Arena, Library, Loader, Symbol};
pub use metadata::{Layout, Metadata};
pub use wasmtime;

#[cfg(feature = "emscripten")]
pub mod emscripten;
