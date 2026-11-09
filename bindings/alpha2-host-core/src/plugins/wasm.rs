//! Runs wasm plugins as core modules in wasmtime with only the `alpha2` imports. Memory is capped,
//! each call is bounded by an epoch deadline, and NaNs are canonicalized so results do not depend
//! on the CPU.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use alpha2_engine::extract::{AudioInput, ExtractorError};
use alpha2_plugin_abi::wasm as names;
use wasmtime::wasmparser::{Parser, Payload, TypeRef, Validator, WasmFeatures};
use wasmtime::{
    Caller, Config, Engine, Instance, Linker, Memory, Module, ResourceLimiter, Store, Trap,
    TypedFunc,
};

use super::{Call, Collected, PluginError, Policy, Transport, codes};

/// How often the engine's epoch advances; deadlines are counted in these.
const EPOCH_TICK: Duration = Duration::from_millis(10);
/// Compiled modules kept by artifact hash, most recent last.
const KEPT_MODULES: usize = 8;

const IMPORTS: [&str; 5] = ["emit", "resource_size", "resource_read", "fail", "log"];

fn engine() -> Result<&'static Engine, PluginError> {
    static ENGINE: OnceLock<Result<Engine, String>> = OnceLock::new();
    ENGINE
        .get_or_init(|| {
            let mut config = Config::new();
            config
                .epoch_interruption(true)
                .cranelift_nan_canonicalization(true)
                .wasm_relaxed_simd(false);
            // Apps on iOS may not generate code at run time; the interpreter runs the module.
            #[cfg(target_os = "ios")]
            config
                .target("pulley64")
                .map_err(|error| error.to_string())?;
            let engine = Engine::new(&config).map_err(|error| error.to_string())?;
            let ticking = engine.weak();
            std::thread::Builder::new()
                .name("alpha2-plugin-epoch".to_owned())
                .spawn(move || {
                    while let Some(engine) = ticking.upgrade() {
                        engine.increment_epoch();
                        drop(engine);
                        std::thread::sleep(EPOCH_TICK);
                    }
                })
                .map_err(|error| error.to_string())?;
            Ok(engine)
        })
        .as_ref()
        .map_err(|error| PluginError::new(codes::LOAD_FAILED, format!("wasmtime: {error}")))
}

/// The default features minus those whose results differ between runtimes or that need threads.
fn features() -> WasmFeatures {
    let mut features = WasmFeatures::default();
    features.remove(
        WasmFeatures::RELAXED_SIMD
            | WasmFeatures::THREADS
            | WasmFeatures::SHARED_EVERYTHING_THREADS
            | WasmFeatures::COMPONENT_MODEL,
    );
    features
}

/// # Errors
/// `PLUGIN_UNSUPPORTED_FEATURE` for a module that uses a refused feature or imports anything but
/// the `alpha2` host functions; `PLUGIN_LOAD_FAILED` for one that is not a valid module.
pub(super) fn check_module(module: &[u8]) -> Result<(), PluginError> {
    let unsupported = |why: String| PluginError::new(codes::UNSUPPORTED_FEATURE, why);
    if let Err(error) = Validator::new_with_features(features()).validate_all(module) {
        return Err(if Validator::new().validate_all(module).is_ok() {
            unsupported(format!(
                "the module uses relaxed SIMD, threads or shared memory, which plugins may not \
                 use: {error}"
            ))
        } else {
            PluginError::new(
                codes::LOAD_FAILED,
                format!("the module is not valid: {error}"),
            )
        });
    }
    for payload in Parser::new(0).parse_all(module) {
        let payload =
            payload.map_err(|error| PluginError::new(codes::LOAD_FAILED, error.to_string()))?;
        if let Payload::ImportSection(section) = payload {
            for import in section.into_imports() {
                let import = import
                    .map_err(|error| PluginError::new(codes::LOAD_FAILED, error.to_string()))?;
                if import.module != "alpha2"
                    || !IMPORTS.contains(&import.name)
                    || !matches!(import.ty, TypeRef::Func(_))
                {
                    return Err(unsupported(format!(
                        "the module imports {}.{}; plugins import only the alpha2 host functions",
                        import.module, import.name
                    )));
                }
            }
        }
    }
    Ok(())
}

fn compiled(sha256: &str, bytes: &[u8]) -> Result<Module, PluginError> {
    static MODULES: Mutex<Vec<(String, Module)>> = Mutex::new(Vec::new());
    let mut modules = MODULES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(at) = modules.iter().position(|(hash, _)| hash == sha256) {
        let kept = modules.remove(at);
        let module = kept.1.clone();
        modules.push(kept);
        return Ok(module);
    }
    let module = Module::new(engine()?, bytes)
        .map_err(|error| PluginError::new(codes::LOAD_FAILED, format!("wasmtime: {error}")))?;
    if modules.len() == KEPT_MODULES {
        modules.remove(0);
    }
    modules.push((sha256.to_owned(), module.clone()));
    Ok(module)
}

struct Limiter {
    memory_bytes: usize,
    hit: bool,
}

impl ResourceLimiter for Limiter {
    fn memory_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        let fits = desired <= self.memory_bytes;
        self.hit |= !fits;
        Ok(fits)
    }

    fn table_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        Ok(desired <= 1 << 20)
    }
}

struct State {
    limiter: Limiter,
    collected: Collected,
    resources: HashMap<String, Vec<u8>>,
}

fn memory(caller: &mut Caller<'_, State>) -> Option<Memory> {
    caller.get_export("memory")?.into_memory()
}

fn span(offset: i32, len: i32) -> Option<std::ops::Range<usize>> {
    let start = usize::try_from(offset).ok()?;
    start
        .checked_add(usize::try_from(len).ok()?)
        .map(|end| start..end)
}

fn read(caller: &mut Caller<'_, State>, offset: i32, len: i32) -> Option<Vec<u8>> {
    let memory = memory(caller)?;
    memory
        .data(&caller)
        .get(span(offset, len)?)
        .map(<[u8]>::to_vec)
}

fn linker() -> Result<Linker<State>, PluginError> {
    let failed = |error: wasmtime::Error| PluginError::new(codes::LOAD_FAILED, error.to_string());
    let mut linker = Linker::new(engine()?);
    linker
        .func_wrap(
            "alpha2",
            "emit",
            |mut caller: Caller<'_, State>, ptr: i32, len: i32| {
                if let Some(batch) = read(&mut caller, ptr, len) {
                    caller.data_mut().collected.emit(&batch)
                } else {
                    caller.data_mut().collected.undecodable =
                        Some("the records lie outside the module's memory".to_owned());
                    alpha2_plugin_abi::status::FAILED
                }
            },
        )
        .map_err(failed)?;
    linker
        .func_wrap(
            "alpha2",
            "resource_size",
            |mut caller: Caller<'_, State>, name: i32, len: i32| {
                read(&mut caller, name, len)
                    .and_then(|name| String::from_utf8(name).ok())
                    .and_then(|name| caller.data().resources.get(&name))
                    .and_then(|bytes| i64::try_from(bytes.len()).ok())
                    .unwrap_or(-1)
            },
        )
        .map_err(failed)?;
    linker
        .func_wrap(
            "alpha2",
            "resource_read",
            |mut caller: Caller<'_, State>, name: i32, len: i32, dst: i32, dst_len: i32| {
                let Some(name) =
                    read(&mut caller, name, len).and_then(|name| String::from_utf8(name).ok())
                else {
                    return 1;
                };
                let Some(memory) = memory(&mut caller) else {
                    return 1;
                };
                let (data, state) = memory.data_and_store_mut(&mut caller);
                match (state.resources.get(&name), span(dst, dst_len)) {
                    (Some(bytes), Some(range)) if range.len() == bytes.len() => {
                        match data.get_mut(range) {
                            Some(target) => {
                                target.copy_from_slice(bytes);
                                0
                            }
                            None => 1,
                        }
                    }
                    _ => 1,
                }
            },
        )
        .map_err(failed)?;
    linker
        .func_wrap(
            "alpha2",
            "fail",
            |mut caller: Caller<'_, State>, ptr: i32, len: i32| {
                if let Some(message) = read(&mut caller, ptr, len) {
                    caller.data_mut().collected.failure =
                        Some(String::from_utf8_lossy(&message).into_owned());
                }
            },
        )
        .map_err(failed)?;
    linker
        .func_wrap(
            "alpha2",
            "log",
            |mut caller: Caller<'_, State>, _level: i32, ptr: i32, len: i32| {
                if let Some(message) = read(&mut caller, ptr, len) {
                    super::log_line(&String::from_utf8_lossy(&message));
                }
            },
        )
        .map_err(failed)?;
    Ok(linker)
}

/// `(start, start_sample, frames, rate, channels, ptr, count) -> status`.
type AudioCall = TypedFunc<(i64, i64, i32, i32, i32, i32, i32), i32>;

struct Exports {
    alloc: TypedFunc<(i32, i32), i32>,
    free: TypedFunc<(i32, i32, i32), ()>,
    describe: TypedFunc<(), i64>,
    begin: TypedFunc<(i32, i32), i32>,
    pass: TypedFunc<(i32, i32), i32>,
    video: TypedFunc<(i64, i32, i32, i32, i32, i32), i32>,
    audio: AudioCall,
    end: TypedFunc<(), i32>,
}

pub(super) struct WasmTransport {
    store: Store<State>,
    memory: Memory,
    exports: Exports,
    deadline: Duration,
    memory_cap: u64,
}

fn to_i32(value: impl TryInto<i32>) -> Result<i32, ExtractorError> {
    value.try_into().map_err(|_| {
        ExtractorError::with_code(
            codes::LIMIT,
            "an argument exceeds the module's 32-bit memory",
        )
    })
}

impl WasmTransport {
    pub(super) fn new(
        id: &str,
        bytes: &[u8],
        sha256: &str,
        resources: HashMap<String, Vec<u8>>,
        policy: &Policy,
    ) -> Result<Self, PluginError> {
        check_module(bytes)?;
        let module = compiled(sha256, bytes)?;
        let failed = |error: wasmtime::Error| {
            PluginError::new(
                codes::LOAD_FAILED,
                format!("{id} did not instantiate: {error}"),
            )
        };
        let mut store = Store::new(
            engine()?,
            State {
                limiter: Limiter {
                    memory_bytes: usize::try_from(policy.memory_bytes).unwrap_or(usize::MAX),
                    hit: false,
                },
                collected: Collected::default(),
                resources,
            },
        );
        store.limiter(|state| &mut state.limiter);
        store.epoch_deadline_trap();
        store.set_epoch_deadline(ticks(policy.call_deadline));
        let instance: Instance = linker()?.instantiate(&mut store, &module).map_err(failed)?;
        let memory = instance
            .get_memory(&mut store, names::MEMORY)
            .ok_or_else(|| PluginError::new(codes::LOAD_FAILED, "the module exports no memory"))?;
        let abi = instance
            .get_typed_func::<(), i32>(&mut store, names::ABI)
            .map_err(failed)?
            .call(&mut store, ())
            .map_err(failed)?;
        if u32::try_from(abi) != Ok(alpha2_plugin_abi::ABI_MAJOR) {
            return Err(PluginError::new(
                codes::ABI_UNSUPPORTED,
                format!("the module implements alpha2-plugin/{abi}"),
            ));
        }
        macro_rules! export {
            ($name:expr) => {
                instance.get_typed_func(&mut store, $name).map_err(failed)?
            };
        }
        let exports = Exports {
            alloc: export!(names::ALLOC),
            free: export!(names::FREE),
            describe: export!(names::DESCRIBE),
            begin: export!(names::BEGIN),
            pass: export!(names::PASS),
            video: export!(names::VIDEO),
            audio: export!(names::AUDIO),
            end: export!(names::END),
        };
        Ok(Self {
            store,
            memory,
            exports,
            deadline: policy.call_deadline,
            memory_cap: policy.memory_bytes,
        })
    }

    /// Why a call failed: past its memory cap or deadline, or trapped.
    fn failure(&mut self, error: &wasmtime::Error) -> ExtractorError {
        let state = self.store.data_mut();
        state.collected = Collected::default();
        if std::mem::take(&mut state.limiter.hit) {
            ExtractorError::with_code(
                codes::LIMIT,
                format!(
                    "the plugin needed more than {} bytes of memory",
                    self.memory_cap
                ),
            )
        } else if matches!(error.downcast_ref::<Trap>(), Some(Trap::Interrupt)) {
            ExtractorError::with_code(
                codes::LIMIT,
                format!(
                    "a plugin call ran longer than {} ms",
                    self.deadline.as_millis()
                ),
            )
        } else {
            ExtractorError::with_code(codes::TRAPPED, format!("the plugin trapped: {error:#}"))
        }
    }

    /// Runs `call` with a fresh deadline and turns its status into the call's outcome.
    fn run(&mut self, call: impl FnOnce(&mut Self) -> wasmtime::Result<i32>) -> Call {
        self.store.set_epoch_deadline(ticks(self.deadline));
        match call(self) {
            Ok(status) => self.store.data_mut().collected.outcome(status),
            Err(error) => Err(self.failure(&error)),
        }
    }

    /// Copies `bytes` into memory the module allocates, runs `call` with its offset and frees it.
    fn with_bytes(
        &mut self,
        bytes: &[u8],
        align: i32,
        call: impl FnOnce(&mut Self, i32) -> wasmtime::Result<i32>,
    ) -> Call {
        let len = to_i32(bytes.len())?;
        self.store.set_epoch_deadline(ticks(self.deadline));
        let alloc = self.exports.alloc.clone();
        let offset = match alloc.call(&mut self.store, (len, align)) {
            Ok(0) if len > 0 => {
                let error = wasmtime::Error::msg("the module could not allocate the call's input");
                return Err(self.failure(&error));
            }
            Ok(offset) => offset,
            Err(error) => return Err(self.failure(&error)),
        };
        let start = usize::try_from(offset).unwrap_or(usize::MAX);
        if let Err(error) = self.memory.write(&mut self.store, start, bytes) {
            return Err(self.failure(&wasmtime::Error::msg(error.to_string())));
        }
        let outcome = self.run(|this| call(this, offset));
        let free = self.exports.free.clone();
        if outcome.is_ok()
            && let Err(error) = free.call(&mut self.store, (offset, len, align))
        {
            return Err(self.failure(&error));
        }
        outcome
    }
}

fn ticks(deadline: Duration) -> u64 {
    u64::try_from(deadline.as_millis() / EPOCH_TICK.as_millis())
        .unwrap_or(u64::MAX)
        .max(1)
}

impl Transport for WasmTransport {
    fn describe(&mut self) -> Result<String, PluginError> {
        let failed = |why: String| PluginError::new(codes::DESCRIPTOR_MISMATCH, why);
        let describe = self.exports.describe.clone();
        let packed = describe
            .call(&mut self.store, ())
            .map_err(|error| failed(format!("describe trapped: {error}")))?
            .cast_unsigned();
        let start = usize::try_from(packed & 0xffff_ffff).unwrap_or(usize::MAX);
        let len = usize::try_from(packed >> 32).unwrap_or(usize::MAX);
        let bytes = start
            .checked_add(len)
            .and_then(|end| self.memory.data(&self.store).get(start..end))
            .ok_or_else(|| failed("the descriptor lies outside the module's memory".to_owned()))?;
        String::from_utf8(bytes.to_vec())
            .map_err(|_| failed("the descriptor is not UTF-8".to_owned()))
    }

    fn begin(&mut self, context: &[u8]) -> Call {
        let len = to_i32(context.len())?;
        self.with_bytes(context, 1, |this, offset| {
            let begin = this.exports.begin.clone();
            begin.call(&mut this.store, (offset, len))
        })
    }

    fn pass(&mut self, pass: u32, direction: u32) -> Call {
        let (pass, direction) = (pass.cast_signed(), direction.cast_signed());
        self.run(|this| {
            let call = this.exports.pass.clone();
            call.call(&mut this.store, (pass, direction))
        })
    }

    fn video(&mut self, time: i64, width: u32, height: u32, format: u32, data: &[u8]) -> Call {
        let len = to_i32(data.len())?;
        let (width, height, format) = (to_i32(width)?, to_i32(height)?, to_i32(format)?);
        self.with_bytes(data, 1, |this, offset| {
            let video = this.exports.video.clone();
            video.call(&mut this.store, (time, width, height, format, offset, len))
        })
    }

    fn audio(&mut self, block: &AudioInput) -> Call {
        let count = to_i32(block.samples.len())?;
        let (frames, rate, channels) = (
            to_i32(block.frames)?,
            to_i32(block.rate)?,
            to_i32(block.channels)?,
        );
        let bytes = block
            .samples
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect::<Vec<_>>();
        let (start, start_sample) = (block.start.ticks(), block.start_sample);
        self.with_bytes(&bytes, 4, |this, offset| {
            let audio = this.exports.audio.clone();
            audio.call(
                &mut this.store,
                (start, start_sample, frames, rate, channels, offset, count),
            )
        })
    }

    fn end(&mut self) -> Call {
        self.run(|this| {
            let end = this.exports.end.clone();
            end.call(&mut this.store, ())
        })
    }
}
