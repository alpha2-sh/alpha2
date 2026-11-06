//! Audio decoding through `AudioToolbox` converters, and Vorbis, which `AudioToolbox` lacks,
//! through `lewton`.
//!
//! Decoders keep their priming: the container's skip-samples are applied by the host-core stream,
//! as `FFmpeg` applies them.

// Every call into the media frameworks is FFI.
#![allow(unsafe_code)]

use std::collections::VecDeque;
use std::ffi::c_void;
use std::ptr::{self, NonNull};

use alpha2_container::{AudioCodec, AudioTrack, Packet, TrackInfo, vorbis_order};
use alpha2_host_core::audio_stream::{AudioBackends, AudioDecodeBackend, AudioPoll, PcmChunk};
use alpha2_host_core::video_stream::{BackendError, Submit};
use alpha2_media::{Speaker, SpeakerLayout};
use objc2_audio_toolbox::{
    AudioConverterDispose, AudioConverterFillComplexBuffer, AudioConverterGetProperty,
    AudioConverterGetPropertyInfo, AudioConverterNew, AudioConverterRef, AudioConverterSetProperty,
    AudioFormatGetProperty, AudioFormatGetPropertyInfo, AudioFormatInfo,
    kAudioConverterDecompressionMagicCookie, kAudioConverterOutputChannelLayout,
    kAudioFormatProperty_AvailableDecodeNumberChannels,
    kAudioFormatProperty_ChannelLayoutForBitmap, kAudioFormatProperty_ChannelLayoutForTag,
    kAudioFormatProperty_FormatInfo, kAudioFormatProperty_FormatList,
};
use objc2_core_audio_types::{
    AudioBuffer, AudioBufferList, AudioFormatListItem, AudioStreamBasicDescription,
    AudioStreamPacketDescription,
};

use crate::config::{eac3_packet_frames, es_descriptor, mp3_packet_frames, opus_packet_frames};
use crate::sys::{self, code};

/// What the input callback returns when it has no packet now: the converter stops and keeps its
/// state for the next call.
pub(crate) const NO_INPUT_NOW: i32 = code(*b"wait").cast_signed();

/// `kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked`.
const FLOAT_PACKED: u32 = 1 | 8;

/// `kAudioChannelLayoutTag_UseChannelDescriptions` and `..._UseChannelBitmap`.
const USE_DESCRIPTIONS: u32 = 0;
const USE_BITMAP: u32 = 1 << 16;

/// The packet queue the converter reads from.
struct Feed {
    queued: VecDeque<Vec<u8>>,
    /// The packet handed to the converter last, which must stay valid until it asks again.
    current: Vec<u8>,
    description: AudioStreamPacketDescription,
    channels: u32,
    end: bool,
}

unsafe extern "C-unwind" fn feed(
    _converter: AudioConverterRef,
    packets: NonNull<u32>,
    buffers: NonNull<AudioBufferList>,
    descriptions: *mut *mut AudioStreamPacketDescription,
    user: *mut c_void,
) -> i32 {
    // SAFETY: the user data is the session's `Feed`, borrowed for the converter call.
    let feed = unsafe { &mut *user.cast::<Feed>() };
    let Some(packet) = feed.queued.pop_front() else {
        // SAFETY: the converter passes valid pointers.
        unsafe { packets.write(0) };
        return if feed.end { 0 } else { NO_INPUT_NOW };
    };
    feed.current = packet;
    let length = u32::try_from(feed.current.len()).unwrap_or(u32::MAX);
    // SAFETY: the converter passes a list with one buffer and a valid packet count.
    unsafe {
        packets.write(1);
        let list = buffers.as_ptr();
        (*list).mNumberBuffers = 1;
        (*list).mBuffers[0] = AudioBuffer {
            mNumberChannels: feed.channels,
            mDataByteSize: length,
            mData: feed.current.as_mut_ptr().cast(),
        };
    }
    feed.description = AudioStreamPacketDescription {
        mStartOffset: 0,
        mVariableFramesInPacket: 0,
        mDataByteSize: length,
    };
    if !descriptions.is_null() {
        // SAFETY: a non-null pointer asks for this packet's description, which lives in `feed`.
        unsafe { descriptions.write(&raw mut feed.description) };
    }
    0
}

/// An `AudioToolbox` converter, disposed when dropped.
pub(crate) struct Converter(pub(crate) AudioConverterRef);

// SAFETY: a converter may be called from any thread; the session owning it is used from one
// thread at a time.
unsafe impl Send for Converter {}

impl Drop for Converter {
    fn drop(&mut self) {
        // SAFETY: the converter is live and nothing uses it after this.
        unsafe { AudioConverterDispose(self.0) };
    }
}

/// The speaker an `AudioChannelLabel` names; `surround_direct` tells whether the layout also has
/// the direct surround pair, which makes the plain surround pair the back pair.
const fn speaker(label: u32, surround_direct: bool) -> Option<Speaker> {
    Some(match label {
        1 => Speaker::FrontLeft,
        2 => Speaker::FrontRight,
        3 | 42 => Speaker::FrontCenter,
        4 => Speaker::LowFrequency,
        5 if surround_direct => Speaker::BackLeft,
        6 if surround_direct => Speaker::BackRight,
        5 | 10 => Speaker::SideLeft,
        6 | 11 => Speaker::SideRight,
        7 => Speaker::FrontLeftOfCenter,
        8 => Speaker::FrontRightOfCenter,
        9 => Speaker::BackCenter,
        12 => Speaker::TopCenter,
        13 => Speaker::TopFrontLeft,
        14 => Speaker::TopFrontCenter,
        15 => Speaker::TopFrontRight,
        16 => Speaker::TopBackLeft,
        17 => Speaker::TopBackCenter,
        18 => Speaker::TopBackRight,
        33 => Speaker::BackLeft,
        34 => Speaker::BackRight,
        35 => Speaker::WideLeft,
        36 => Speaker::WideRight,
        _ => return None,
    })
}

/// The labels of a variable-length `AudioChannelLayout` in `bytes`, expanding a tag or bitmap.
fn layout_labels(bytes: &[u8]) -> Option<Vec<u32>> {
    let word = |at: usize| {
        bytes
            .get(at..at + 4)
            .map(|word| u32::from_ne_bytes([word[0], word[1], word[2], word[3]]))
    };
    let (tag, bitmap) = (word(0)?, word(4)?);
    if tag == USE_DESCRIPTIONS {
        return descriptions(bytes);
    }
    let (property, specifier) = if tag == USE_BITMAP {
        (kAudioFormatProperty_ChannelLayoutForBitmap, bitmap)
    } else {
        (kAudioFormatProperty_ChannelLayoutForTag, tag)
    };
    descriptions(&format_property(property, &specifier.to_ne_bytes())?)
}

/// The labels of a layout's channel descriptions (label, flags, three coordinates each); `None`
/// when it has none.
fn descriptions(bytes: &[u8]) -> Option<Vec<u32>> {
    let word = |at: usize| {
        bytes
            .get(at..at + 4)
            .map(|word| u32::from_ne_bytes([word[0], word[1], word[2], word[3]]))
    };
    let count = word(8)? as usize;
    (count > 0)
        .then(|| (0..count).map(|index| word(12 + index * 20)).collect())
        .flatten()
}

/// An `AudioFormatGetProperty` result of any size.
pub(crate) fn format_property(property: u32, specifier: &[u8]) -> Option<Vec<u8>> {
    let specifier_size = u32::try_from(specifier.len()).ok()?;
    let mut size = 0_u32;
    // SAFETY: `specifier` holds `specifier_size` bytes; `size` is a valid out pointer.
    let status = unsafe {
        AudioFormatGetPropertyInfo(
            property,
            specifier_size,
            specifier.as_ptr().cast(),
            NonNull::from(&mut size),
        )
    };
    if status != 0 || size == 0 {
        return None;
    }
    let mut out = vec![0_u8; size as usize];
    // SAFETY: `out` holds `size` bytes.
    let status = unsafe {
        AudioFormatGetProperty(
            property,
            specifier_size,
            specifier.as_ptr().cast(),
            &raw mut size,
            out.as_mut_ptr().cast(),
        )
    };
    (status == 0).then(|| {
        out.truncate(size as usize);
        out
    })
}

/// The converter's output channel layout, as channel labels.
fn output_labels(converter: AudioConverterRef) -> Option<Vec<u32>> {
    let mut size = 0_u32;
    // SAFETY: valid out pointers; the converter is live.
    let status = unsafe {
        AudioConverterGetPropertyInfo(
            converter,
            kAudioConverterOutputChannelLayout,
            &raw mut size,
            ptr::null_mut(),
        )
    };
    if status != 0 || size < 12 {
        return None;
    }
    let mut bytes = vec![0_u8; size as usize];
    // SAFETY: `bytes` holds `size` bytes.
    let status = unsafe {
        AudioConverterGetProperty(
            converter,
            kAudioConverterOutputChannelLayout,
            NonNull::from(&mut size),
            NonNull::new(bytes.as_mut_ptr().cast())?,
        )
    };
    (status == 0).then(|| layout_labels(&bytes)).flatten()
}

/// Asks the converter for its channels in `FFmpeg`'s native order and returns the speakers of
/// the output planes. `speaker_of` names the speaker of each channel label.
///
/// The output layout is set rather than read: some decoders report their input's layout while
/// their planes come in another order, and a layout set on the converter makes it map the
/// channels into that order.
fn order_output(
    converter: AudioConverterRef,
    channels: usize,
    speaker_of: impl Fn(u32, &[u32]) -> Option<Speaker>,
) -> Option<SpeakerLayout> {
    let labels = output_labels(converter)?;
    let mut named = labels
        .iter()
        .map(|label| Some((speaker_of(*label, &labels)?, *label)))
        .collect::<Option<Vec<_>>>()?;
    if named.len() != channels {
        return None;
    }
    named.sort_by_key(|(speaker, _)| speaker.ffmpeg_channel());
    // An `AudioChannelLayout` with descriptions: tag, bitmap, count, then label, flags and three
    // coordinates per channel.
    let mut layout = Vec::with_capacity(12 + 20 * channels);
    layout.extend_from_slice(&USE_DESCRIPTIONS.to_ne_bytes());
    layout.extend_from_slice(&0_u32.to_ne_bytes());
    layout.extend_from_slice(&u32::try_from(channels).ok()?.to_ne_bytes());
    for (_, label) in &named {
        layout.extend_from_slice(&label.to_ne_bytes());
        layout.extend_from_slice(&[0; 16]);
    }
    // SAFETY: `layout` is a complete channel layout of its length.
    let status = unsafe {
        AudioConverterSetProperty(
            converter,
            kAudioConverterOutputChannelLayout,
            u32::try_from(layout.len()).ok()?,
            NonNull::new(layout.as_mut_ptr().cast())?,
        )
    };
    let labels = if status == 0 {
        output_labels(converter)?
    } else {
        labels
    };
    let speakers = labels
        .iter()
        .map(|label| speaker_of(*label, &labels))
        .collect::<Option<Vec<_>>>()?;
    (speakers.len() == channels)
        .then(|| SpeakerLayout::new(&speakers))
        .flatten()
}

/// The channel counts the system's decoder for `input` outputs; empty when it does not say.
///
/// Some systems' decoders take more channels than they output: the AAC decoder in the iOS
/// simulator opens 5.1 and 7.1 converters but downmixes them to two planes.
fn decode_channels(input: &AudioStreamBasicDescription) -> Vec<u32> {
    // SAFETY: the description is plain data read as bytes.
    let specifier = unsafe {
        std::slice::from_raw_parts(
            ptr::from_ref(input).cast::<u8>(),
            size_of::<AudioStreamBasicDescription>(),
        )
    };
    format_property(
        kAudioFormatProperty_AvailableDecodeNumberChannels,
        specifier,
    )
    .map(|bytes| {
        bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|word| u32::from_ne_bytes(*word))
            .collect()
    })
    .unwrap_or_default()
}

/// A codec's name for messages.
fn codec_name(codec: AudioCodec) -> String {
    match codec {
        AudioCodec::Aac => "AAC".into(),
        AudioCodec::Mp3 => "MP3".into(),
        AudioCodec::Opus => "Opus".into(),
        AudioCodec::Vorbis => "Vorbis".into(),
        AudioCodec::Flac => "FLAC".into(),
        AudioCodec::Alac => "ALAC".into(),
        AudioCodec::Pcm(_) => "PCM".into(),
        AudioCodec::Ac3 => "AC-3".into(),
        AudioCodec::Eac3 => "E-AC-3".into(),
        AudioCodec::Dts => "DTS".into(),
        AudioCodec::Other(fourcc) => format!("'{fourcc}'"),
    }
}

/// The compressed format and magic cookie `AudioToolbox` decodes `audio` from.
fn input_format(
    audio: &AudioTrack,
) -> Result<(AudioStreamBasicDescription, Vec<u8>), BackendError> {
    let private = &audio.codec_private[..];
    let (id, cookie, frames) = match audio.codec {
        AudioCodec::Aac => (code(*b"aac "), es_descriptor(0x40, 5, private), 1_024),
        AudioCodec::Mp3 => (
            code(*b".mp3"),
            Vec::new(),
            if audio.sample_rate >= 32_000 {
                1_152
            } else {
                576
            },
        ),
        AudioCodec::Alac => (code(*b"alac"), private.to_vec(), 0),
        AudioCodec::Flac => (code(*b"flac"), flac_cookie(private), 0),
        AudioCodec::Opus => (code(*b"opus"), private.to_vec(), 960),
        AudioCodec::Ac3 => (code(*b"ac-3"), Vec::new(), 1_536),
        AudioCodec::Eac3 => (code(*b"ec-3"), Vec::new(), 1_536),
        AudioCodec::Vorbis | AudioCodec::Pcm(_) | AudioCodec::Dts | AudioCodec::Other(_) => {
            return Err(BackendError::Unsupported(format!(
                "AudioToolbox has no {} decoder",
                codec_name(audio.codec)
            )));
        }
    };
    let mut format = AudioStreamBasicDescription {
        mSampleRate: if audio.codec == AudioCodec::Opus {
            48_000.0
        } else {
            f64::from(audio.sample_rate)
        },
        mFormatID: id,
        mFormatFlags: 0,
        mBytesPerPacket: 0,
        mFramesPerPacket: frames,
        mBytesPerFrame: 0,
        mChannelsPerFrame: audio.channels,
        mBitsPerChannel: 0,
        mReserved: 0,
    };
    let cookie_size = u32::try_from(cookie.len()).unwrap_or(0);
    let mut size = u32::try_from(size_of::<AudioStreamBasicDescription>()).unwrap_or(0);
    // `FormatInfo` completes the description from the cookie; a failure keeps what is set.
    // SAFETY: `cookie` holds `cookie_size` bytes and `format` is a full description.
    let _ = unsafe {
        AudioFormatGetProperty(
            kAudioFormatProperty_FormatInfo,
            cookie_size,
            cookie.as_ptr().cast(),
            &raw mut size,
            (&raw mut format).cast(),
        )
    };
    // The format list's first entry is the richest layer the decoder outputs (HE-AAC's SBR rate).
    if let Some(cookie_ptr) =
        NonNull::new(cookie.as_ptr().cast_mut().cast::<c_void>()).filter(|_| !cookie.is_empty())
    {
        let info = AudioFormatInfo {
            mASBD: format,
            mMagicCookie: cookie_ptr,
            mMagicCookieSize: cookie_size,
        };
        // SAFETY: `AudioFormatInfo` is plain data read as bytes.
        let info_bytes = unsafe {
            std::slice::from_raw_parts((&raw const info).cast::<u8>(), size_of::<AudioFormatInfo>())
        };
        if let Some(list) = format_property(kAudioFormatProperty_FormatList, info_bytes)
            && list.len() >= size_of::<AudioFormatListItem>()
        {
            // SAFETY: the list holds at least one item.
            let first = unsafe { list.as_ptr().cast::<AudioFormatListItem>().read_unaligned() };
            if first.mASBD.mFormatID != 0 && first.mASBD.mSampleRate > 0.0 {
                format = first.mASBD;
            }
        }
    }
    if format.mChannelsPerFrame == 0 {
        format.mChannelsPerFrame = audio.channels;
    }
    if audio.codec == AudioCodec::Flac
        && let Some((block, depth)) = flac_stream_info(private)
    {
        // The FLAC decoder takes the block size as frames per packet and the source depth as
        // the Apple Lossless depth flag.
        format.mFramesPerPacket = block;
        format.mFormatFlags = match depth {
            16 => 1,
            20 => 2,
            24 => 3,
            32 => 4,
            _ => 0,
        };
        format.mBitsPerChannel = depth;
    }
    Ok((format, cookie))
}

/// The largest block size and the bits per sample a FLAC `STREAMINFO` declares.
fn flac_stream_info(private: &[u8]) -> Option<(u32, u32)> {
    let blocks = private.strip_prefix(b"fLaC").unwrap_or(private);
    // The first metadata block is STREAMINFO: a 4-byte header, then minimum and maximum block
    // size (16 bits each), frame sizes (24 bits each), rate (20), channels (3), depth (5).
    let info = blocks.get(4..22)?;
    let block = u32::from(u16::from_be_bytes([info[2], info[3]]));
    let depth = (u32::from(info[12] & 1) << 4 | u32::from(info[13] >> 4)) + 1;
    Some((block, depth))
}

/// The magic cookie `AudioToolbox` takes for FLAC, as its encoder writes it: a whole `dfLa` box
/// holding the `STREAMINFO` block flagged as the last metadata block.
fn flac_cookie(private: &[u8]) -> Vec<u8> {
    let blocks = private.strip_prefix(b"fLaC").unwrap_or(private);
    let Some(info) = blocks.get(4..38) else {
        return Vec::new();
    };
    let mut cookie = Vec::with_capacity(50);
    cookie.extend_from_slice(&50_u32.to_be_bytes());
    cookie.extend_from_slice(b"dfLa");
    cookie.extend_from_slice(&[0; 4]); // version and flags
    cookie.extend_from_slice(&[0x80, 0, 0, 34]); // last block, STREAMINFO, 34 bytes
    cookie.extend_from_slice(info);
    cookie
}

/// An `AudioToolbox` audio decoder session.
pub struct AudioToolboxAudio {
    converter: Converter,
    /// The compressed and decoded formats, to make a fresh converter on a flush.
    formats: (AudioStreamBasicDescription, AudioStreamBasicDescription),
    cookie: Vec<u8>,
    codec: AudioCodec,
    timescale: i64,
    rate: u32,
    channels: usize,
    speakers: Option<SpeakerLayout>,
    feed: Box<Feed>,
    framing: Framing,
    /// Packets fed whose samples are not all out yet: their time and decoded length, when known.
    packets: VecDeque<(i64, Option<usize>)>,
    /// Decoded samples not yet handed out, one plane per channel.
    decoded: Vec<VecDeque<f32>>,
    /// Where the samples handed out last end, in the track's timescale.
    next_pts: Option<i64>,
    out: VecDeque<PcmChunk>,
    output: Vec<f32>,
    drained: bool,
}

/// Reads the number of samples a packet decodes to from its header.
type PacketFrames = fn(&[u8]) -> Option<usize>;

/// How a decoder's output lines up with the packets fed to it.
#[derive(Clone, Copy, Debug)]
struct Framing {
    /// Samples the decoder drops from the start of its first packet. They are put back as
    /// silence, since the container's skip-samples, which cover the codec's delay, drop them.
    trimmed: usize,
    /// Samples a packet decodes to, so output held back by the decoder is still handed out
    /// with its own packet; `None` hands out what each packet produces.
    frames: Option<PacketFrames>,
}

impl Framing {
    /// `AudioToolbox`'s Opus decoder drops 120 samples, its AC-3 and E-AC-3 decoders drop 256
    /// and hold back 32, and its MP3 decoder drops its 529-sample delay; the others (AAC and
    /// FLAC among them) drop nothing and hold nothing back.
    fn of(codec: AudioCodec) -> Self {
        match codec {
            AudioCodec::Opus => Self {
                trimmed: 120,
                frames: Some(opus_packet_frames),
            },
            AudioCodec::Ac3 => Self {
                trimmed: 256,
                frames: Some(|_| Some(1_536)),
            },
            AudioCodec::Eac3 => Self {
                trimmed: 256,
                frames: Some(eac3_packet_frames),
            },
            AudioCodec::Mp3 => Self {
                trimmed: 529,
                frames: Some(mp3_packet_frames),
            },
            _ => Self {
                trimmed: 0,
                frames: None,
            },
        }
    }
}

impl std::fmt::Debug for AudioToolboxAudio {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AudioToolboxAudio")
            .field("codec", &self.codec)
            .finish_non_exhaustive()
    }
}

/// Output frames asked for per converter call: more than any packet holds.
const OUTPUT_FRAMES: u32 = 16_384;

/// A converter from `input` to `output` with `cookie` set.
fn converter(
    codec: AudioCodec,
    mut input: AudioStreamBasicDescription,
    mut output: AudioStreamBasicDescription,
    cookie: &[u8],
) -> Result<Converter, BackendError> {
    let mut converter: AudioConverterRef = ptr::null_mut();
    // SAFETY: both descriptions are complete and `converter` is a valid out pointer.
    let status = unsafe {
        AudioConverterNew(
            NonNull::from(&mut input),
            NonNull::from(&mut output),
            NonNull::from(&mut converter),
        )
    };
    if status != 0 || converter.is_null() {
        return Err(BackendError::Unsupported(format!(
            "AudioToolbox has no {} decoder for {} channels at {} Hz ({})",
            codec_name(codec),
            input.mChannelsPerFrame,
            input.mSampleRate,
            sys::status(status)
        )));
    }
    let converter = Converter(converter);
    if let Some(cookie_ptr) =
        NonNull::new(cookie.as_ptr().cast_mut().cast::<c_void>()).filter(|_| !cookie.is_empty())
    {
        // SAFETY: the cookie holds its length in bytes.
        let status = unsafe {
            AudioConverterSetProperty(
                converter.0,
                kAudioConverterDecompressionMagicCookie,
                u32::try_from(cookie.len()).unwrap_or(0),
                cookie_ptr,
            )
        };
        if status != 0 {
            return Err(BackendError::Unsupported(format!(
                "AudioToolbox refused the {} configuration ({})",
                codec_name(codec),
                sys::status(status)
            )));
        }
    }
    Ok(converter)
}

impl AudioToolboxAudio {
    /// A session for `track`.
    ///
    /// # Errors
    /// [`BackendError::Unsupported`] when `AudioToolbox` has no decoder for the track.
    pub fn open(track: &TrackInfo) -> Result<Self, BackendError> {
        let audio = track
            .audio
            .as_ref()
            .ok_or_else(|| BackendError::Unsupported("not an audio track".into()))?;
        let (input, cookie) = input_format(audio)?;
        let channels = input.mChannelsPerFrame;
        let output = AudioStreamBasicDescription {
            mSampleRate: input.mSampleRate,
            mFormatID: code(*b"lpcm"),
            mFormatFlags: FLOAT_PACKED,
            mBytesPerPacket: 4 * channels,
            mFramesPerPacket: 1,
            mBytesPerFrame: 4 * channels,
            mChannelsPerFrame: channels,
            mBitsPerChannel: 32,
            mReserved: 0,
        };
        let decodable = decode_channels(&input);
        if !decodable.is_empty() && !decodable.contains(&channels) {
            return Err(BackendError::Unsupported(format!(
                "AudioToolbox here decodes {} to {decodable:?} channels, not {channels}",
                codec_name(audio.codec)
            )));
        }
        let converter = converter(audio.codec, input, output, &cookie)?;
        let channels = channels as usize;
        let aac_seven = audio.codec == AudioCodec::Aac
            && channels == 8
            && alpha2_host_core::aac::channel_configuration(&audio.codec_private) == Some(7);
        let speakers = match audio.codec {
            // FLAC's coded order is the stream's native order, whatever layout the decoder names.
            AudioCodec::Flac => None,
            // The decoder names AAC channel configuration 7 C Lc Rc L R Ls Rs LFE; host-core
            // reads the same elements as FC, FL+FR, SL+SR, BL+BR and LFE.
            _ if aac_seven => order_output(converter.0, channels, |label, _| match label {
                7 => Some(Speaker::FrontLeft),
                8 => Some(Speaker::FrontRight),
                1 => Some(Speaker::SideLeft),
                2 => Some(Speaker::SideRight),
                5 => Some(Speaker::BackLeft),
                6 => Some(Speaker::BackRight),
                label => speaker(label, false),
            }),
            _ => order_output(converter.0, channels, |label, labels| {
                speaker(label, labels.iter().any(|label| matches!(label, 10 | 11)))
            }),
        };
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let rate = input.mSampleRate.round() as u32;
        Ok(Self {
            converter,
            formats: (input, output),
            cookie,
            codec: audio.codec,
            timescale: i64::from(track.timescale.max(1)),
            rate,
            channels,
            speakers,
            framing: Framing::of(audio.codec),
            packets: VecDeque::new(),
            decoded: Vec::new(),
            feed: Box::new(Feed {
                queued: VecDeque::new(),
                current: Vec::new(),
                description: AudioStreamPacketDescription {
                    mStartOffset: 0,
                    mVariableFramesInPacket: 0,
                    mDataByteSize: 0,
                },
                channels: u32::try_from(channels).unwrap_or(0),
                end: false,
            }),
            next_pts: None,
            out: VecDeque::new(),
            output: vec![0.0; OUTPUT_FRAMES as usize * channels.max(1)],
            drained: false,
        })
    }

    /// Runs the converter over the queued packets; returns the planes it produced.
    fn convert(&mut self) -> Result<Vec<Vec<f32>>, BackendError> {
        let mut planes = vec![Vec::new(); self.channels];
        loop {
            let mut frames = OUTPUT_FRAMES;
            let mut list = AudioBufferList {
                mNumberBuffers: 1,
                mBuffers: [AudioBuffer {
                    mNumberChannels: u32::try_from(self.channels).unwrap_or(0),
                    mDataByteSize: u32::try_from(self.output.len() * 4).unwrap_or(u32::MAX),
                    mData: self.output.as_mut_ptr().cast(),
                }],
            };
            // SAFETY: the output list points at `OUTPUT_FRAMES` interleaved frames, and the feed
            // stays borrowed for the call.
            let status = unsafe {
                AudioConverterFillComplexBuffer(
                    self.converter.0,
                    Some(feed),
                    ptr::from_mut::<Feed>(&mut self.feed).cast(),
                    NonNull::from(&mut frames),
                    NonNull::from(&mut list),
                    ptr::null_mut(),
                )
            };
            if status != 0 && status != NO_INPUT_NOW {
                return Err(BackendError::Failed(format!(
                    "AudioToolbox {} decode failed ({})",
                    codec_name(self.codec),
                    sys::status(status)
                )));
            }
            let produced = frames as usize * self.channels;
            for (index, sample) in self.output[..produced].iter().enumerate() {
                planes[index % self.channels].push(*sample);
            }
            if frames < OUTPUT_FRAMES {
                return Ok(planes);
            }
        }
    }

    /// Queues a chunk of `planes` decoded from the packet at `pts`.
    fn push(&mut self, pts: i64, planes: Vec<Vec<f32>>) {
        let frames = planes.first().map_or(0, Vec::len);
        if frames == 0 {
            return;
        }
        let length = i64::try_from(frames).unwrap_or(0) * self.timescale / i64::from(self.rate);
        self.next_pts = Some(pts + length);
        self.out.push_back(PcmChunk {
            pts,
            sample_rate: self.rate,
            planes,
            speakers: self.speakers,
        });
    }

    /// Takes `planes` from the decoder and hands out the packets they complete; at the end every
    /// packet gets what is left.
    fn deliver(&mut self, planes: Vec<Vec<f32>>, end: bool) {
        if self.decoded.is_empty() {
            self.decoded = vec![VecDeque::from(vec![0.0; self.framing.trimmed]); self.channels];
        }
        for (held, more) in self.decoded.iter_mut().zip(planes) {
            held.extend(more);
        }
        loop {
            let held = self.decoded.first().map_or(0, VecDeque::len);
            let Some(&(pts, frames)) = self.packets.front() else {
                break;
            };
            let take = match frames {
                Some(frames) if frames <= held => frames,
                Some(_) if !end => break,
                Some(frames) => frames.min(held),
                None => held,
            };
            self.packets.pop_front();
            let planes = self
                .decoded
                .iter_mut()
                .map(|plane| plane.drain(..take).collect())
                .collect();
            self.push(pts, planes);
        }
        if end && self.decoded.first().is_some_and(|plane| !plane.is_empty()) {
            let planes = self
                .decoded
                .iter_mut()
                .map(|plane| plane.drain(..).collect())
                .collect();
            if let Some(pts) = self.next_pts {
                self.push(pts, planes);
            }
        }
    }
}

impl AudioDecodeBackend for AudioToolboxAudio {
    fn submit(&mut self, packet: &Packet) -> Result<Submit, BackendError> {
        self.feed.queued.push_back(packet.data.to_vec());
        let frames = self.framing.frames.and_then(|frames| frames(&packet.data));
        self.packets.push_back((packet.pts, frames));
        let planes = self.convert()?;
        self.deliver(planes, false);
        Ok(Submit::Accepted)
    }

    fn receive(&mut self, _wait: bool) -> Result<AudioPoll, BackendError> {
        Ok(match self.out.pop_front() {
            Some(chunk) => AudioPoll::Chunk(chunk),
            None if self.drained => AudioPoll::Drained,
            None => AudioPoll::Pending,
        })
    }

    fn finish_input(&mut self) -> Result<(), BackendError> {
        self.feed.end = true;
        let planes = self.convert()?;
        self.deliver(planes, true);
        self.drained = true;
        Ok(())
    }

    /// Replaces the converter: `AudioConverterReset` keeps part of the decoder's state, which
    /// leaks the previous frames' overlap into the two frames after it.
    fn flush(&mut self) -> Result<(), BackendError> {
        let (input, output) = self.formats;
        self.converter = converter(self.codec, input, output, &self.cookie)?;
        self.feed.queued.clear();
        self.feed.end = false;
        self.packets.clear();
        self.decoded.clear();
        self.next_pts = None;
        self.out.clear();
        self.drained = false;
        Ok(())
    }
}

/// A Vorbis session through `lewton`. Planes come in Vorbis channel order.
pub struct VorbisAudio {
    ident: lewton::header::IdentHeader,
    setup: lewton::header::SetupHeader,
    window: lewton::audio::PreviousWindowRight,
    speakers: Option<SpeakerLayout>,
    out: VecDeque<PcmChunk>,
    drained: bool,
}

impl std::fmt::Debug for VorbisAudio {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("VorbisAudio")
            .finish_non_exhaustive()
    }
}

/// The three Vorbis headers from Xiph lacing.
fn vorbis_headers(private: &[u8]) -> Option<[&[u8]; 3]> {
    let (&count, mut rest) = private.split_first()?;
    if count != 2 {
        return None;
    }
    let mut sizes = [0_usize; 2];
    for size in &mut sizes {
        loop {
            let (&byte, tail) = rest.split_first()?;
            rest = tail;
            *size += usize::from(byte);
            if byte != 255 {
                break;
            }
        }
    }
    let (first, rest) = rest.split_at_checked(sizes[0])?;
    let (second, third) = rest.split_at_checked(sizes[1])?;
    Some([first, second, third])
}

impl VorbisAudio {
    /// A session for a Vorbis track.
    ///
    /// # Errors
    /// [`BackendError::Unsupported`] for unreadable headers.
    pub fn open(track: &TrackInfo) -> Result<Self, BackendError> {
        let audio = track
            .audio
            .as_ref()
            .filter(|audio| audio.codec == AudioCodec::Vorbis)
            .ok_or_else(|| BackendError::Unsupported("not a Vorbis track".into()))?;
        let unreadable = |error: &dyn std::fmt::Debug| {
            BackendError::Unsupported(format!("Vorbis headers: {error:?}"))
        };
        let [identification, _, setup] =
            vorbis_headers(&audio.codec_private).ok_or_else(|| unreadable(&"unlaced"))?;
        let ident =
            lewton::header::read_header_ident(identification).map_err(|e| unreadable(&e))?;
        let setup = lewton::header::read_header_setup(
            setup,
            ident.audio_channels,
            (ident.blocksize_0, ident.blocksize_1),
        )
        .map_err(|e| unreadable(&e))?;
        let speakers = (ident.audio_channels > 2)
            .then(|| vorbis_order(u32::from(ident.audio_channels)))
            .flatten();
        Ok(Self {
            ident,
            setup,
            window: lewton::audio::PreviousWindowRight::new(),
            speakers,
            out: VecDeque::new(),
            drained: false,
        })
    }
}

impl AudioDecodeBackend for VorbisAudio {
    fn submit(&mut self, packet: &Packet) -> Result<Submit, BackendError> {
        let planes: Vec<Vec<f32>> = lewton::audio::read_audio_packet_generic(
            &self.ident,
            &self.setup,
            &packet.data,
            &mut self.window,
        )
        .map_err(|error| BackendError::Corrupt(format!("Vorbis packet: {error:?}")))?;
        if planes.first().is_some_and(|plane| !plane.is_empty()) {
            self.out.push_back(PcmChunk {
                pts: packet.pts,
                sample_rate: self.ident.audio_sample_rate,
                planes,
                speakers: self.speakers,
            });
        }
        Ok(Submit::Accepted)
    }

    fn receive(&mut self, _wait: bool) -> Result<AudioPoll, BackendError> {
        Ok(match self.out.pop_front() {
            Some(chunk) => AudioPoll::Chunk(chunk),
            None if self.drained => AudioPoll::Drained,
            None => AudioPoll::Pending,
        })
    }

    fn finish_input(&mut self) -> Result<(), BackendError> {
        self.drained = true;
        Ok(())
    }

    fn flush(&mut self) -> Result<(), BackendError> {
        self.window = lewton::audio::PreviousWindowRight::new();
        self.out.clear();
        self.drained = false;
        Ok(())
    }
}

/// Opens `AudioToolbox` audio sessions, and `lewton` ones for Vorbis.
#[derive(Clone, Copy, Debug, Default)]
pub struct AppleAudio;

impl AudioBackends for AppleAudio {
    fn open(&self, track: &TrackInfo) -> Result<Box<dyn AudioDecodeBackend>, BackendError> {
        match track.audio.as_ref().map(|audio| audio.codec) {
            Some(AudioCodec::Vorbis) => Ok(Box::new(VorbisAudio::open(track)?)),
            _ => Ok(Box::new(AudioToolboxAudio::open(track)?)),
        }
    }
}
