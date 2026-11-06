//! Matroska and `WebM`.

mod block;
mod describe;
mod ebml;
mod read;
mod timing;

pub use read::MatroskaReader;

#[cfg(test)]
mod tests;
