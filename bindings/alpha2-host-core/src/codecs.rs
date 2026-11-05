//! The codecs native hosts encode.

/// Video codecs the native encoders implement.
#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
pub enum NativeVideoCodec {
    /// H.264/AVC, preferred for broad browser, Windows, and Apple playback.
    H264,
    /// H.265/HEVC.
    Hevc,
    /// VP9, where the host has a VP9 encoder.
    Vp9,
    /// AV1, where the host has an AV1 encoder.
    Av1,
    /// MPEG-4 Part 2, retained for compatibility with existing alpha2 hosts.
    Mpeg4Part2,
    /// Apple `ProRes`: 422 HQ, or 4444 with alpha.
    ProRes,
}

impl NativeVideoCodec {
    /// Stable identifier suitable for an application protocol or settings file.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::H264 => "h264",
            Self::Hevc => "hevc",
            Self::Vp9 => "vp9",
            Self::Av1 => "av1",
            Self::Mpeg4Part2 => "mpeg4-part2",
            Self::ProRes => "prores",
        }
    }

    /// The pictures an encoder must take to be offered: 10-bit 4:2:2 for
    /// `ProRes`, 8-bit 4:2:0 otherwise.
    #[must_use]
    pub const fn base_layout(self) -> alpha2_media::PixelLayout {
        match self {
            Self::ProRes => alpha2_media::PixelLayout {
                chroma: alpha2_media::Chroma::Yuv422,
                bit_depth: alpha2_media::BitDepth::Ten,
                alpha: false,
                planes: alpha2_media::PlaneLayout::Planar,
            },
            _ => alpha2_media::PixelLayout::YUV420P,
        }
    }

    /// The engine's codec this encoder family implements.
    #[must_use]
    pub const fn model(self) -> alpha2_media::VideoCodec {
        match self {
            Self::H264 => alpha2_media::VideoCodec::H264,
            Self::Hevc => alpha2_media::VideoCodec::Hevc,
            Self::Vp9 => alpha2_media::VideoCodec::Vp9,
            Self::Av1 => alpha2_media::VideoCodec::Av1,
            Self::Mpeg4Part2 => alpha2_media::VideoCodec::Mpeg4Part2,
            Self::ProRes => alpha2_media::VideoCodec::ProRes,
        }
    }

    /// The native codec for the engine's `codec`.
    #[must_use]
    pub const fn from_model(codec: alpha2_media::VideoCodec) -> Self {
        match codec {
            alpha2_media::VideoCodec::H264 => Self::H264,
            alpha2_media::VideoCodec::Hevc => Self::Hevc,
            alpha2_media::VideoCodec::Vp9 => Self::Vp9,
            alpha2_media::VideoCodec::Av1 => Self::Av1,
            alpha2_media::VideoCodec::Mpeg4Part2 => Self::Mpeg4Part2,
            alpha2_media::VideoCodec::ProRes => Self::ProRes,
        }
    }
}

/// Audio codecs the native encoders implement.
#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
pub enum NativeAudioCodec {
    /// AAC-LC compatible audio.
    Aac,
    /// Opus.
    Opus,
}

impl NativeAudioCodec {
    /// Every codec, in the order the engine lists them.
    pub const ALL: [Self; 2] = [Self::Aac, Self::Opus];

    /// Stable identifier suitable for an application protocol or settings file.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Aac => "aac",
            Self::Opus => "opus",
        }
    }

    /// The engine's codec this encoder family implements.
    #[must_use]
    pub const fn model(self) -> alpha2_media::AudioCodec {
        match self {
            Self::Aac => alpha2_media::AudioCodec::Aac,
            Self::Opus => alpha2_media::AudioCodec::Opus,
        }
    }

    /// The native codec for the engine's `codec`.
    #[must_use]
    pub const fn from_model(codec: alpha2_media::AudioCodec) -> Self {
        match codec {
            alpha2_media::AudioCodec::Aac => Self::Aac,
            alpha2_media::AudioCodec::Opus => Self::Opus,
        }
    }
}
