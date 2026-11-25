//! The `scripted` test extractor as a plugin. Its params hold the script: `per_delivery`, one list
//! of emits per delivered frame in order, and `at_end`, emitted by `end`. Emits are spelled as in
//! the host protocol's acknowledgements.
//!
//! For loader tests, a delivery index in `fail_at`, `panic_at`, `spin_at`, `hog_at` or `abort_at`
//! makes that delivery fail, panic, never return, allocate without bound, or end the process; and
//! `resource` names a resource whose UTF-8 text `end` emits as a word of `words`. As wasm, `flood`
//! (`calls`, 0 for no end, and `ignore_refusal`) makes the second delivery call the host's `emit`
//! that many times with one interval of `quiet`. As a native library, `park_thread: true` starts a
//! thread at `begin` that runs the library's code until the process exits. `log` (`text`, `lines`)
//! makes `begin` log `text` that many times, and `fail_message` sets the `fail_at` message.
//! `path` names a resource whose path, as the host hands it out, `end` emits as a word of
//! `words`, or `none` when the host gives none.
//!
//! As a native library, for isolation tests: a delivery index in `segv_at` makes that delivery
//! write through a null pointer, in `write_input_at` write into its frame, in `sleep_at` sleep
//! for good, and in `print_at` print to standard output first; `spawn_at` (`delivery`,
//! `pid_file`, `then`) starts a long-lived process, writes its id to `pid_file`, then sleeps for
//! good, or with `then` `abort` or `return` does that. `report` names a file `begin` writes this
//! process's id and open descriptors to, and `stderr_lines` makes `begin` write that many lines to
//! standard error.

use alpha2_plugin_sdk::{Context, Emitter, Error, Extractor, Frame, Host};
use serde_json::Value;

/// The extractor.
pub struct Scripted {
    per_delivery: Vec<Value>,
    at_end: Vec<Value>,
    fail_at: Option<u64>,
    fail_message: Option<String>,
    panic_at: Option<u64>,
    spin_at: Option<u64>,
    hog_at: Option<u64>,
    abort_at: Option<u64>,
    segv_at: Option<u64>,
    write_input_at: Option<u64>,
    sleep_at: Option<u64>,
    print_at: Option<u64>,
    spawn_at: Option<(u64, String, String)>,
    resource: Option<String>,
    path: Option<String>,
    flood: Option<(u64, bool)>,
    delivered: u64,
}

/// Calls the host's `emit` `calls` times (0 for no end) with one interval of `quiet`, stopping at
/// a refusal unless `ignore_refusal`.
#[cfg(target_arch = "wasm32")]
fn flood(calls: u64, ignore_refusal: bool) -> Result<(), Error> {
    use alpha2_plugin_sdk::abi::records::{Record, encode};

    #[link(wasm_import_module = "alpha2")]
    unsafe extern "C" {
        fn emit(ptr: *const u8, len: usize) -> i32;
    }
    let mut record = Vec::new();
    encode(
        &Record::Interval {
            output: 4,
            start: 0,
            end: 4_000,
            label: None,
            score: None,
        },
        &mut record,
    );
    let mut sent = 0;
    while calls == 0 || sent < calls {
        sent += 1;
        // SAFETY: the record stays alive for the call.
        let status = unsafe { emit(record.as_ptr(), record.len()) };
        if status != alpha2_plugin_sdk::abi::status::OK && !ignore_refusal {
            return Err("the host refused an emit".into());
        }
    }
    Ok(())
}

#[cfg(not(target_arch = "wasm32"))]
fn flood(_calls: u64, _ignore_refusal: bool) -> Result<(), Error> {
    Err("flood runs only as wasm".into())
}

/// Sleeps until the process ends.
fn sleep_for_good() -> ! {
    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

/// Starts a process that lives about a minute and writes its id to `pid_file`.
#[cfg(not(target_arch = "wasm32"))]
fn spawn_long_lived(pid_file: &str) -> Result<(), Error> {
    let mut command = if cfg!(windows) {
        let mut command = std::process::Command::new("ping");
        command.args(["-n", "60", "127.0.0.1"]);
        command
    } else {
        let mut command = std::process::Command::new("sleep");
        command.arg("60");
        command
    };
    let child = command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|error| Error::from(error.to_string()))?;
    std::fs::write(pid_file, child.id().to_string()).map_err(|error| error.to_string().into())
}

#[cfg(target_arch = "wasm32")]
fn spawn_long_lived(_pid_file: &str) -> Result<(), Error> {
    Err("spawn_at runs only as a native library".into())
}

/// Writes this process's id and its open descriptors, one per line, to `path`.
#[cfg(not(target_arch = "wasm32"))]
fn report(path: &str) -> Result<(), Error> {
    let directory = if cfg!(target_os = "linux") {
        "/proc/self/fd"
    } else {
        "/dev/fd"
    };
    let mut descriptors = std::fs::read_dir(directory)
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse::<i32>().ok())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    descriptors.sort_unstable();
    let lines = std::iter::once(std::process::id().to_string())
        .chain(descriptors.iter().map(ToString::to_string))
        .collect::<Vec<_>>();
    std::fs::write(path, lines.join("\n")).map_err(|error| error.to_string().into())
}

#[cfg(target_arch = "wasm32")]
fn report(_path: &str) -> Result<(), Error> {
    Err("report runs only as a native library".into())
}

/// A thread that wakes every millisecond in this library's code, after `destroy` too.
#[cfg(not(target_arch = "wasm32"))]
fn park_thread() -> Result<(), Error> {
    std::thread::Builder::new()
        .spawn(|| {
            loop {
                std::thread::sleep(std::time::Duration::from_millis(1));
                std::hint::black_box(());
            }
        })
        .map(drop)
        .map_err(|error| error.to_string().into())
}

#[cfg(target_arch = "wasm32")]
fn park_thread() -> Result<(), Error> {
    Err("park_thread runs only as a native library".into())
}

fn list(context: &Context, key: &str) -> Result<Vec<Value>, Error> {
    match context.params.get(key) {
        None => Ok(Vec::new()),
        Some(Value::Array(items)) => Ok(items.clone()),
        Some(_) => Err(format!("params.{key} is not a list").into()),
    }
}

fn emit(out: &mut Emitter, item: &Value) -> Result<(), Error> {
    let text = |key: &str| item.get(key).and_then(Value::as_str);
    let tick = |key: &str| {
        item.get(key)
            .and_then(Value::as_i64)
            .ok_or_else(|| Error::from(format!("an emit has no integer {key}")))
    };
    let output = text("output").ok_or("an emit has no output")?;
    if let Some(values) = item.get("values").and_then(Value::as_array) {
        #[allow(clippy::cast_possible_truncation)]
        let values = values
            .iter()
            .map(|value| value.as_f64().map(|value| value as f32))
            .collect::<Vec<_>>();
        out.sample(output, tick("time")?, &values);
    } else if let Some(word) = text("text") {
        out.word(output, tick("start")?, tick("end")?, word, text("speaker"));
    } else {
        #[allow(clippy::cast_possible_truncation)]
        let score = item
            .get("score")
            .and_then(Value::as_f64)
            .map(|score| score as f32);
        out.interval(output, tick("start")?, tick("end")?, text("label"), score);
    }
    Ok(())
}

impl Extractor for Scripted {
    const DESCRIPTOR: &'static str = include_str!("../descriptor.json");

    fn begin(context: &Context, host: &Host) -> Result<Self, Error> {
        let index = |key: &str| context.params.get(key).and_then(Value::as_u64);
        if context.params.get("park_thread") == Some(&Value::Bool(true)) {
            park_thread()?;
        }
        if let Some(path) = context.params.get("report").and_then(Value::as_str) {
            report(path)?;
        }
        for line in 0..context
            .params
            .get("stderr_lines")
            .and_then(Value::as_u64)
            .unwrap_or(0)
        {
            eprintln!("{line:08} {}", "e".repeat(91));
        }
        if let Some(log) = context.params.get("log") {
            let text = log.get("text").and_then(Value::as_str).unwrap_or_default();
            for _ in 0..log.get("lines").and_then(Value::as_u64).unwrap_or(1) {
                host.log(alpha2_plugin_sdk::abi::log_level::INFO, text);
            }
        }
        let resource = match context.params.get("resource").and_then(Value::as_str) {
            None => None,
            Some(name) => {
                let bytes = host
                    .resource(name)
                    .ok_or_else(|| Error::from(format!("no resource named {name}")))?;
                Some(String::from_utf8(bytes).map_err(|_| "the resource is not UTF-8")?)
            }
        };
        Ok(Self {
            per_delivery: list(context, "per_delivery")?,
            at_end: list(context, "at_end")?,
            fail_at: index("fail_at"),
            fail_message: context
                .params
                .get("fail_message")
                .and_then(Value::as_str)
                .map(str::to_owned),
            panic_at: index("panic_at"),
            spin_at: index("spin_at"),
            hog_at: index("hog_at"),
            abort_at: index("abort_at"),
            segv_at: index("segv_at"),
            write_input_at: index("write_input_at"),
            sleep_at: index("sleep_at"),
            print_at: index("print_at"),
            spawn_at: context.params.get("spawn_at").and_then(|spawn| {
                Some((
                    spawn.get("delivery")?.as_u64()?,
                    spawn.get("pid_file")?.as_str()?.to_owned(),
                    spawn
                        .get("then")
                        .and_then(Value::as_str)
                        .unwrap_or("sleep")
                        .to_owned(),
                ))
            }),
            resource,
            path: context
                .params
                .get("path")
                .and_then(Value::as_str)
                .map(|name| {
                    host.resource_path(name)
                        .map_or_else(|| "none".to_owned(), |path| path.display().to_string())
                }),
            flood: context.params.get("flood").map(|flood| {
                (
                    flood.get("calls").and_then(Value::as_u64).unwrap_or(0),
                    flood
                        .get("ignore_refusal")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                )
            }),
            delivered: 0,
        })
    }

    fn video(&mut self, frame: &Frame<'_>, out: &mut Emitter) -> Result<(), Error> {
        let delivery = self.delivered;
        self.delivered += 1;
        assert!(
            self.panic_at != Some(delivery),
            "scripted panic at delivery {delivery}"
        );
        if self.fail_at == Some(delivery) {
            return Err(self
                .fail_message
                .clone()
                .unwrap_or_else(|| format!("scripted failure at delivery {delivery}"))
                .into());
        }
        if self.print_at == Some(delivery) {
            println!("text a runtime prints to standard output at delivery {delivery}");
        }
        if self.abort_at == Some(delivery) {
            std::process::abort();
        }
        if self.segv_at == Some(delivery) {
            // SAFETY: not sound by design: the write faults and the process ends there.
            unsafe {
                std::ptr::write_volatile(std::hint::black_box(std::ptr::null_mut::<u8>()), 1);
            };
        }
        if self.write_input_at == Some(delivery) {
            // SAFETY: not sound by design: the frame is borrowed read-only from the host.
            unsafe { frame.data.as_ptr().cast_mut().write_volatile(1) };
        }
        if self.sleep_at == Some(delivery) {
            sleep_for_good();
        }
        if let Some((_, pid_file, then)) =
            self.spawn_at.as_ref().filter(|(at, _, _)| *at == delivery)
        {
            spawn_long_lived(pid_file)?;
            match then.as_str() {
                "abort" => std::process::abort(),
                "return" => {}
                _ => sleep_for_good(),
            }
        }
        if self.spin_at == Some(delivery) {
            loop {
                std::hint::black_box(delivery);
            }
        }
        if let Some((calls, ignore_refusal)) = self.flood.filter(|_| delivery == 1) {
            flood(calls, ignore_refusal)?;
        }
        if self.hog_at == Some(delivery) {
            let mut held = Vec::new();
            loop {
                held.push(std::hint::black_box(vec![1_u8; 16 << 20]));
            }
        }
        let items = usize::try_from(delivery)
            .ok()
            .and_then(|delivery| self.per_delivery.get(delivery))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        items.iter().try_for_each(|item| emit(out, item))
    }

    fn end(&mut self, out: &mut Emitter) -> Result<(), Error> {
        if let Some(text) = &self.resource {
            out.word("words", 0, 1, text, None);
        }
        if let Some(path) = &self.path {
            out.word("words", 1, 2, path, None);
        }
        self.at_end.iter().try_for_each(|item| emit(out, item))
    }
}

alpha2_plugin_sdk::export_extractor!(Scripted);

#[cfg(test)]
mod tests;
