use std::panic::{AssertUnwindSafe, catch_unwind};

use alpha2_plugin_abi::{status, video_format};
use serde::Deserialize;

use crate::{Audio, Context, Direction, Emitter, Error, Extractor, Frame, VideoFormat};

/// What the host offers a running plugin.
#[derive(Debug)]
pub struct Host {
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) table: alpha2_plugin_abi::HostV1,
}

impl Host {
    /// The bytes of the resource the manifest declares as `name`, copied.
    #[must_use]
    pub fn resource(&self, name: &str) -> Option<Vec<u8>> {
        self.read_resource(name)
    }

    /// Writes a diagnostic at an [`alpha2_plugin_abi::log_level`].
    pub fn log(&self, level: u32, message: &str) {
        self.write_log(level, message);
    }
}

/// One plugin instance: the extractor once `begin` has made it, and the emitter its steps share.
#[doc(hidden)]
pub struct Runtime<E> {
    host: Host,
    outputs: Result<Vec<String>, String>,
    extractor: Option<E>,
    emitter: Emitter,
}

#[derive(Deserialize)]
struct Outputs {
    outputs: Vec<Named>,
}

#[derive(Deserialize)]
struct Named {
    name: String,
}

/// Runs a call, turning a panic into [`status::TRAPPED`].
fn guarded(call: impl FnOnce() -> i32) -> i32 {
    catch_unwind(AssertUnwindSafe(call)).unwrap_or(status::TRAPPED)
}

impl<E: Extractor> Runtime<E> {
    /// An instance that calls `host`.
    #[must_use]
    pub fn new(host: Host) -> Self {
        let outputs = serde_json::from_str::<Outputs>(E::DESCRIPTOR)
            .map(|outputs| {
                outputs
                    .outputs
                    .into_iter()
                    .map(|output| output.name)
                    .collect()
            })
            .map_err(|error| format!("the plugin's descriptor does not parse: {error}"));
        Self {
            host,
            emitter: Emitter::new(outputs.clone().unwrap_or_default()),
            outputs,
            extractor: None,
        }
    }

    fn failed(&self, message: &str) -> i32 {
        self.host.fail(message);
        status::FAILED
    }

    /// `begin` with the context's JSON.
    pub fn begin(&mut self, context: &[u8]) -> i32 {
        guarded(|| {
            if let Err(error) = &self.outputs {
                return self.failed(error);
            }
            if self.extractor.is_some() {
                return self.failed("the run has begun already");
            }
            let context: Context = match serde_json::from_slice(context) {
                Ok(context) => context,
                Err(error) => return self.failed(&format!("the context does not parse: {error}")),
            };
            match E::begin(&context, &self.host) {
                Ok(extractor) => {
                    self.extractor = Some(extractor);
                    status::OK
                }
                Err(Error(message)) => self.failed(&message),
            }
        })
    }

    /// `pass` with the ABI's direction.
    pub fn pass(&mut self, pass: u32, direction: u32) -> i32 {
        let direction = match direction {
            alpha2_plugin_abi::direction::FORWARD => Direction::Forward,
            alpha2_plugin_abi::direction::BACKWARD => Direction::Backward,
            _ => return self.failed("unknown pass direction"),
        };
        self.step(|extractor, _| extractor.pass(pass, direction))
    }

    /// `video` with the ABI's format.
    pub fn video(&mut self, time: i64, width: u32, height: u32, format: u32, data: &[u8]) -> i32 {
        let format = match format {
            video_format::RGBA8 => VideoFormat::Rgba8,
            video_format::LUMA8 => VideoFormat::Luma8,
            _ => return self.failed("unknown video format"),
        };
        let frame = Frame {
            time,
            width,
            height,
            format,
            data,
        };
        self.step(|extractor, out| extractor.video(&frame, out))
    }

    /// `audio`.
    pub fn audio(
        &mut self,
        start: i64,
        start_sample: i64,
        frames: u32,
        rate: u32,
        channels: u32,
        samples: &[f32],
    ) -> i32 {
        let block = Audio {
            start,
            start_sample,
            frames,
            rate,
            channels,
            samples,
        };
        self.step(|extractor, out| extractor.audio(&block, out))
    }

    /// `end`.
    pub fn end(&mut self) -> i32 {
        self.step(Extractor::end)
    }

    /// Runs a step and sends what it emitted; a failed step sends nothing.
    fn step(&mut self, call: impl FnOnce(&mut E, &mut Emitter) -> Result<(), Error>) -> i32 {
        guarded(|| {
            let Some(extractor) = self.extractor.as_mut() else {
                return self.failed("the run has not begun");
            };
            self.emitter.clear();
            if let Err(Error(message)) = call(extractor, &mut self.emitter) {
                return self.failed(&message);
            }
            if let Some(unknown) = self.emitter.unknown.take() {
                return self.failed(&format!("the descriptor has no output named {unknown}"));
            }
            if self.emitter.records.is_empty() {
                return status::OK;
            }
            // A refusal ends the step; the host already knows why.
            if self.host.emit(&self.emitter.records) == status::OK {
                status::OK
            } else {
                status::FAILED
            }
        })
    }
}
