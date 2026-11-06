//! MP4, MOV and M4A (ISO base media and `QuickTime` files).

mod boxes;
mod describe;
mod fragment;
mod index;
mod movie;
mod nal;
mod read;
mod write;

pub use read::Mp4Reader;
pub use write::{Mp4Flavor, Mp4Writer};

#[cfg(test)]
mod tests;
