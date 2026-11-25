//! Runs a native plugin in a worker process, one per run.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use alpha2_engine::extract::AudioInput;
use alpha2_media::CancelToken;
use alpha2_model::SuppliedFile;

use super::wire::{self, ChildMessage, Load, WireError, host};
use super::{
    CANCELLED, Call, Collected, FramePath, LogBudget, MAX_FAIL_BYTES, MAX_LOG_BYTES, OutputCap,
    PluginError, Transport, WorkerCommand, bounded_text, codes,
};

/// How often the watcher looks at the cancel and the deadline.
const WATCH_INTERVAL: Duration = Duration::from_millis(50);
/// How long a worker may take to end after it was asked to, before it is killed.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

#[cfg(unix)]
type WorkerInput = std::os::unix::net::UnixStream;
#[cfg(unix)]
type WorkerOutput = std::os::unix::net::UnixStream;
#[cfg(not(unix))]
type WorkerInput = std::process::ChildStdin;
#[cfg(not(unix))]
type WorkerOutput = std::process::ChildStdout;

/// Why the host killed a worker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Killed {
    Cancelled,
    Deadline,
    Wire,
}

/// What the call thread and the watcher share.
struct Watch {
    /// The worker until it is reaped.
    child: Option<Child>,
    job: Option<alpha2_os::KillOnCloseJob>,
    /// When the running exchange must have ended.
    ends: Option<Instant>,
    killed: Option<Killed>,
    done: bool,
}

impl Watch {
    /// Reaps the worker once it has ended, after killing what it left in its process tree.
    fn reap_ended(&mut self) -> Option<ExitStatus> {
        let child = self.child.as_mut()?;
        if !alpha2_os::child_exited(child).unwrap_or(true) {
            return None;
        }
        let _ = alpha2_os::kill_process_tree(child);
        let status = child.wait().ok();
        self.child = None;
        self.job = None;
        status
    }

    fn kill(&mut self, why: Killed) {
        if self.killed.is_none() {
            self.killed = Some(why);
        }
        if let Some(child) = self.child.as_mut() {
            let _ = alpha2_os::kill_process_tree(child);
        }
        self.job = None;
    }
}

struct Shared {
    watch: Mutex<Watch>,
    wake: Condvar,
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, Watch> {
        self.watch.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Kills the worker once `cancel` is set or the running exchange is past its end.
fn watcher(shared: &Shared, cancel: &CancelToken) {
    let mut watch = shared.lock();
    while !watch.done {
        if watch.killed.is_none() {
            if cancel.is_cancelled() {
                watch.kill(Killed::Cancelled);
            } else if watch.ends.is_some_and(|ends| Instant::now() >= ends) {
                watch.kill(Killed::Deadline);
            }
        }
        watch = shared
            .wake
            .wait_timeout(watch, WATCH_INTERVAL)
            .unwrap_or_else(PoisonError::into_inner)
            .0;
    }
}

/// Writes the worker's standard error as plugin diagnostics under the run's log budget, line by
/// line; past the budget it reads on and drops.
fn drain_errors(stream: impl Read, log: &Mutex<LogBudget>) {
    let mut reader = BufReader::new(stream);
    let mut line = Vec::new();
    loop {
        let (consumed, ended) = match reader.fill_buf() {
            Ok([]) | Err(_) => break,
            Ok(available) => {
                let newline = available.iter().position(|byte| *byte == b'\n');
                let take = newline.map_or(available.len(), |at| at + 1);
                let room = (MAX_LOG_BYTES + 1).saturating_sub(line.len());
                line.extend_from_slice(&available[..take.min(room)]);
                (take, newline.is_some())
            }
        };
        reader.consume(consumed);
        if ended {
            write_line(log, &line);
            line.clear();
        }
    }
    if !line.is_empty() {
        write_line(log, &line);
    }
}

fn write_line(log: &Mutex<LogBudget>, line: &[u8]) {
    let text = line.strip_suffix(b"\n").unwrap_or(line);
    let text = text.strip_suffix(b"\r").unwrap_or(text);
    log.lock().unwrap_or_else(PoisonError::into_inner).log(text);
}

/// The details and message of a worker that ended without its reply.
fn crash(status: Option<ExitStatus>) -> PluginError {
    let mut details = serde_json::Map::new();
    #[cfg(unix)]
    let signal = {
        use std::os::unix::process::ExitStatusExt;
        status.and_then(|status| status.signal())
    };
    #[cfg(not(unix))]
    let signal: Option<i32> = None;
    let message = match (signal, status.and_then(|status| status.code())) {
        (Some(signal), _) => {
            details.insert("signal".to_owned(), signal.into());
            if signal == 9 {
                "the plugin process was killed (signal 9), possibly for running out of memory"
                    .to_owned()
            } else {
                format!("the plugin process crashed (signal {signal})")
            }
        }
        (None, Some(code)) if cfg!(windows) => {
            let code = code.cast_unsigned();
            details.insert("exit".to_owned(), code.into());
            format!("the plugin process ended without its reply (exit code {code:#010x})")
        }
        (None, Some(code)) => {
            details.insert("exit".to_owned(), code.into());
            format!("the plugin process ended without its reply (exit code {code})")
        }
        (None, None) => "the plugin process ended without its reply".to_owned(),
    };
    PluginError::new(codes::TRAPPED, message).with_details(details)
}

fn wire_violation(what: &str) -> PluginError {
    let mut details = serde_json::Map::new();
    details.insert("wire".to_owned(), what.into());
    PluginError::new(
        codes::TRAPPED,
        format!("the plugin process broke the wire: {what}"),
    )
    .with_details(details)
}

#[cfg(unix)]
type Channel = (WorkerInput, WorkerOutput);

#[cfg(not(unix))]
struct Channel;

#[cfg(unix)]
fn open_channel(spawn: &mut Command) -> std::io::Result<Channel> {
    use std::os::fd::OwnedFd;
    let (host, worker) = std::os::unix::net::UnixStream::pair()?;
    spawn
        .stdin(Stdio::from(OwnedFd::from(worker.try_clone()?)))
        .stdout(Stdio::from(OwnedFd::from(worker)));
    Ok((host.try_clone()?, host))
}

#[cfg(not(unix))]
#[expect(clippy::unnecessary_wraps, reason = "the socket pair of Unix may fail")]
fn open_channel(spawn: &mut Command) -> std::io::Result<Channel> {
    spawn.stdin(Stdio::piped()).stdout(Stdio::piped());
    Ok(Channel)
}

#[cfg(unix)]
#[expect(
    clippy::unnecessary_wraps,
    reason = "the pipes of other platforms may be missing"
)]
const fn channel_ends(channel: Channel, _child: &mut Child) -> Option<(WorkerInput, WorkerOutput)> {
    Some(channel)
}

#[cfg(not(unix))]
fn channel_ends(_channel: Channel, child: &mut Child) -> Option<(WorkerInput, WorkerOutput)> {
    Some((child.stdin.take()?, child.stdout.take()?))
}

#[derive(Debug)]
pub(super) enum Frames {
    Unmade,
    Shared(alpha2_os::SharedRegion),
    Pipe,
}

fn write_request(
    input: &mut WorkerInput,
    tag: u8,
    parts: &[&[u8]],
    region: Option<&alpha2_os::SharedRegion>,
) -> std::io::Result<()> {
    #[cfg(unix)]
    if let Some(region) = region {
        let mut frame = Vec::new();
        wire::write_frame(&mut frame, tag, parts)?;
        return alpha2_os::write_with_descriptor(input, &frame, region.descriptor());
    }
    #[cfg(not(unix))]
    let _ = region;
    wire::write_frame(input, tag, parts)
}

/// A native plugin in a worker process.
pub(super) struct ProcessTransport {
    shared: Arc<Shared>,
    input: Option<WorkerInput>,
    output: BufReader<WorkerOutput>,
    sequence: u32,
    deadline: Duration,
    collected: Collected,
    caps: Vec<OutputCap>,
    log: Arc<Mutex<LogBudget>>,
    /// The supplied files the worker may report, by name, with their enumerated lengths.
    readable: BTreeMap<String, u64>,
    /// The supplied files the worker reported, by name.
    read: BTreeMap<String, SuppliedFile>,
    /// Why the worker can take no further request.
    broken: Option<PluginError>,
    pub(super) frames: Frames,
    pub(super) make_region: fn(usize) -> std::io::Result<alpha2_os::SharedRegion>,
}

impl ProcessTransport {
    /// Starts a worker with `command` and waits for its `hello`.
    ///
    /// # Errors
    /// `PLUGIN_LOAD_FAILED` when it does not start or is another build, `PLUGIN_TRAPPED` when it
    /// ends or breaks the wire before its `hello`, `PLUGIN_LIMIT` when it does not answer within
    /// `deadline`, `CANCELLED` once `cancel` is set.
    pub(super) fn spawn(
        command: &WorkerCommand,
        cancel: &CancelToken,
        deadline: Duration,
    ) -> Result<Self, PluginError> {
        let failed = |why: String| PluginError::new(codes::LOAD_FAILED, why);
        let mut spawn = Command::new(&command.program);
        spawn
            .args(&command.args)
            .envs(command.envs.iter().map(|(key, value)| (key, value)))
            .stderr(Stdio::piped());
        alpha2_os::own_process_tree(&mut spawn);
        let channel = open_channel(&mut spawn)
            .map_err(|error| failed(format!("the plugin worker's channel: {error}")))?;
        let spawned = spawn.spawn();
        drop(spawn);
        let mut child = spawned.map_err(|error| {
            failed(format!(
                "the plugin worker {} did not start: {error}",
                command.program.display()
            ))
        })?;
        let job = alpha2_os::KillOnCloseJob::new().ok();
        if let Some(job) = &job {
            let _ = job.assign(&child);
        }
        let (Some((input, output)), Some(errors)) =
            (channel_ends(channel, &mut child), child.stderr.take())
        else {
            let _ = alpha2_os::kill_process_tree(&mut child);
            return Err(failed("the plugin worker has no pipes".to_owned()));
        };
        let log = Arc::new(Mutex::new(LogBudget::default()));
        let shared = Arc::new(Shared {
            watch: Mutex::new(Watch {
                child: Some(child),
                job,
                ends: None,
                killed: None,
                done: false,
            }),
            wake: Condvar::new(),
        });
        let errors_log = Arc::clone(&log);
        let started = std::thread::Builder::new()
            .name("alpha2-plugin-stderr".to_owned())
            .spawn(move || drain_errors(errors, &errors_log))
            .and_then(|_| {
                let watched = Arc::clone(&shared);
                let cancel = cancel.clone();
                std::thread::Builder::new()
                    .name("alpha2-plugin-watch".to_owned())
                    .spawn(move || watcher(&watched, &cancel))
            });
        let mut transport = Self {
            shared,
            input: Some(input),
            output: BufReader::new(output),
            sequence: 0,
            deadline,
            collected: Collected::default(),
            caps: Vec::new(),
            log,
            readable: BTreeMap::new(),
            read: BTreeMap::new(),
            broken: None,
            frames: match command.frames {
                FramePath::Shared => Frames::Unmade,
                FramePath::Pipe => Frames::Pipe,
            },
            make_region: alpha2_os::SharedRegion::create,
        };
        if let Err(error) = started {
            transport.shared.lock().kill(Killed::Wire);
            return Err(failed(format!("the plugin worker's threads: {error}")));
        }
        transport.arm();
        let hello = wire::skip_to_magic(&mut transport.output)
            .and_then(|()| wire::read_frame(&mut transport.output, wire::MAX_CHILD_FRAME_BYTES))
            .and_then(wire::parse_child);
        transport.disarm();
        match hello {
            Ok(ChildMessage::Hello(hello))
                if hello.wire == wire::WIRE_VERSION
                    && hello.host_core == env!("CARGO_PKG_VERSION") =>
            {
                Ok(transport)
            }
            Ok(ChildMessage::Hello(hello)) => {
                transport.shared.lock().kill(Killed::Wire);
                Err(failed(format!(
                    "the plugin worker is another build (wire {}, alpha2-host-core {}); this \
                     host speaks wire {} as alpha2-host-core {}",
                    hello.wire,
                    hello.host_core,
                    wire::WIRE_VERSION,
                    env!("CARGO_PKG_VERSION")
                )))
            }
            Ok(_) => Err(transport.violation("a first frame that is not hello")),
            Err(error) => Err(transport.ended(&error)),
        }
    }

    /// Asks the worker to load the plugin.
    ///
    /// # Errors
    /// The worker's load failure, or why the worker failed.
    pub(super) fn load(&mut self, load: &Load, caps: Vec<OutputCap>) -> Result<(), PluginError> {
        self.readable = load.resources.supplied_lengths();
        self.caps = caps;
        let payload = serde_json::to_vec(load).map_err(|error| {
            PluginError::new(
                codes::LOAD_FAILED,
                format!("the plugin's files cannot be named to its worker: {error}"),
            )
        })?;
        match self.request(host::LOAD, &[&payload])? {
            ChildMessage::Loaded { .. } => Ok(()),
            ChildMessage::LoadFailed { failure, .. } => {
                match failure.into_error().filter(|error| {
                    [
                        codes::LOAD_FAILED,
                        codes::ABI_UNSUPPORTED,
                        codes::HASH_MISMATCH,
                    ]
                    .contains(&error.code)
                }) {
                    Some(error) => Err(error),
                    None => Err(self.violation("a load failure with another code")),
                }
            }
            _ => Err(self.violation("another reply to load")),
        }
    }

    fn arm(&self) {
        let ends = Instant::now().checked_add(self.deadline);
        self.shared.lock().ends = ends;
        self.shared.wake.notify_all();
    }

    fn disarm(&self) {
        self.shared.lock().ends = None;
    }

    /// Kills the worker for breaking the wire.
    fn violation(&mut self, what: &str) -> PluginError {
        self.shared.lock().kill(Killed::Wire);
        let error = wire_violation(what);
        self.broken = Some(error.clone());
        error
    }

    /// Why the worker's pipe failed: a cancel, the deadline, a broken frame or the worker's end.
    fn ended(&mut self, error: &WireError) -> PluginError {
        if matches!(error, WireError::Oversized(_) | WireError::Malformed(_)) {
            return self.violation(&error.to_string());
        }
        let killed_first = self.shared.lock().killed;
        let status = match killed_first {
            Some(_) => None,
            None => self.exit_status(),
        };
        let killed = self.shared.lock().killed;
        let failure = match killed {
            Some(Killed::Cancelled) => PluginError::new(CANCELLED, "the extraction was cancelled"),
            Some(Killed::Deadline) => PluginError::new(
                codes::LIMIT,
                format!(
                    "a plugin call ran longer than {} ms",
                    self.deadline.as_millis()
                ),
            ),
            Some(Killed::Wire) => self
                .broken
                .clone()
                .unwrap_or_else(|| wire_violation(&error.to_string())),
            None if status.is_none() => {
                self.shared.lock().kill(Killed::Wire);
                wire_violation(&format!("{error} while the worker lives"))
            }
            None => crash(status),
        };
        self.broken = Some(failure.clone());
        failure
    }

    /// The worker's exit status once it ends within the grace period; `None` while it lives.
    fn exit_status(&self) -> Option<ExitStatus> {
        let asked = Instant::now();
        loop {
            {
                let mut watch = self.shared.lock();
                if watch.child.is_none() || watch.killed.is_some() {
                    return None;
                }
                if let Some(status) = watch.reap_ended() {
                    return Some(status);
                }
            }
            if asked.elapsed() >= SHUTDOWN_GRACE {
                return None;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Sends a request and reads the worker's frames until the reply that ends it.
    fn request(&mut self, tag: u8, parts: &[&[u8]]) -> Result<ChildMessage, PluginError> {
        self.exchange(tag, parts, None)
    }

    fn exchange(
        &mut self,
        tag: u8,
        parts: &[&[u8]],
        region: Option<&alpha2_os::SharedRegion>,
    ) -> Result<ChildMessage, PluginError> {
        if let Some(broken) = &self.broken {
            return Err(broken.clone());
        }
        self.sequence = self.sequence.wrapping_add(1);
        self.collected = Collected::default();
        let mut latched: Option<PluginError> = None;
        self.arm();
        let written = match self.input.as_mut() {
            Some(input) => write_request(input, tag, parts, region)
                .map_err(|error| WireError::Io(error.to_string())),
            None => Err(WireError::Closed { clean: true }),
        };
        let reply = written.and_then(|()| {
            loop {
                let message = match wire::read_frame(&mut self.output, wire::MAX_CHILD_FRAME_BYTES)
                    .and_then(wire::parse_child)
                {
                    Ok(message) => message,
                    Err(error) => break Err(error),
                };
                if let Some(sequence) = message.sequence() {
                    if sequence != self.sequence {
                        break Err(WireError::Malformed(format!(
                            "a reply to request {sequence} while request {} runs",
                            self.sequence
                        )));
                    }
                    break Ok(message);
                }
                if let Err(what) = self.take(message, &mut latched) {
                    break Err(WireError::Malformed(what));
                }
            }
        });
        self.disarm();
        match reply {
            Ok(ChildMessage::Returned { status, fail, .. }) => {
                if let Some(error) = latched {
                    self.collected = Collected::default();
                    return Err(error);
                }
                self.collected.failure = fail.map(|fail| bounded_text(&fail, MAX_FAIL_BYTES));
                Ok(ChildMessage::Returned {
                    sequence: self.sequence,
                    status,
                    fail: None,
                })
            }
            Ok(reply) if latched.is_none() => Ok(reply),
            Ok(_) => Err(self.violation("a resource failure outside a call")),
            Err(error) => Err(self.ended(&error)),
        }
    }

    /// Takes a frame that does not end the request; why the wire does not allow it otherwise.
    fn take(
        &mut self,
        message: ChildMessage,
        latched: &mut Option<PluginError>,
    ) -> Result<(), String> {
        match message {
            ChildMessage::Log { text, .. } => {
                self.log
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .log(&text);
            }
            ChildMessage::Records(batch) => {
                self.collected.emit(&batch, &self.caps);
            }
            ChildMessage::Refused(reason) => {
                if self.collected.refusal.is_none() {
                    self.collected.records.clear();
                    self.collected.refusal = Some(bounded_text(reason.as_bytes(), MAX_FAIL_BYTES));
                }
            }
            ChildMessage::ResourceRead(file) => {
                if self.readable.get(&file.name) != Some(&file.bytes)
                    || self.read.contains_key(&file.name)
                {
                    return Err(format!(
                        "a read of {} that the run does not supply as such",
                        file.name
                    ));
                }
                self.read.insert(file.name.clone(), file);
            }
            ChildMessage::ResourceFailed(failure) => {
                let error = failure
                    .into_error()
                    .ok_or_else(|| "a resource failure with an unknown code".to_owned())?;
                latched.get_or_insert(error);
            }
            ChildMessage::Hello(_) => return Err("a second hello".to_owned()),
            _ => return Err("a reply out of order".to_owned()),
        }
        Ok(())
    }

    fn call(&mut self, tag: u8, parts: &[&[u8]]) -> Call {
        match self
            .request(tag, parts)
            .map_err(PluginError::into_call_error)?
        {
            ChildMessage::Returned { status, .. } => self.collected.outcome(status),
            _ => Err(self.violation("another reply to a call").into_call_error()),
        }
    }

    fn region_for(
        &mut self,
        len: usize,
    ) -> Result<Option<&mut alpha2_os::SharedRegion>, PluginError> {
        let fits = matches!(&self.frames, Frames::Shared(region) if region.len() >= len);
        if !fits && !matches!(self.frames, Frames::Pipe) {
            self.frames = match self.map_region(len)? {
                Ok(region) => Frames::Shared(region),
                Err(why) => {
                    alpha2_os::diagnostic!(
                        "plugin: video frames go through the worker's pipe for the rest of the \
                         run: {why}"
                    );
                    Frames::Pipe
                }
            };
        }
        Ok(match &mut self.frames {
            Frames::Shared(region) => Some(region),
            Frames::Unmade | Frames::Pipe => None,
        })
    }

    fn map_region(
        &mut self,
        len: usize,
    ) -> Result<Result<alpha2_os::SharedRegion, String>, PluginError> {
        let region = match (self.make_region)(len) {
            Ok(region) => region,
            Err(error) => return Ok(Err(format!("the memory cannot be made: {error}"))),
        };
        let length = u64::try_from(region.len())
            .unwrap_or(u64::MAX)
            .to_le_bytes();
        #[cfg(windows)]
        let handle = {
            let shared = match self.shared.lock().child.as_ref() {
                Some(child) => region.share_with(child),
                None => Err(std::io::Error::other("the worker has ended")),
            };
            match shared {
                Ok(handle) => handle.to_le_bytes(),
                Err(error) => return Ok(Err(format!("the memory cannot be shared: {error}"))),
            }
        };
        #[cfg(windows)]
        let parts: [&[u8]; 2] = [&length, &handle];
        #[cfg(not(windows))]
        let parts: [&[u8]; 1] = [&length];
        match self.exchange(host::REGION, &parts, Some(&region))? {
            ChildMessage::RegionMapped { .. } => Ok(Ok(region)),
            ChildMessage::RegionFailed { failure, .. } => Ok(Err(format!(
                "the worker cannot map the memory: {}",
                bounded_text(failure.message.as_bytes(), MAX_FAIL_BYTES)
            ))),
            _ => Err(self.violation("another reply to region")),
        }
    }

    /// Asks the worker to end and leaves it to a reaper thread, which kills it past a grace.
    fn finish(&mut self) {
        let Some(mut input) = self.input.take() else {
            return;
        };
        let _ = wire::write_frame(&mut input, host::SHUTDOWN, &[]);
        drop(input);
        let (child, job, killed) = {
            let mut watch = self.shared.lock();
            watch.done = true;
            (watch.child.take(), watch.job.take(), watch.killed.is_some())
        };
        self.shared.wake.notify_all();
        let Some(mut child) = child else {
            return;
        };
        let reaped = std::thread::Builder::new()
            .name("alpha2-plugin-reap".to_owned())
            .spawn(move || reap(&mut child, job, !killed));
        if let Err(error) = reaped {
            alpha2_os::diagnostic!("plugin: the worker's reaper did not start: {error}");
        }
    }
}

/// Waits for a worker asked to end, and kills it and what it started past the grace period. An
/// end other than the asked one is written as a diagnostic when `report`.
fn reap(child: &mut Child, _job: Option<alpha2_os::KillOnCloseJob>, report: bool) {
    let asked = Instant::now();
    loop {
        match alpha2_os::child_exited(child) {
            Ok(true) => {
                let _ = alpha2_os::kill_process_tree(child);
                match child.wait() {
                    Ok(status) if report && !status.success() => {
                        alpha2_os::diagnostic!(
                            "plugin: the worker ended with {status} after its last reply"
                        );
                    }
                    _ => {}
                }
                break;
            }
            Ok(false) if asked.elapsed() < SHUTDOWN_GRACE => std::thread::sleep(WATCH_INTERVAL),
            Ok(false) | Err(_) => {
                alpha2_os::diagnostic!(
                    "plugin: the worker did not end within {} ms of its last reply; killed",
                    SHUTDOWN_GRACE.as_millis()
                );
                let _ = alpha2_os::kill_process_tree(child);
                let _ = child.wait();
                break;
            }
        }
    }
}

impl Transport for ProcessTransport {
    fn describe(&mut self) -> Result<String, PluginError> {
        match self.request(host::DESCRIBE, &[])? {
            ChildMessage::Described { descriptor, .. } => {
                String::from_utf8(descriptor).map_err(|_| {
                    PluginError::new(codes::DESCRIPTOR_MISMATCH, "the descriptor is not UTF-8")
                })
            }
            _ => Err(self.violation("another reply to describe")),
        }
    }

    fn begin(&mut self, context: &[u8]) -> Call {
        self.call(host::BEGIN, &[context])
    }

    fn pass(&mut self, pass: u32, direction: u32) -> Call {
        self.call(host::PASS, &[&pass.to_le_bytes(), &direction.to_le_bytes()])
    }

    fn video(&mut self, time: i64, width: u32, height: u32, format: u32, data: &[u8]) -> Call {
        let (time, width, height, format) = (
            time.to_le_bytes(),
            width.to_le_bytes(),
            height.to_le_bytes(),
            format.to_le_bytes(),
        );
        let shared = match self.region_for(data.len()) {
            Ok(Some(region)) => {
                region.as_mut_slice()[..data.len()].copy_from_slice(data);
                true
            }
            Ok(None) => false,
            Err(error) => return Err(error.into_call_error()),
        };
        if shared {
            let length = u64::try_from(data.len()).unwrap_or(u64::MAX).to_le_bytes();
            self.call(
                host::VIDEO_SHARED,
                &[
                    &time,
                    &width,
                    &height,
                    &format,
                    &0_u64.to_le_bytes(),
                    &length,
                ],
            )
        } else {
            self.call(host::VIDEO, &[&time, &width, &height, &format, data])
        }
    }

    fn audio(&mut self, block: &AudioInput) -> Call {
        let samples = block
            .samples
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect::<Vec<_>>();
        self.call(
            host::AUDIO,
            &[
                &block.start.ticks().to_le_bytes(),
                &block.start_sample.to_le_bytes(),
                &block.frames.to_le_bytes(),
                &block.rate.to_le_bytes(),
                &block.channels.to_le_bytes(),
                &samples,
            ],
        )
    }

    fn end(&mut self) -> Call {
        let outcome = self.call(host::END, &[]);
        self.finish();
        outcome
    }

    fn supplied(&self) -> Vec<SuppliedFile> {
        self.read.values().cloned().collect()
    }
}

impl Drop for ProcessTransport {
    fn drop(&mut self) {
        self.finish();
        let mut watch = self.shared.lock();
        watch.done = true;
        if let Some(child) = watch.child.as_mut() {
            let _ = alpha2_os::kill_process_tree(child);
            let _ = child.wait();
        }
        drop(watch);
        self.shared.wake.notify_all();
    }
}
