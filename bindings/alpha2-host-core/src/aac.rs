//! What an AAC `AudioSpecificConfig` (ISO/IEC 14496-3 1.6.2.1) declares, for every backend.

use alpha2_media::{Speaker, SpeakerLayout};

/// `samplingFrequencyIndex` values 0 to 12 (ISO/IEC 14496-3 Table 1.18).
const SAMPLING_FREQUENCIES: [u32; 13] = [
    96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025, 8_000,
    7_350,
];

/// Reads an `AudioSpecificConfig` bit by bit, most significant bit first.
struct Bits<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Bits<'_> {
    fn read(&mut self, count: u32) -> Option<u32> {
        (0..count).try_fold(0, |value, _| {
            let byte = self.bytes.get(self.at / 8)?;
            let bit = u32::from(byte >> (7 - self.at % 8) & 1);
            self.at += 1;
            Some(value << 1 | bit)
        })
    }

    /// `GetAudioObjectType()`.
    fn object_type(&mut self) -> Option<u32> {
        match self.read(5)? {
            31 => Some(32 + self.read(6)?),
            object => Some(object),
        }
    }

    /// `samplingFrequencyIndex` and, for index 15, the explicit frequency after it.
    fn sampling_frequency(&mut self) -> Option<u32> {
        match self.read(4)? {
            15 => self.read(24),
            index => SAMPLING_FREQUENCIES
                .get(usize::try_from(index).ok()?)
                .copied(),
        }
    }
}

/// The fields of an `AudioSpecificConfig` up to its `channelConfiguration`, and after it the
/// core object type of an explicitly signaled SBR or PS stream.
struct Header<'a> {
    bits: Bits<'a>,
    /// The core coder's object type.
    object: u32,
    /// The core coder's sampling frequency, when its index names one.
    core_rate: Option<u32>,
    configuration: u8,
}

fn header(config: &[u8]) -> Option<Header<'_>> {
    let mut bits = Bits {
        bytes: config,
        at: 0,
    };
    let mut object = bits.object_type()?;
    let core_rate = bits.sampling_frequency();
    let configuration = u8::try_from(bits.read(4)?).ok()?;
    // Explicit SBR or PS signaling names the extension rate, then the core object type.
    if matches!(object, 5 | 29) {
        bits.sampling_frequency();
        object = bits.object_type()?;
    }
    Some(Header {
        bits,
        object,
        core_rate,
        configuration,
    })
}

/// The `channelConfiguration` of an `AudioSpecificConfig`.
#[must_use]
pub fn channel_configuration(config: &[u8]) -> Option<u8> {
    let mut bits = Bits {
        bytes: config,
        at: 0,
    };
    bits.object_type()?;
    if bits.read(4)? == 15 {
        bits.read(24)?;
    }
    u8::try_from(bits.read(4)?).ok()
}

/// The core frame of a stream: its samples per frame (`frameLengthFlag`: 960 or 1024) and the
/// core sampling frequency they are counted at. An SBR stream's decoded frame has twice the
/// samples at twice the rate, so the same duration.
#[must_use]
pub fn core_frame(config: &[u8]) -> Option<(u32, u32)> {
    let mut header = header(config)?;
    let rate = header.core_rate?;
    let samples = if matches!(header.object, 1..=4 | 6 | 7 | 17 | 19..=23) {
        if header.bits.read(1)? == 1 {
            960
        } else {
            1_024
        }
    } else {
        1_024
    };
    Some((samples, rate))
}

/// The speakers an `AudioSpecificConfig` declares for `channels` channels, in native (WAVE mask)
/// order: its channel configuration's, or for configuration 0 its program config element's.
///
/// ISO/IEC 14496-3 Table 1.19 assigns configuration 7's three channel pairs to the left and right
/// center front, outside front and surround speakers (a front-wide 7.1). They are read here as
/// the front, side and back pairs of configuration 12, as every alpha2 host reads them, so one
/// file names the same speakers on every host.
#[must_use]
pub fn layout(config: &[u8], channels: usize) -> Option<SpeakerLayout> {
    use Speaker::{
        BackCenter, BackLeft, BackRight, FrontCenter, FrontLeft, FrontRight, LowFrequency,
        SideLeft, SideRight,
    };
    let mut header = header(config)?;
    let speakers: Vec<Speaker> = match header.configuration {
        0 => {
            if !matches!(header.object, 1..=4 | 6 | 7 | 17 | 19..=23) {
                return None;
            }
            // GASpecificConfig: frameLengthFlag, dependsOnCoreCoder (and its delay),
            // extensionFlag; then the program config element.
            let bits = &mut header.bits;
            bits.read(1)?;
            if bits.read(1)? == 1 {
                bits.read(14)?;
            }
            bits.read(1)?;
            program_config_layout(bits)?
        }
        1 => vec![FrontCenter],
        2 => vec![FrontLeft, FrontRight],
        3 => vec![FrontLeft, FrontRight, FrontCenter],
        4 => vec![FrontLeft, FrontRight, FrontCenter, BackCenter],
        5 => vec![FrontLeft, FrontRight, FrontCenter, BackLeft, BackRight],
        6 => vec![
            FrontLeft,
            FrontRight,
            FrontCenter,
            LowFrequency,
            BackLeft,
            BackRight,
        ],
        7 | 12 => vec![
            FrontLeft,
            FrontRight,
            FrontCenter,
            LowFrequency,
            BackLeft,
            BackRight,
            SideLeft,
            SideRight,
        ],
        11 => vec![
            FrontLeft,
            FrontRight,
            FrontCenter,
            LowFrequency,
            BackLeft,
            BackRight,
            BackCenter,
        ],
        _ => return None,
    };
    (speakers.len() == channels)
        .then(|| SpeakerLayout::new(&speakers))
        .flatten()
}

/// The speakers of an AAC `program_config_element()` (ISO/IEC 14496-3 4.4.1.1) in native order:
/// front channels center first then left-right pairs, side pairs, back pairs then a back center,
/// and the LFE.
fn program_config_layout(bits: &mut Bits<'_>) -> Option<Vec<Speaker>> {
    use Speaker::{
        BackCenter, BackLeft, BackRight, FrontCenter, FrontLeft, FrontLeftOfCenter, FrontRight,
        FrontRightOfCenter, LowFrequency, SideLeft, SideRight,
    };
    bits.read(4 + 2 + 4)?; // element_instance_tag, object_type, sampling_frequency_index
    let (front, side, back, lfe) = (bits.read(4)?, bits.read(4)?, bits.read(4)?, bits.read(2)?);
    bits.read(3 + 4)?; // num_assoc_data_elements, num_valid_cc_elements
    for _ in 0..2 {
        if bits.read(1)? == 1 {
            bits.read(4)?; // mono and stereo mixdown element numbers
        }
    }
    if bits.read(1)? == 1 {
        bits.read(3)?; // matrix_mixdown_idx, pseudo_surround_enable
    }
    let mut channels = |elements: u32| -> Option<u32> {
        (0..elements).try_fold(0, |total, _| {
            let pair = bits.read(1)?;
            bits.read(4)?; // element tag
            Some(total + 1 + pair)
        })
    };
    let (front, side, back) = (channels(front)?, channels(side)?, channels(back)?);
    let mut speakers = Vec::new();
    if front % 2 == 1 {
        speakers.push(FrontCenter);
    }
    let front_pairs = [
        (FrontLeft, FrontRight),
        (FrontLeftOfCenter, FrontRightOfCenter),
    ];
    for pair in front_pairs.iter().take(front as usize / 2) {
        speakers.extend([pair.0, pair.1]);
    }
    if side / 2 > 0 {
        speakers.extend([SideLeft, SideRight]);
    }
    if back / 2 > 0 {
        speakers.extend([BackLeft, BackRight]);
    }
    if back % 2 == 1 {
        speakers.push(BackCenter);
    }
    if lfe > 0 {
        speakers.push(LowFrequency);
    }
    speakers.sort_by_key(|speaker| speaker.ffmpeg_channel());
    Some(speakers)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aac_layouts_come_from_the_channel_configuration_or_the_program_config_element() {
        let names = |config: &[u8], channels| {
            layout(config, channels).map(|layout| {
                layout
                    .speakers()
                    .iter()
                    .map(|speaker| speaker.name())
                    .collect::<Vec<_>>()
            })
        };
        // The corpus's AAC 3.0, 5.1 and 7.1 configurations, and its quad's program config element.
        assert_eq!(
            names(&[0x11, 0x98, 0x56, 0xe5, 0x00], 3).expect("3.0"),
            ["FL", "FR", "FC"]
        );
        assert_eq!(
            names(&[0x11, 0xb0, 0x56, 0xe5, 0x00], 6).expect("5.1"),
            ["FL", "FR", "FC", "LFE", "BL", "BR"]
        );
        assert_eq!(
            names(&[0x11, 0xb8, 0x56, 0xe5, 0x00], 8).expect("7.1"),
            ["FL", "FR", "FC", "LFE", "BL", "BR", "SL", "SR"]
        );
        let quad = [
            0x11, 0x80, 0x04, 0xc4, 0x04, 0x00, 0x21, 0x10, 0x04, 0x4c, 0x61, 0x76, 0x63, 0x56,
            0xe5, 0x00,
        ];
        assert_eq!(names(&quad, 4).expect("quad"), ["FL", "FR", "BL", "BR"]);
        // A count other than the configuration's is left to the default.
        assert_eq!(names(&[0x11, 0x98], 2), None);
    }

    #[test]
    fn the_core_frame_counts_at_the_core_rate() {
        assert_eq!(core_frame(&[0x11, 0x90]), Some((1_024, 48_000)));
        // frameLengthFlag set: 960-sample frames.
        assert_eq!(core_frame(&[0x11, 0x94]), Some((960, 48_000)));
        // Explicit SBR: core AAC-LC at 24 kHz under a 48 kHz extension.
        assert_eq!(core_frame(&[0x2b, 0x11, 0x88]), Some((1_024, 24_000)));
        assert_eq!(channel_configuration(&[0x11, 0xb8]), Some(7));
        assert_eq!(channel_configuration(&[0x2b, 0x11, 0x88]), Some(2));
    }
}
