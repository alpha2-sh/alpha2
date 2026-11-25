//! Checks that a file is a whole whisper.cpp ggml model before whisper.cpp loads it.

use std::io::{BufReader, Read, Seek};
use std::path::Path;

use whisper_rs::whisper_rs_sys::{ggml_blck_size, ggml_type_GGML_TYPE_COUNT, ggml_type_size};
use whisper_rs::{DtwMode, DtwModelPreset};

/// `GGML_FILE_MAGIC`, the first four bytes of a whisper.cpp model, little-endian.
const GGML_MAGIC: u32 = 0x6767_6d6c;
/// The audio context of every whisper model: 30 s in 20 ms steps.
const AUDIO_CONTEXT: i32 = 1_500;
/// Vocabulary sizes of whisper models: English-only, multilingual, and large-v3's.
const VOCABULARIES: std::ops::RangeInclusive<i32> = 51_864..=51_866;
/// Mel bands whisper models read: 80, or 128 from large-v3 on.
const MEL_BANDS: [i32; 2] = [80, 128];
const MAX_LAYERS: i32 = 256;
const MAX_STATE: i32 = 1 << 16;
const MAX_TOKEN_BYTES: u32 = 1 << 12;
const MAX_NAME_BYTES: i32 = 1 << 12;
const MAX_FILTER_VALUES: i64 = 1 << 20;

/// The smallest vocabulary of a multilingual whisper model; English-only ones have one token less.
const MULTILINGUAL_VOCABULARY: i32 = 51_865;
/// The vocabulary of large-v3 and the models made from it.
const LARGE_V3_VOCABULARY: i32 = 51_866;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub vocabulary: i32,
    pub audio_layers: i32,
    pub text_layers: i32,
}

impl Header {
    pub fn multilingual(self) -> bool {
        self.vocabulary >= MULTILINGUAL_VOCABULARY
    }

    /// The cross-attention heads whisper.cpp aligns tokens with: the preset of the published model
    /// of this shape, or else the upper half of the decoder's layers.
    pub fn alignment(self) -> DtwMode<'static> {
        let preset = match (self.audio_layers, self.text_layers, self.multilingual()) {
            (4, 4, false) => DtwModelPreset::TinyEn,
            (4, 4, true) => DtwModelPreset::Tiny,
            (6, 6, false) => DtwModelPreset::BaseEn,
            (6, 6, true) => DtwModelPreset::Base,
            (12, 12, false) => DtwModelPreset::SmallEn,
            (12, 12, true) => DtwModelPreset::Small,
            (24, 24, false) => DtwModelPreset::MediumEn,
            (24, 24, true) => DtwModelPreset::Medium,
            (32, 32, true) if self.vocabulary == LARGE_V3_VOCABULARY => DtwModelPreset::LargeV3,
            (32, 32, true) => DtwModelPreset::LargeV2,
            (32, 4, true) if self.vocabulary == LARGE_V3_VOCABULARY => DtwModelPreset::LargeV3Turbo,
            _ => {
                return DtwMode::TopMost {
                    n_top: (self.text_layers / 2).max(1),
                };
            }
        };
        DtwMode::ModelPreset {
            model_preset: preset,
        }
    }
}

struct Reader<R> {
    inner: R,
    at: u64,
    length: u64,
}

impl<R: Read + Seek> Reader<R> {
    fn i32(&mut self, what: &str) -> Result<i32, String> {
        let mut bytes = [0; 4];
        self.inner
            .read_exact(&mut bytes)
            .map_err(|_| format!("it ends in {what}"))?;
        self.at += 4;
        Ok(i32::from_le_bytes(bytes))
    }

    fn skip(&mut self, bytes: u64, what: &str) -> Result<(), String> {
        if self.length - self.at < bytes {
            return Err(format!("it ends in {what}"));
        }
        let step = i64::try_from(bytes).map_err(|_| format!("{what} is too long"))?;
        self.inner
            .seek_relative(step)
            .map_err(|error| error.to_string())?;
        self.at += bytes;
        Ok(())
    }
}

fn whisper_header<R: Read + Seek>(file: &mut Reader<R>) -> Result<Header, String> {
    let magic = file.i32("the magic")?.cast_unsigned();
    if magic != GGML_MAGIC {
        let bytes = magic.to_le_bytes();
        return Err(if &bytes == b"GGUF" {
            "it is a GGUF file; whisper.cpp reads models in the older ggml format (ggml-*.bin)"
                .to_owned()
        } else {
            "it does not start with the ggml magic of a whisper.cpp model (ggml-*.bin)".to_owned()
        });
    }
    let mut values = [0; 11];
    for value in &mut values {
        *value = file.i32("the header")?;
    }
    let [
        vocabulary,
        audio_context,
        audio_state,
        audio_heads,
        audio_layers,
        text_context,
        text_state,
        text_heads,
        text_layers,
        mels,
        _,
    ] = values;
    if audio_context != AUDIO_CONTEXT {
        return Err(format!(
            "its audio context is {audio_context}, not whisper's {AUDIO_CONTEXT}"
        ));
    }
    if !VOCABULARIES.contains(&vocabulary) {
        return Err(format!(
            "its vocabulary has {vocabulary} tokens, not whisper's {} to {}",
            VOCABULARIES.start(),
            VOCABULARIES.end()
        ));
    }
    if !MEL_BANDS.contains(&mels) {
        return Err(format!("it reads {mels} mel bands, not 80 or 128"));
    }
    let sizes_fit = [audio_layers, text_layers]
        .iter()
        .all(|layers| (1..=MAX_LAYERS).contains(layers))
        && [
            audio_state,
            audio_heads,
            text_context,
            text_state,
            text_heads,
        ]
        .iter()
        .all(|size| (1..=MAX_STATE).contains(size));
    if !sizes_fit {
        return Err("its layer sizes are not those of a whisper model".to_owned());
    }
    Ok(Header {
        vocabulary,
        audio_layers,
        text_layers,
    })
}

fn filters_and_vocabulary<R: Read + Seek>(file: &mut Reader<R>) -> Result<(), String> {
    let bands = i64::from(file.i32("the mel filters")?);
    let bins = i64::from(file.i32("the mel filters")?);
    if !(1..=MAX_FILTER_VALUES).contains(&(bands * bins)) || bands < 1 {
        return Err("its mel filters have an impossible size".to_owned());
    }
    file.skip(
        u64::try_from(bands * bins * 4).unwrap_or(u64::MAX),
        "the mel filters",
    )?;
    let tokens = file.i32("the vocabulary")?;
    if !(0..=*VOCABULARIES.end()).contains(&tokens) {
        return Err(format!("its vocabulary lists {tokens} tokens"));
    }
    for _ in 0..tokens {
        let length = file.i32("the vocabulary")?.cast_unsigned();
        if length > MAX_TOKEN_BYTES {
            return Err("a token of its vocabulary has an impossible length".to_owned());
        }
        file.skip(u64::from(length), "the vocabulary")?;
    }
    Ok(())
}

fn tensor_bytes(ggml_type: i32, elements: i64) -> Option<u64> {
    let ggml_type = u32::try_from(ggml_type)
        .ok()
        .filter(|&id| id < ggml_type_GGML_TYPE_COUNT)?;
    // SAFETY: ggml's type tables cover every id below `GGML_TYPE_COUNT`.
    let (size, block) = unsafe { (ggml_type_size(ggml_type), ggml_blck_size(ggml_type)) };
    if size == 0 || block <= 0 || elements % block != 0 {
        return None;
    }
    u64::try_from(elements / block)
        .ok()?
        .checked_mul(u64::try_from(size).ok()?)
}

fn tensors<R: Read + Seek>(file: &mut Reader<R>) -> Result<(), String> {
    let mut count = 0_usize;
    while file.at < file.length {
        let dimensions = file.i32("a tensor's header")?;
        let name_bytes = file.i32("a tensor's header")?;
        let ggml_type = file.i32("a tensor's header")?;
        if !(1..=4).contains(&dimensions) || !(1..=MAX_NAME_BYTES).contains(&name_bytes) {
            return Err(format!("tensor {count} has an impossible header"));
        }
        let mut elements = 1_i64;
        for _ in 0..dimensions {
            let extent = file.i32("a tensor's header")?;
            if extent < 1 {
                return Err(format!("tensor {count} has an impossible shape"));
            }
            elements = elements.saturating_mul(i64::from(extent));
        }
        let mut name = vec![0; usize::try_from(name_bytes).unwrap_or(0)];
        file.inner
            .read_exact(&mut name)
            .map_err(|_| "it ends in a tensor's name".to_owned())?;
        file.at += u64::from(name_bytes.cast_unsigned());
        let name = String::from_utf8_lossy(&name).into_owned();
        let bytes = tensor_bytes(ggml_type, elements).ok_or_else(|| {
            format!("tensor {name} has type {ggml_type}, which ggml does not know")
        })?;
        file.skip(bytes, &format!("tensor {name}"))?;
        count += 1;
    }
    if count == 0 {
        return Err("it holds no tensors".to_owned());
    }
    Ok(())
}

/// Checks that `path` is a whole whisper.cpp ggml model and returns its header.
///
/// # Errors
/// What is wrong with the file, as a sentence about "it".
pub fn check(path: &Path) -> Result<Header, String> {
    let file =
        std::fs::File::open(path).map_err(|error| format!("it cannot be opened: {error}"))?;
    let length = file
        .metadata()
        .map_err(|error| format!("it cannot be read: {error}"))?
        .len();
    let mut reader = Reader {
        inner: BufReader::new(file),
        at: 0,
        length,
    };
    let header = whisper_header(&mut reader)?;
    filters_and_vocabulary(&mut reader)?;
    tensors(&mut reader)?;
    Ok(header)
}
