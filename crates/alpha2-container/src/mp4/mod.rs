//! MP4, MOV and M4A (ISO base media and `QuickTime` files).

mod boxes;
pub(crate) mod describe;
mod fragment;
pub(crate) mod index;
pub(crate) mod movie;
pub(crate) mod nal;
mod read;
pub(crate) mod write;

pub use read::Mp4Reader;
pub use write::{Mp4Flavor, Mp4Writer};

#[cfg(test)]
pub(crate) mod tests;
