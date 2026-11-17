//! Builds an alpha2 extractor plugin from one [`Extractor`] implementation. It makes a core wasm
//! module for `wasm32-unknown-unknown` and a C-ABI library for every other target, both speaking
//! the `alpha2-plugin/1` ABI that `docs/plugin-abi.md` specifies.
//!
//! ```ignore
//! struct Brightness;
//!
//! impl alpha2_plugin_sdk::Extractor for Brightness {
//!     const DESCRIPTOR: &'static str = include_str!("../descriptor.json");
//!
//!     fn begin(_: &Context, _: &Host) -> Result<Self, Error> {
//!         Ok(Self)
//!     }
//!
//!     fn video(&mut self, frame: &Frame<'_>, out: &mut Emitter) -> Result<(), Error> {
//!         let sum: u64 = frame.data.iter().map(|&v| u64::from(v)).sum();
//!         out.sample("level", frame.time, &[Some(sum as f32 / frame.data.len() as f32 / 255.0)]);
//!         Ok(())
//!     }
//! }
//!
//! alpha2_plugin_sdk::export_extractor!(Brightness);
//! ```
//!
//! A plugin must not depend on the bit pattern of a NaN: the same wasm module then gives the same
//! values on every runtime.

mod emitter;
mod runtime;

#[doc(hidden)]
#[cfg(not(target_arch = "wasm32"))]
pub mod native;
#[cfg(all(feature = "testing", not(target_arch = "wasm32")))]
pub mod testing;
#[doc(hidden)]
#[cfg(target_arch = "wasm32")]
pub mod wasm;

pub use alpha2_plugin_abi as abi;
pub use emitter::Emitter;
pub use runtime::Host;
#[doc(hidden)]
pub use runtime::Runtime;

use serde::Deserialize;

/// An extractor a plugin provides. Every step but `begin` has a default that does nothing.
pub trait Extractor: Sized + 'static {
    /// The descriptor as JSON, exactly the manifest's `descriptor`.
    const DESCRIPTOR: &'static str;

    /// Starts a run; models and resources load here.
    ///
    /// # Errors
    /// Why the extractor cannot run; the run ends with `EXTRACTOR_FAILED`.
    fn begin(context: &Context, host: &Host) -> Result<Self, Error>;

    /// Starts pass `pass`.
    ///
    /// # Errors
    /// Why the run cannot go on.
    fn pass(&mut self, _pass: u32, _direction: Direction) -> Result<(), Error> {
        Ok(())
    }

    /// One frame.
    ///
    /// # Errors
    /// Why the run cannot go on.
    fn video(&mut self, _frame: &Frame<'_>, _out: &mut Emitter) -> Result<(), Error> {
        Ok(())
    }

    /// One block of audio.
    ///
    /// # Errors
    /// Why the run cannot go on.
    fn audio(&mut self, _block: &Audio<'_>, _out: &mut Emitter) -> Result<(), Error> {
        Ok(())
    }

    /// The end of the run: whole-range work emits here, anywhere in the range.
    ///
    /// # Errors
    /// Why the run cannot finish.
    fn end(&mut self, _out: &mut Emitter) -> Result<(), Error> {
        Ok(())
    }
}

/// Why a step failed; the host reports it as `EXTRACTOR_FAILED` with this message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error(pub String);

impl From<&str> for Error {
    fn from(message: &str) -> Self {
        Self(message.to_owned())
    }
}

impl From<String> for Error {
    fn from(message: String) -> Self {
        Self(message)
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a run reads, as `begin` receives it.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct Context {
    /// Ticks on the input's clock.
    pub range: Range,
    /// The passes, in order.
    pub passes: Vec<PassSpec>,
    /// `source` or `timeline`.
    pub from: String,
    /// The delivered video, for a video extractor.
    #[serde(default)]
    pub video: Option<VideoContext>,
    /// The delivered audio, for an audio extractor.
    #[serde(default)]
    pub audio: Option<AudioContext>,
    /// The frame rate of timeline video.
    #[serde(default)]
    pub frame_rate: Option<Rate>,
    /// The request's parameters.
    #[serde(default)]
    pub params: serde_json::Map<String, serde_json::Value>,
}

/// A time range in ticks.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
pub struct Range {
    /// Inclusive.
    pub start: i64,
    /// Exclusive.
    pub end: i64,
}

/// One pass of a run.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
pub struct PassSpec {
    /// Where it starts, in ticks.
    pub start: i64,
    /// Which way it goes.
    pub direction: Direction,
}

/// Which way a pass goes.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// Times increase.
    Forward,
    /// Times decrease.
    Backward,
}

/// The delivered video.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct VideoContext {
    /// The longer side of a delivered frame, in pixels.
    pub size: u32,
    /// `rgba8` or `luma8`.
    pub format: String,
}

/// The delivered audio.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct AudioContext {
    /// Samples per second.
    pub rate: u32,
    /// `mono` or `source`.
    pub channels: String,
    /// Frames per block.
    pub block_frames: u32,
}

/// A rational frame rate.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
pub struct Rate {
    /// Numerator.
    pub num: u32,
    /// Denominator.
    pub den: u32,
}

/// How a frame's bytes are packed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoFormat {
    /// Four bytes a pixel, straight alpha, display-encoded sRGB.
    Rgba8,
    /// One byte a pixel, BT.709 luma.
    Luma8,
}

/// A delivered frame.
#[derive(Clone, Copy, Debug)]
pub struct Frame<'a> {
    /// Presentation time in ticks.
    pub time: i64,
    /// In pixels.
    pub width: u32,
    /// In pixels.
    pub height: u32,
    /// How `data` is packed.
    pub format: VideoFormat,
    /// Rows top first, tightly packed.
    pub data: &'a [u8],
}

/// A delivered block of audio.
#[derive(Clone, Copy, Debug)]
pub struct Audio<'a> {
    /// The tick at or before the first sample.
    pub start: i64,
    /// The first sample's index at `rate` from the clock's zero.
    pub start_sample: i64,
    /// Frames in the block.
    pub frames: u32,
    /// Samples per second.
    pub rate: u32,
    /// Channels in the block.
    pub channels: u32,
    /// Planar, channel after channel.
    pub samples: &'a [f32],
}

/// Generates the plugin's exports for `$extractor`, the `alpha2_plugin_v1` table on native targets
/// and the `alpha2_*` functions on wasm32. Use it once per plugin crate.
#[macro_export]
macro_rules! export_extractor {
    ($extractor:ty) => {
        /// The plugin's function table.
        #[cfg(not(target_arch = "wasm32"))]
        #[unsafe(no_mangle)]
        pub extern "C" fn alpha2_plugin_v1() -> *const $crate::abi::PluginV1 {
            static TABLE: $crate::abi::PluginV1 = $crate::native::table::<$extractor>();
            &raw const TABLE
        }

        #[cfg(target_arch = "wasm32")]
        const _: () = {
            static SLOT: $crate::wasm::Slot<$extractor> = $crate::wasm::Slot::new();

            #[unsafe(no_mangle)]
            extern "C" fn alpha2_abi() -> i32 {
                $crate::wasm::abi()
            }

            #[unsafe(no_mangle)]
            extern "C" fn alpha2_alloc(size: i32, align: i32) -> i32 {
                $crate::wasm::alloc(size, align)
            }

            #[unsafe(no_mangle)]
            extern "C" fn alpha2_free(ptr: i32, size: i32, align: i32) {
                $crate::wasm::free(ptr, size, align);
            }

            #[unsafe(no_mangle)]
            extern "C" fn alpha2_describe() -> i64 {
                $crate::wasm::describe::<$extractor>()
            }

            #[unsafe(no_mangle)]
            extern "C" fn alpha2_begin(ptr: i32, len: i32) -> i32 {
                $crate::wasm::begin(&SLOT, ptr, len)
            }

            #[unsafe(no_mangle)]
            extern "C" fn alpha2_pass(pass: i32, direction: i32) -> i32 {
                $crate::wasm::pass(&SLOT, pass, direction)
            }

            #[unsafe(no_mangle)]
            extern "C" fn alpha2_video(
                time: i64,
                width: i32,
                height: i32,
                format: i32,
                ptr: i32,
                len: i32,
            ) -> i32 {
                $crate::wasm::video(&SLOT, time, width, height, format, ptr, len)
            }

            #[unsafe(no_mangle)]
            extern "C" fn alpha2_audio(
                start: i64,
                start_sample: i64,
                frames: i32,
                rate: i32,
                channels: i32,
                ptr: i32,
                count: i32,
            ) -> i32 {
                $crate::wasm::audio(
                    &SLOT,
                    start,
                    start_sample,
                    frames,
                    rate,
                    channels,
                    ptr,
                    count,
                )
            }

            #[unsafe(no_mangle)]
            extern "C" fn alpha2_end() -> i32 {
                $crate::wasm::end(&SLOT)
            }
        };
    };
}
