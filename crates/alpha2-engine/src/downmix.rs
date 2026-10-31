//! How a source's channels become the project's.
//!
//! Providers hand over the source's own channels and their speakers ([`SpeakerLayout`]); the
//! engine folds them so the downmix is bit-identical on every platform.
//!
//! Coefficients follow ITU-R BS.775: center and surrounds reach the front pair at -3 dB, a back
//! center splits at a further -3 dB, and LFE is dropped. Speakers BS.775 does not place follow
//! `FFmpeg`: front-plane extras (left of center, wide, Lt/Rt, binaural) at unity, side
//! surround/height/bottom at -3 dB, centered height/bottom as a back center, unnamed dropped.
//! Nothing is normalized; the master limiter handles peaks. A mono project hears the average of
//! the stereo fold, and a mono source feeds every project channel at unity.
//!
//! [`SpeakerLayout`]: alpha2_media::SpeakerLayout

use alpha2_audio::ChannelLayout;
use alpha2_media::{MAX_AUDIO_CHANNELS, Speaker};

/// -3 dB.
const HALF_POWER: f32 = std::f32::consts::FRAC_1_SQRT_2;

/// One project channel as a weighted sum of source planes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ChannelMix {
    terms: [(usize, f32); MAX_AUDIO_CHANNELS],
    count: usize,
}

impl ChannelMix {
    const EMPTY: Self = Self {
        terms: [(0, 0.0); MAX_AUDIO_CHANNELS],
        count: 0,
    };

    pub(crate) fn terms(&self) -> &[(usize, f32)] {
        &self.terms[..self.count]
    }

    /// The one plane this channel copies unchanged, if its weight is exactly 1.0.
    #[allow(clippy::float_cmp)]
    pub(crate) fn unity(&self) -> Option<usize> {
        match self.terms() {
            [(plane, weight)] if *weight == 1.0 => Some(*plane),
            _ => None,
        }
    }

    fn add(&mut self, plane: usize, weight: f32) {
        self.terms[self.count] = (plane, weight);
        self.count += 1;
    }
}

/// Every project channel's mix of one source's planes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Downmix {
    channels: [ChannelMix; 2],
}

impl Downmix {
    /// The fold of `speakers` into `layout`, or `None` for zero or more than
    /// [`MAX_AUDIO_CHANNELS`] planes.
    pub(crate) fn new(speakers: &[Speaker], layout: ChannelLayout) -> Option<Self> {
        if speakers.is_empty() || speakers.len() > MAX_AUDIO_CHANNELS {
            return None;
        }
        let mut channels = [ChannelMix::EMPTY; 2];
        if let [Speaker::FrontCenter] = speakers {
            for mix in &mut channels[..layout.channels()] {
                mix.add(0, 1.0);
            }
            return Some(Self { channels });
        }
        for (plane, speaker) in speakers.iter().enumerate() {
            let (left, right) = stereo_weights(*speaker);
            match layout {
                ChannelLayout::Stereo => {
                    if left != 0.0 {
                        channels[0].add(plane, left);
                    }
                    if right != 0.0 {
                        channels[1].add(plane, right);
                    }
                }
                ChannelLayout::Mono => {
                    let weight = f32::midpoint(left, right);
                    if weight != 0.0 {
                        channels[0].add(plane, weight);
                    }
                }
            }
        }
        Some(Self { channels })
    }

    pub(crate) const fn channel(&self, channel: usize) -> &ChannelMix {
        &self.channels[channel]
    }
}

/// A speaker's weights in the stereo fold.
const fn stereo_weights(speaker: Speaker) -> (f32, f32) {
    use Speaker::{
        BackCenter, BackLeft, BackRight, BinauralLeft, BinauralRight, BottomFrontCenter,
        BottomFrontLeft, BottomFrontRight, DownmixLeft, DownmixRight, FrontCenter, FrontLeft,
        FrontLeftOfCenter, FrontRight, FrontRightOfCenter, LowFrequency, LowFrequency2, SideLeft,
        SideRight, SideSurroundLeft, SideSurroundRight, SurroundDirectLeft, SurroundDirectRight,
        TopBackCenter, TopBackLeft, TopBackRight, TopCenter, TopFrontCenter, TopFrontLeft,
        TopFrontRight, TopSideLeft, TopSideRight, TopSurroundLeft, TopSurroundRight, Unnamed,
        WideLeft, WideRight,
    };
    match speaker {
        FrontLeft | FrontLeftOfCenter | WideLeft | DownmixLeft | BinauralLeft => (1.0, 0.0),
        FrontRight | FrontRightOfCenter | WideRight | DownmixRight | BinauralRight => (0.0, 1.0),
        FrontCenter => (HALF_POWER, HALF_POWER),
        LowFrequency | LowFrequency2 | Unnamed => (0.0, 0.0),
        BackLeft | SideLeft | SurroundDirectLeft | SideSurroundLeft | TopFrontLeft
        | TopBackLeft | TopSideLeft | TopSurroundLeft | BottomFrontLeft => (HALF_POWER, 0.0),
        BackRight | SideRight | SurroundDirectRight | SideSurroundRight | TopFrontRight
        | TopBackRight | TopSideRight | TopSurroundRight | BottomFrontRight => (0.0, HALF_POWER),
        BackCenter | TopCenter | TopFrontCenter | TopBackCenter | BottomFrontCenter => (0.5, 0.5),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn weights(mix: &ChannelMix) -> Vec<(usize, f32)> {
        mix.terms().to_vec()
    }

    fn default_fold(channels: usize, layout: ChannelLayout) -> Option<Downmix> {
        let speakers = alpha2_media::SpeakerLayout::default_for(channels)?;
        Downmix::new(speakers.speakers(), layout)
    }

    #[test]
    fn mono_and_stereo_sources_copy_planes_unchanged_into_stereo() {
        let mono = default_fold(1, ChannelLayout::Stereo).expect("mono");
        assert_eq!(mono.channel(0).unity(), Some(0));
        assert_eq!(mono.channel(1).unity(), Some(0));
        let stereo = default_fold(2, ChannelLayout::Stereo).expect("stereo");
        assert_eq!(stereo.channel(0).unity(), Some(0));
        assert_eq!(stereo.channel(1).unity(), Some(1));
    }

    #[test]
    fn surround_folds_center_and_surrounds_at_minus_three_decibels_without_lfe() {
        let fold = default_fold(6, ChannelLayout::Stereo).expect("5.1");
        assert_eq!(
            weights(fold.channel(0)),
            [(0, 1.0), (2, HALF_POWER), (4, HALF_POWER)]
        );
        assert_eq!(
            weights(fold.channel(1)),
            [(1, 1.0), (2, HALF_POWER), (5, HALF_POWER)]
        );
        let seven = default_fold(8, ChannelLayout::Stereo).expect("7.1");
        assert_eq!(
            weights(seven.channel(0)),
            [(0, 1.0), (2, HALF_POWER), (4, HALF_POWER), (6, HALF_POWER)]
        );
        let six = default_fold(7, ChannelLayout::Stereo).expect("6.1");
        assert!(weights(six.channel(1)).contains(&(4, 0.5)));
    }

    #[test]
    fn a_mono_project_hears_the_average_of_the_stereo_fold() {
        let stereo = default_fold(2, ChannelLayout::Mono).expect("stereo");
        assert_eq!(weights(stereo.channel(0)), [(0, 0.5), (1, 0.5)]);
        assert_eq!(stereo.channel(0).unity(), None);
        let mono = default_fold(1, ChannelLayout::Mono).expect("mono");
        assert_eq!(mono.channel(0).unity(), Some(0));
        let surround = default_fold(6, ChannelLayout::Mono).expect("5.1");
        assert_eq!(
            weights(surround.channel(0)),
            [
                (0, 0.5),
                (1, 0.5),
                (2, HALF_POWER),
                (4, 0.5 * HALF_POWER),
                (5, 0.5 * HALF_POWER)
            ]
        );
    }

    #[test]
    fn counts_outside_the_contract_have_no_fold() {
        assert!(Downmix::new(&[], ChannelLayout::Stereo).is_none());
        assert!(
            Downmix::new(
                &[Speaker::FrontLeft; MAX_AUDIO_CHANNELS + 1],
                ChannelLayout::Stereo
            )
            .is_none()
        );
        for channels in 1..=MAX_AUDIO_CHANNELS {
            assert!(default_fold(channels, ChannelLayout::Stereo).is_some());
        }
    }

    #[test]
    fn a_source_folds_by_the_speakers_it_declares_not_by_its_plane_count() {
        // The center reaches both sides whatever its plane index.
        let surround = Downmix::new(
            &[
                Speaker::FrontLeft,
                Speaker::FrontRight,
                Speaker::FrontCenter,
            ],
            ChannelLayout::Stereo,
        )
        .expect("3.0");
        assert_eq!(weights(surround.channel(0)), [(0, 1.0), (2, HALF_POWER)]);
        assert_eq!(weights(surround.channel(1)), [(1, 1.0), (2, HALF_POWER)]);
        let lcr = Downmix::new(
            &[
                Speaker::FrontCenter,
                Speaker::FrontLeft,
                Speaker::FrontRight,
            ],
            ChannelLayout::Stereo,
        )
        .expect("C L R");
        assert_eq!(weights(lcr.channel(0)), [(0, HALF_POWER), (1, 1.0)]);
        assert_eq!(weights(lcr.channel(1)), [(0, HALF_POWER), (2, 1.0)]);
        let quad = Downmix::new(
            &[
                Speaker::FrontLeft,
                Speaker::FrontRight,
                Speaker::BackLeft,
                Speaker::BackRight,
            ],
            ChannelLayout::Stereo,
        )
        .expect("quad");
        assert_eq!(weights(quad.channel(0)), [(0, 1.0), (2, HALF_POWER)]);
        assert_eq!(weights(quad.channel(1)), [(1, 1.0), (3, HALF_POWER)]);
    }

    #[test]
    fn speakers_bs775_does_not_place_fold_where_they_stand() {
        let speakers = [
            Speaker::FrontLeftOfCenter,
            Speaker::TopFrontRight,
            Speaker::TopCenter,
            Speaker::LowFrequency2,
            Speaker::Unnamed,
        ];
        let fold = Downmix::new(&speakers, ChannelLayout::Stereo).expect("fold");
        assert_eq!(weights(fold.channel(0)), [(0, 1.0), (2, 0.5)]);
        assert_eq!(weights(fold.channel(1)), [(1, HALF_POWER), (2, 0.5)]);
        // Every plane of a 22.2 layout is folded.
        let all = alpha2_media::SpeakerLayout::default_for(MAX_AUDIO_CHANNELS).expect("22.2");
        let wide = Downmix::new(all.speakers(), ChannelLayout::Stereo).expect("22.2");
        assert!(
            weights(wide.channel(0))
                .iter()
                .any(|(plane, _)| *plane == 22)
        );
        assert!(
            weights(wide.channel(1))
                .iter()
                .any(|(plane, _)| *plane == 23)
        );
    }
}
