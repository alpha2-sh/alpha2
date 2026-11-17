//! Matroska and `WebM`.

mod block;
mod describe;
mod ebml;
mod read;
mod timing;
mod write;

pub use read::MatroskaReader;
pub use write::{MatroskaFlavor, MatroskaWriter};

#[cfg(test)]
mod tests;
