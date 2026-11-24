//! The worker process that runs one native plugin for its host.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use std::rc::Rc;

use alpha2_media::CancelToken;

use super::native::{Forward, NativeInstance};
use super::resources::ResourceReader;
use super::wire::{self, Failure, Hello, Load, child, host};
use super::{OutputCap, PluginError, codes, sha256_hex};

/// The exit status of a worker whose host broke the wire or went away.
const BROKEN: u8 = 3;

type Writer = Rc<RefCell<BufWriter<File>>>;

#[cfg(unix)]
type Input = alpha2_os::DescriptorReader;
#[cfg(not(unix))]
type Input = File;

/// Writes a frame, or ends the process when the host is gone.
fn send(writer: &Writer, tag: u8, parts: &[&[u8]]) {
    if wire::write_frame(&mut *writer.borrow_mut(), tag, parts).is_err() {
        alpha2_os::exit_without_handlers(BROKEN);
    }
}

/// Forwards what the plugin emits and logs to the host.
struct WireForward(Writer);

impl Forward for WireForward {
    fn records(&mut self, batch: &[u8]) {
        for run in wire::record_runs(batch) {
            send(&self.0, child::RECORDS, &[run]);
        }
    }

    fn refused(&mut self, reason: &str) {
        send(&self.0, child::REFUSED, &[reason.as_bytes()]);
    }

    fn log(&mut self, level: u32, message: &[u8]) {
        send(&self.0, child::LOG, &[&level.to_le_bytes(), message]);
    }
}

/// Runs a plugin worker: call it from a subcommand of a program that loads plugins, which a
/// [`super::WorkerCommand`] names. It takes its standard input and output as the host's pipes and
/// never returns.
pub fn main() -> ! {
    let (input, output) = match (alpha2_os::claim_stdin(), alpha2_os::claim_stdout()) {
        (Ok(input), Ok(output)) => (input, output),
        (Err(error), _) | (_, Err(error)) => {
            alpha2_os::diagnostic!("plugin worker: its standard streams: {error}");
            alpha2_os::exit_without_handlers(BROKEN);
        }
    };
    alpha2_os::close_descriptors_except(&[&input, &output]);
    #[cfg(unix)]
    let input = alpha2_os::DescriptorReader::new(input);
    let _ = alpha2_os::on_termination(|termination| {
        alpha2_os::exit_without_handlers(termination.exit_code());
    });
    let _ = alpha2_os::watch_parent();
    alpha2_os::disable_crash_dumps();
    alpha2_os::prefer_out_of_memory_kill();
    let writer: Writer = Rc::new(RefCell::new(BufWriter::new(output)));
    {
        let mut out = writer.borrow_mut();
        let hello = serde_json::to_vec(&Hello {
            wire: wire::WIRE_VERSION,
            host_core: env!("CARGO_PKG_VERSION").to_owned(),
        })
        .unwrap_or_default();
        if out.write_all(&wire::MAGIC).is_err()
            || wire::write_frame(&mut *out, child::HELLO, &[&hello]).is_err()
        {
            alpha2_os::exit_without_handlers(BROKEN);
        }
    }
    serve(&mut BufReader::new(input), &writer)
}

/// Answers the host's requests until it asks the worker to end.
fn serve(input: &mut BufReader<Input>, writer: &Writer) -> ! {
    let mut instance: Option<NativeInstance> = None;
    let mut reported = BTreeSet::new();
    let mut sequence = 0_u32;
    loop {
        let frame = match wire::read_frame(input, wire::MAX_HOST_FRAME_BYTES) {
            Ok(frame) => frame,
            Err(wire::WireError::Closed { clean: true }) => {
                drop(instance.take());
                alpha2_os::exit_without_handlers(0);
            }
            Err(_) => alpha2_os::exit_without_handlers(BROKEN),
        };
        if descriptors_pending(input) {
            alpha2_os::exit_without_handlers(BROKEN);
        }
        sequence = sequence.wrapping_add(1);
        let seq = sequence.to_le_bytes();
        match (frame.tag, instance.as_mut()) {
            (host::SHUTDOWN, _) => {
                drop(instance.take());
                alpha2_os::exit_without_handlers(0);
            }
            (host::LOAD, None) => match load(&frame.payload, writer) {
                Ok(loaded) => {
                    instance = Some(loaded);
                    send(writer, child::LOADED, &[&seq]);
                }
                Err(error) => {
                    let failure = serde_json::to_vec(&Failure::from(&error)).unwrap_or_default();
                    send(writer, child::LOAD_FAILED, &[&seq, &failure]);
                }
            },
            (host::DESCRIBE, Some(plugin)) => {
                send(writer, child::DESCRIBED, &[&seq, &plugin.describe()]);
            }
            (tag, Some(plugin)) => {
                let Some(status) = call(plugin, tag, &frame.payload) else {
                    alpha2_os::exit_without_handlers(BROKEN);
                };
                finish_call(plugin, writer, &mut reported, &seq, status);
            }
            (_, None) => alpha2_os::exit_without_handlers(BROKEN),
        }
    }
}

#[cfg(unix)]
fn descriptors_pending(input: &BufReader<Input>) -> bool {
    input.get_ref().pending() > 0
}

#[cfg(not(unix))]
const fn descriptors_pending(_input: &BufReader<Input>) -> bool {
    false
}

/// Loads the library a `load` frame names.
fn load(payload: &[u8], writer: &Writer) -> Result<NativeInstance, PluginError> {
    let failed = |why: String| PluginError::new(codes::LOAD_FAILED, why);
    let load: Load = serde_json::from_slice(payload)
        .map_err(|error| failed(format!("the worker's load request: {error}")))?;
    if load.check {
        let bytes = std::fs::read(&load.library)
            .map_err(|error| failed(format!("{}: {error}", load.library.display())))?;
        if sha256_hex(&bytes) != load.sha256 {
            return Err(PluginError::new(
                codes::HASH_MISMATCH,
                format!(
                    "{} no longer holds the artifact the manifest pins",
                    load.library.display()
                ),
            ));
        }
    }
    let caps = load
        .caps
        .into_iter()
        .map(|(name, items)| OutputCap { name, items })
        .collect();
    NativeInstance::new(
        &load.sha256,
        || Ok(load.library.clone()),
        ResourceReader::new(load.resources, CancelToken::default()),
        caps,
        Some(Box::new(WireForward(Rc::clone(writer)))),
    )
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(*bytes.get(at..at + 4)?.first_chunk()?))
}

fn i64_at(bytes: &[u8], at: usize) -> Option<i64> {
    Some(i64::from_le_bytes(*bytes.get(at..at + 8)?.first_chunk()?))
}

/// Runs the call a frame asks for; `None` for a frame no call reads.
fn call(plugin: &mut NativeInstance, tag: u8, payload: &[u8]) -> Option<i32> {
    Some(match tag {
        host::BEGIN => plugin.begin(payload),
        host::PASS => plugin.pass(u32_at(payload, 0)?, u32_at(payload, 4)?),
        host::VIDEO => plugin.video(
            i64_at(payload, 0)?,
            (
                u32_at(payload, 8)?,
                u32_at(payload, 12)?,
                u32_at(payload, 16)?,
            ),
            payload.get(20..)?,
        ),
        host::AUDIO => {
            let samples = payload
                .get(28..)?
                .as_chunks::<4>()
                .0
                .iter()
                .map(|bytes| f32::from_le_bytes(*bytes))
                .collect::<Vec<_>>();
            plugin.audio(
                (
                    i64_at(payload, 0)?,
                    i64_at(payload, 8)?,
                    u32_at(payload, 16)?,
                    u32_at(payload, 20)?,
                    u32_at(payload, 24)?,
                ),
                &samples,
            )
        }
        host::END => plugin.end(),
        _ => return None,
    })
}

/// Reports the files the call read, its latched failure and its status.
fn finish_call(
    plugin: &NativeInstance,
    writer: &Writer,
    reported: &mut BTreeSet<String>,
    seq: &[u8],
    status: i32,
) {
    let (read, failure, fail) = {
        let mut state = plugin.state().borrow_mut();
        let read = state
            .reader
            .supplied()
            .into_iter()
            .filter(|file| reported.insert(file.name.clone()))
            .collect::<Vec<_>>();
        let failure = state.reader.take_failure();
        (read, failure, state.collected.failure.take())
    };
    for file in read {
        send(
            writer,
            child::RESOURCE_READ,
            &[&serde_json::to_vec(&file).unwrap_or_default()],
        );
    }
    if let Some(failure) = failure {
        send(
            writer,
            child::RESOURCE_FAILED,
            &[&serde_json::to_vec(&Failure::from(&failure)).unwrap_or_default()],
        );
    }
    send(
        writer,
        child::RETURNED,
        &[
            seq,
            &status.to_le_bytes(),
            fail.as_deref().unwrap_or_default().as_bytes(),
        ],
    );
}
