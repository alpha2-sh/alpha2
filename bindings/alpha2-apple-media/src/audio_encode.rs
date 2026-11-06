//! Audio encoding through `AudioToolbox` converters: AAC, and Opus up to two channels.

// Every call into the media frameworks is FFI.
#![allow(unsafe_code)]

use std::collections::VecDeque;
use std::ffi::c_void;
use std::ptr::{self, NonNull};

use alpha2_host_core::audio_frames::encoder_sample_rate;
use alpha2_host_core::encode::{
    AudioEncodeBackend, AudioEncodeConfig, AudioEncoders, EncodePoll, EncodedPacket,
};
use alpha2_host_core::video_stream::Submit;
use alpha2_media::{
    AudioCodec, EncodeError, EncoderRefusal, OpenFailureKind, Speaker, SpeakerLayout,
};
use bytes::Bytes;
use objc2_audio_toolbox::{
    AudioConverterFillComplexBuffer, AudioConverterGetProperty, AudioConverterGetPropertyInfo,
    AudioConverterNew, AudioConverterRef, AudioConverterSetProperty, AudioFormatGetProperty,
    kAudioConverterCompressionMagicCookie, kAudioConverterEncodeBitRate,
    kAudioConverterInputChannelLayout, kAudioConverterPrimeInfo,
    kAudioConverterPropertyMaximumOutputPacketSize,
    kAudioFormatProperty_AvailableEncodeSampleRates, kAudioFormatProperty_EncodeFormatIDs,
    kAudioFormatProperty_FormatInfo,
};
use objc2_core_audio_types::{
    AudioBuffer, AudioBufferList, AudioStreamBasicDescription, AudioStreamPacketDescription,
};

use crate::audio::{Converter, NO_INPUT_NOW, format_property};
use crate::config::es_specific_info;
use crate::sys::{self, code};

/// `kMPEG4Object_AAC_LC`.
const AAC_LC: u32 = 2;

/// Interleaved samples waiting for the converter.
struct Feed {
    pending: VecDeque<f32>,
    /// The samples handed to the converter last, which must stay valid until it asks again.
    current: Vec<f32>,
    channels: usize,
    end: bool,
}

unsafe extern "C-unwind" fn feed(
    _converter: AudioConverterRef,
    packets: NonNull<u32>,
    buffers: NonNull<AudioBufferList>,
    _descriptions: *mut *mut AudioStreamPacketDescription,
    user: *mut c_void,
) -> i32 {
    // SAFETY: the user data is the session's `Feed`, borrowed for the converter call.
    let feed = unsafe { &mut *user.cast::<Feed>() };
    // SAFETY: the converter passes a valid packet count.
    let asked = unsafe { packets.read() } as usize;
    let frames = asked.min(feed.pending.len() / feed.channels.max(1));
    if frames == 0 {
        // SAFETY: as above.
        unsafe { packets.write(0) };
        return if feed.end { 0 } else { NO_INPUT_NOW };
    }
    feed.current = feed.pending.drain(..frames * feed.channels).collect();
    // SAFETY: the converter passes a list with one buffer; linear PCM packets are frames.
    unsafe {
        packets.write(u32::try_from(frames).unwrap_or(0));
        let list = buffers.as_ptr();
        (*list).mNumberBuffers = 1;
        (*list).mBuffers[0] = AudioBuffer {
            mNumberChannels: u32::try_from(feed.channels).unwrap_or(0),
            mDataByteSize: u32::try_from(feed.current.len() * 4).unwrap_or(u32::MAX),
            mData: feed.current.as_mut_ptr().cast(),
        };
    }
    0
}

/// The `AudioChannelLabel`s of `speakers`, the surround pair as the plain surround pair unless
/// both a side and a back pair are present.
fn labels(speakers: &[Speaker]) -> Option<Vec<u32>> {
    let both = speakers.contains(&Speaker::SideLeft) && speakers.contains(&Speaker::BackLeft);
    speakers
        .iter()
        .map(|speaker| {
            Some(match speaker {
                Speaker::FrontLeft => 1,
                Speaker::FrontRight => 2,
                Speaker::FrontCenter => 3,
                Speaker::LowFrequency => 4,
                Speaker::BackLeft if both => 33,
                Speaker::BackRight if both => 34,
                Speaker::SideLeft | Speaker::BackLeft => 5,
                Speaker::SideRight | Speaker::BackRight => 6,
                Speaker::FrontLeftOfCenter => 7,
                Speaker::FrontRightOfCenter => 8,
                Speaker::BackCenter => 9,
                Speaker::TopCenter => 12,
                Speaker::TopFrontLeft => 13,
                Speaker::TopFrontCenter => 14,
                Speaker::TopFrontRight => 15,
                Speaker::TopBackLeft => 16,
                Speaker::TopBackCenter => 17,
                Speaker::TopBackRight => 18,
                Speaker::WideLeft => 35,
                Speaker::WideRight => 36,
                _ => return None,
            })
        })
        .collect()
}

/// The `AudioToolbox` format ID of `codec`.
const fn format_id(codec: AudioCodec) -> u32 {
    match codec {
        AudioCodec::Aac => code(*b"aac "),
        AudioCodec::Opus => code(*b"opus"),
    }
}

/// The rate the system's encoder for `id` runs at for `project_rate`, picked from its
/// `kAudioFormatProperty_AvailableEncodeSampleRates` as [`encoder_sample_rate`] picks; `None` when
/// it lists none.
fn encode_rate(id: u32, project_rate: u32) -> Option<u32> {
    let ranges = format_property(
        kAudioFormatProperty_AvailableEncodeSampleRates,
        &id.to_ne_bytes(),
    )?;
    // `AudioValueRange`s: a minimum and a maximum `f64` each.
    let ranges = ranges
        .as_chunks::<16>()
        .0
        .iter()
        .map(|range| {
            let bound = |at: usize| {
                let mut bytes = [0; 8];
                bytes.copy_from_slice(&range[at..at + 8]);
                f64::from_ne_bytes(bytes)
            };
            (bound(0), bound(8))
        })
        .collect::<Vec<_>>();
    if ranges.is_empty() {
        return None;
    }
    let project = f64::from(project_rate);
    if ranges
        .iter()
        .any(|(minimum, maximum)| (*minimum..=*maximum).contains(&project))
    {
        return Some(project_rate);
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let rates = ranges
        .iter()
        .flat_map(|(minimum, maximum)| [*minimum, *maximum])
        .map(|rate| rate.round() as u32)
        .collect::<Vec<_>>();
    Some(encoder_sample_rate(Some(&rates), project_rate))
}

/// An `OpusHead` (RFC 7845 5.1) for channel mapping family 0.
fn opus_head(channels: u8, pre_skip: u16, rate: u32) -> Bytes {
    let mut head = b"OpusHead".to_vec();
    head.push(1);
    head.push(channels);
    head.extend_from_slice(&pre_skip.to_le_bytes());
    head.extend_from_slice(&rate.to_le_bytes());
    head.extend_from_slice(&0_i16.to_le_bytes());
    head.push(0);
    Bytes::from(head)
}

/// An `AudioToolbox` audio encoder session.
pub struct AudioToolboxEncoder {
    converter: Converter,
    name: &'static str,
    rate: u32,
    channels: usize,
    frame_size: usize,
    priming: u32,
    codec_private: Bytes,
    feed: Box<Feed>,
    out: VecDeque<EncodedPacket>,
    packet: Vec<u8>,
    next_pts: i64,
    drained: bool,
}

impl std::fmt::Debug for AudioToolboxEncoder {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AudioToolboxEncoder")
            .field("encoder", &self.name)
            .finish_non_exhaustive()
    }
}

fn refused(encoder: &str, kind: OpenFailureKind, reason: String) -> EncodeError {
    EncodeError::EncodersRefused {
        message: format!("{encoder}: {reason}"),
        refusals: vec![EncoderRefusal {
            encoder: encoder.into(),
            hardware_accelerated: false,
            kind,
            reason,
        }],
    }
}

/// An encoder from interleaved float samples at `rate` to the format `id` with `flags`.
fn new_encoder(
    name: &str,
    id: u32,
    flags: u32,
    rate: u32,
    channels: usize,
    frame_size: usize,
) -> Result<Converter, EncodeError> {
    let channel_count = u32::try_from(channels).unwrap_or(0);
    let mut input = AudioStreamBasicDescription {
        mSampleRate: f64::from(rate),
        mFormatID: code(*b"lpcm"),
        mFormatFlags: 1 | 8,
        mBytesPerPacket: 4 * channel_count,
        mFramesPerPacket: 1,
        mBytesPerFrame: 4 * channel_count,
        mChannelsPerFrame: channel_count,
        mBitsPerChannel: 32,
        mReserved: 0,
    };
    let mut output = AudioStreamBasicDescription {
        mSampleRate: f64::from(rate),
        mFormatID: id,
        mFormatFlags: flags,
        mBytesPerPacket: 0,
        mFramesPerPacket: u32::try_from(frame_size).unwrap_or(0),
        mBytesPerFrame: 0,
        mChannelsPerFrame: channel_count,
        mBitsPerChannel: 0,
        mReserved: 0,
    };
    let mut size = u32::try_from(size_of::<AudioStreamBasicDescription>()).unwrap_or(0);
    // `FormatInfo` completes the description; a failure keeps what is set.
    // SAFETY: no specifier; `output` is a full description.
    let _ = unsafe {
        AudioFormatGetProperty(
            kAudioFormatProperty_FormatInfo,
            0,
            ptr::null(),
            &raw mut size,
            (&raw mut output).cast(),
        )
    };
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
        return Err(refused(
            name,
            OpenFailureKind::Absent,
            format!(
                "no encoder for {channels} channels at {rate} Hz ({})",
                sys::status(status)
            ),
        ));
    }
    Ok(Converter(converter))
}

/// Sets converter property `id` to `bytes`.
fn set_property(converter: &Converter, id: u32, bytes: &mut [u8]) -> i32 {
    let Some(data) = NonNull::new(bytes.as_mut_ptr().cast::<c_void>()) else {
        return -1;
    };
    // SAFETY: `bytes` holds the property's value.
    unsafe {
        AudioConverterSetProperty(
            converter.0,
            id,
            u32::try_from(bytes.len()).unwrap_or(0),
            data,
        )
    }
}

/// Tells the encoder which speaker each input plane feeds.
fn set_input_layout(
    converter: &Converter,
    name: &str,
    speakers: SpeakerLayout,
) -> Result<(), EncodeError> {
    let labels = labels(speakers.speakers()).ok_or_else(|| {
        refused(
            name,
            OpenFailureKind::Unsuitable,
            format!("no channel labels for {}", speakers.describe()),
        )
    })?;
    // An `AudioChannelLayout` with descriptions: tag, bitmap, count, then label, flags and three
    // coordinates per channel.
    let mut layout = Vec::with_capacity(12 + 20 * labels.len());
    layout.extend_from_slice(&0_u32.to_ne_bytes());
    layout.extend_from_slice(&0_u32.to_ne_bytes());
    layout.extend_from_slice(&u32::try_from(labels.len()).unwrap_or(0).to_ne_bytes());
    for label in labels {
        layout.extend_from_slice(&label.to_ne_bytes());
        layout.extend_from_slice(&[0; 16]);
    }
    let status = set_property(converter, kAudioConverterInputChannelLayout, &mut layout);
    if status != 0 {
        return Err(refused(
            name,
            OpenFailureKind::Unsuitable,
            format!(
                "the layout {} was refused ({})",
                speakers.describe(),
                sys::status(status)
            ),
        ));
    }
    Ok(())
}

/// A converter property of `T`.
fn property<T: Default>(converter: AudioConverterRef, id: u32) -> Option<T> {
    let mut value = T::default();
    let mut size = u32::try_from(size_of::<T>()).ok()?;
    // SAFETY: `value` holds `size` bytes of the property's type.
    let status = unsafe {
        AudioConverterGetProperty(
            converter,
            id,
            NonNull::from(&mut size),
            NonNull::from(&mut value).cast(),
        )
    };
    (status == 0).then_some(value)
}

impl AudioToolboxEncoder {
    /// A session for `config`.
    ///
    /// # Errors
    /// [`EncodeError::EncodersRefused`] when the system's encoder does not take the
    /// configuration.
    pub fn open(config: &AudioEncodeConfig) -> Result<Self, EncodeError> {
        let speakers = config.speakers.speakers();
        let channels = speakers.len();
        let id = format_id(config.codec);
        let (name, rate, frame_size, flags) = match config.codec {
            AudioCodec::Aac => {
                let name = "audiotoolbox aac";
                let rate = encode_rate(id, config.project_rate).ok_or_else(|| {
                    refused(
                        name,
                        OpenFailureKind::Absent,
                        "the system lists no AAC encode rates".into(),
                    )
                })?;
                (name, rate, 1_024, AAC_LC)
            }
            AudioCodec::Opus => ("audiotoolbox opus", 48_000, 960, 0),
        };
        if config.codec == AudioCodec::Opus && channels > 2 {
            return Err(refused(
                name,
                OpenFailureKind::Unsuitable,
                format!(
                    "Opus is written with mapping family 0, up to two channels, not {channels}"
                ),
            ));
        }
        let converter = new_encoder(name, id, flags, rate, channels, frame_size)?;
        if channels > 2 {
            set_input_layout(&converter, name, config.speakers)?;
        }
        let status = set_property(
            &converter,
            kAudioConverterEncodeBitRate,
            &mut config.bitrate.to_ne_bytes(),
        );
        if status != 0 {
            return Err(refused(
                name,
                OpenFailureKind::Unsuitable,
                format!(
                    "{} bit/s was refused ({})",
                    config.bitrate,
                    sys::status(status)
                ),
            ));
        }
        // The encoder delay, which the round trip test pins: 2112 samples for AAC, 312 for Opus.
        let [priming, _] =
            property::<[u32; 2]>(converter.0, kAudioConverterPrimeInfo).ok_or_else(|| {
                refused(
                    name,
                    OpenFailureKind::Unknown,
                    "no priming information".into(),
                )
            })?;
        let codec_private = match config.codec {
            AudioCodec::Aac => {
                let cookie = cookie(converter.0).ok_or_else(|| {
                    refused(name, OpenFailureKind::Unknown, "no magic cookie".into())
                })?;
                Bytes::copy_from_slice(es_specific_info(&cookie).ok_or_else(|| {
                    refused(
                        name,
                        OpenFailureKind::Unknown,
                        "an unreadable magic cookie".into(),
                    )
                })?)
            }
            AudioCodec::Opus => opus_head(
                u8::try_from(channels).unwrap_or(2),
                u16::try_from(priming).unwrap_or(0),
                rate,
            ),
        };
        let packet_size =
            property::<u32>(converter.0, kAudioConverterPropertyMaximumOutputPacketSize)
                .filter(|size| *size > 0)
                .unwrap_or(1 << 16);
        Ok(Self {
            converter,
            name,
            rate,
            channels,
            frame_size,
            priming,
            codec_private,
            feed: Box::new(Feed {
                pending: VecDeque::new(),
                current: Vec::new(),
                channels,
                end: false,
            }),
            out: VecDeque::new(),
            packet: vec![0; packet_size as usize],
            next_pts: 0,
            drained: false,
        })
    }

    /// Takes every packet the converter can make from the samples queued.
    fn pull(&mut self) -> Result<(), EncodeError> {
        loop {
            let mut packets = 1_u32;
            let mut description = AudioStreamPacketDescription {
                mStartOffset: 0,
                mVariableFramesInPacket: 0,
                mDataByteSize: 0,
            };
            let mut list = AudioBufferList {
                mNumberBuffers: 1,
                mBuffers: [AudioBuffer {
                    mNumberChannels: u32::try_from(self.channels).unwrap_or(0),
                    mDataByteSize: u32::try_from(self.packet.len()).unwrap_or(u32::MAX),
                    mData: self.packet.as_mut_ptr().cast(),
                }],
            };
            // SAFETY: the output list holds one packet of the encoder's maximum size, and the
            // feed stays borrowed for the call.
            let status = unsafe {
                AudioConverterFillComplexBuffer(
                    self.converter.0,
                    Some(feed),
                    ptr::from_mut::<Feed>(&mut self.feed).cast(),
                    NonNull::from(&mut packets),
                    NonNull::from(&mut list),
                    &raw mut description,
                )
            };
            if status != 0 && status != NO_INPUT_NOW {
                return Err(EncodeError::Adapter(format!(
                    "{}: encoding failed ({})",
                    self.name,
                    sys::status(status)
                )));
            }
            if packets == 0 {
                return Ok(());
            }
            let start = usize::try_from(description.mStartOffset).unwrap_or(0);
            let length = description.mDataByteSize as usize;
            let data = self.packet.get(start..start + length).ok_or_else(|| {
                EncodeError::Adapter(format!("{}: a packet past its buffer", self.name))
            })?;
            self.out.push_back(EncodedPacket {
                pts: self.next_pts,
                dts: self.next_pts,
                keyframe: true,
                data: Bytes::copy_from_slice(data),
            });
            self.next_pts += i64::try_from(self.frame_size).unwrap_or(0);
        }
    }
}

/// The converter's magic cookie.
fn cookie(converter: AudioConverterRef) -> Option<Vec<u8>> {
    let mut size = 0_u32;
    // SAFETY: valid out pointers; the converter is live.
    let status = unsafe {
        AudioConverterGetPropertyInfo(
            converter,
            kAudioConverterCompressionMagicCookie,
            &raw mut size,
            ptr::null_mut(),
        )
    };
    if status != 0 || size == 0 {
        return None;
    }
    let mut bytes = vec![0_u8; size as usize];
    // SAFETY: `bytes` holds `size` bytes.
    let status = unsafe {
        AudioConverterGetProperty(
            converter,
            kAudioConverterCompressionMagicCookie,
            NonNull::from(&mut size),
            NonNull::new(bytes.as_mut_ptr().cast())?,
        )
    };
    (status == 0).then(|| {
        bytes.truncate(size as usize);
        bytes
    })
}

impl AudioEncodeBackend for AudioToolboxEncoder {
    fn name(&self) -> &str {
        self.name
    }

    fn sample_rate(&self) -> u32 {
        self.rate
    }

    fn frame_size(&self) -> usize {
        self.frame_size
    }

    fn submit(&mut self, _pts: i64, planes: &[Vec<f32>]) -> Result<Submit, EncodeError> {
        if planes.len() != self.channels {
            return Err(EncodeError::Adapter(format!(
                "{}: {} planes for {} channels",
                self.name,
                planes.len(),
                self.channels
            )));
        }
        let frames = planes.first().map_or(0, Vec::len);
        for index in 0..frames {
            self.feed
                .pending
                .extend(planes.iter().map(|plane| plane[index]));
        }
        self.pull()?;
        Ok(Submit::Accepted)
    }

    fn receive(&mut self, _wait: bool) -> Result<EncodePoll, EncodeError> {
        Ok(match self.out.pop_front() {
            Some(packet) => EncodePoll::Packet(packet),
            None if self.drained => EncodePoll::Drained,
            None => EncodePoll::Pending,
        })
    }

    fn finish_input(&mut self) -> Result<(), EncodeError> {
        self.feed.end = true;
        self.pull()?;
        self.drained = true;
        Ok(())
    }

    fn codec_private(&self) -> Option<Bytes> {
        Some(self.codec_private.clone())
    }

    fn priming(&self) -> u32 {
        self.priming
    }
}

/// Opens `AudioToolbox` encoder sessions.
#[derive(Clone, Copy, Debug, Default)]
pub struct AppleAudioEncoders;

impl AppleAudioEncoders {
    /// The codecs this system encodes through this backend: those of AAC and Opus whose format
    /// `kAudioFormatProperty_EncodeFormatIDs` lists.
    #[must_use]
    pub fn available_codecs(&self) -> Vec<AudioCodec> {
        let listed = format_property(kAudioFormatProperty_EncodeFormatIDs, &[]).unwrap_or_default();
        let listed = listed
            .as_chunks::<4>()
            .0
            .iter()
            .map(|id| u32::from_ne_bytes(*id))
            .collect::<Vec<_>>();
        [AudioCodec::Aac, AudioCodec::Opus]
            .into_iter()
            .filter(|codec| listed.contains(&format_id(*codec)))
            .collect()
    }
}

impl AudioEncoders for AppleAudioEncoders {
    fn open(&self, config: &AudioEncodeConfig) -> Result<Box<dyn AudioEncodeBackend>, EncodeError> {
        Ok(Box::new(AudioToolboxEncoder::open(config)?))
    }
}
