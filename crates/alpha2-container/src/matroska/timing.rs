//! Packet times as `FFmpeg` hands them out for Matroska: its codec parsers' durations and its
//! generic demuxing layer's fills (`compute_pkt_fields`), in the track's time base.

use super::describe::{Parser, Timing};
use crate::audio::{flac, mpeg};

/// A frame as the demuxer reads it from a block.
pub(crate) struct Raw<'a> {
    pub timecode: Option<i64>,
    pub duration: i64,
    pub keyframe: bool,
    pub data: &'a [u8],
    /// The first frame of the track's first block in the file.
    pub first: bool,
}

/// The times a packet ends up with; `dts` is `None` where `FFmpeg` leaves it unset.
pub(crate) struct Times {
    pub pts: Option<i64>,
    pub dts: Option<i64>,
    pub duration: i64,
    pub keyframe: bool,
}

/// One track's timing state from where reading started.
#[derive(Clone)]
pub(crate) struct Clock {
    timing: Timing,
    intra: bool,
    decode_times: bool,
    /// Decode times come from H.264 or HEVC reordering rather than being equal to presentation
    /// times (`FFmpeg`'s codecs that are not one-in-one-out).
    reordered: bool,
    /// The time base, `num / den` seconds.
    time_base: (i64, i64),
    pts_buffer: Vec<Option<i64>>,
    cur_dts: Option<i64>,
    parser_samples: i64,
    vorbis_previous: i64,
}

impl Clock {
    pub fn new(
        timing: Timing,
        intra: bool,
        decode_times: bool,
        reordered: bool,
        time_base: (u64, u64),
    ) -> Self {
        let depth = match timing {
            Timing::Video { depth, .. } => depth,
            _ => 0,
        };
        let vorbis_previous = match &timing {
            Timing::Parsed {
                parser: Parser::Vorbis(modes),
                ..
            } => modes.blocksizes[usize::from(modes.long.first().copied().unwrap_or(false))],
            _ => 0,
        };
        Self {
            timing,
            intra,
            decode_times,
            reordered,
            time_base: (
                i64::try_from(time_base.0).unwrap_or(1),
                i64::try_from(time_base.1).unwrap_or(1),
            ),
            pts_buffer: vec![None; depth + 1],
            cur_dts: None,
            parser_samples: 0,
            vorbis_previous,
        }
    }

    /// `samples` at `rate` in the time base, rounded down (`AV_ROUND_DOWN`).
    fn to_time_base(&self, samples: i64, rate: u32) -> i64 {
        let (num, den) = self.time_base;
        let scaled = (i128::from(samples) * i128::from(den))
            .div_euclid(i128::from(rate.max(1)) * i128::from(num.max(1)));
        i64::try_from(scaled).unwrap_or(i64::MAX)
    }

    /// The samples a parser finds in `data`, `None` when it cannot read the frame.
    fn parsed_samples(&mut self, parser: &Parser, data: &[u8]) -> Option<i64> {
        match parser {
            Parser::Opus => crate::audio::ogg::opus_duration(data),
            Parser::Vorbis(modes) => {
                let first = *data.first()?;
                modes.duration(first, &mut self.vorbis_previous)
            }
            Parser::Flac => flac::frame_header(data).map(|header| i64::from(header.blocksize)),
            Parser::Ac3 => ac3_samples(data),
            Parser::Dts => dts_samples(data),
            Parser::Mpeg => {
                let header = u32::from_be_bytes(data.get(..4)?.try_into().ok()?);
                mpeg::decode(header).map(|header| header.samples)
            }
        }
    }

    /// The times of the next frame, read in order since the clock started.
    pub fn times(&mut self, raw: &Raw<'_>) -> Times {
        let (mut pts, mut dts) = if self.decode_times {
            (None, raw.timecode)
        } else {
            (raw.timecode, None)
        };
        let mut duration = raw.duration;
        let timing = self.timing.clone();
        if let Timing::Parsed { parser, rate } = &timing {
            if let Some(samples) = self.parsed_samples(parser, raw.data) {
                self.parser_samples = samples;
            }
            if *rate > 0 {
                duration = self.to_time_base(self.parser_samples, *rate);
            }
        }
        // The exact duration in seconds, for stepping the next missing timestamp.
        let mut step = (duration.saturating_mul(self.time_base.0), self.time_base.1);
        if duration <= 0 {
            let filled = match timing {
                Timing::Video {
                    frame: Some((num, den)),
                    ..
                } => i64::try_from(den).ok().zip(i64::try_from(num).ok()),
                Timing::Aac { frame, rate } if !raw.first => Some((frame, i64::from(rate))),
                Timing::Pcm { bytes, rate } if bytes > 0 => Some((
                    i64::try_from(raw.data.len() / bytes as usize).unwrap_or(0),
                    i64::from(rate),
                )),
                _ => None,
            };
            if let Some((num, den)) = filled.filter(|(num, den)| *num > 0 && *den > 0) {
                let (tb_num, tb_den) = self.time_base;
                let scaled = (i128::from(num) * i128::from(tb_den))
                    .div_euclid(i128::from(den) * i128::from(tb_num));
                duration = i64::try_from(scaled).unwrap_or(0);
                step = (num, den);
            }
        }
        let depth = self.pts_buffer.len() - 1;
        if depth == 0 && !self.reordered {
            if pts.is_none() {
                pts = dts.or(self.cur_dts);
            }
            dts = pts;
            if let Some(pts) = pts {
                self.cur_dts = Some(add_stable(pts, self.time_base, step));
            }
        }
        if let Some(pts) = pts {
            self.pts_buffer[0] = Some(pts);
            for at in 0..depth {
                if self.pts_buffer[at] > self.pts_buffer[at + 1] {
                    self.pts_buffer.swap(at, at + 1);
                } else {
                    break;
                }
            }
            if depth > 0 || self.reordered {
                dts = dts.or(self.pts_buffer[0]);
            }
        }
        if let Some(dts) = dts
            && self.cur_dts.is_none_or(|current| dts > current)
        {
            self.cur_dts = Some(dts);
        }
        Times {
            pts,
            dts,
            duration: duration.max(0),
            keyframe: raw.keyframe || self.intra,
        }
    }
}

/// `ts` in `time_base` advanced by `step` seconds without drifting (`av_add_stable`).
fn add_stable(ts: i64, time_base: (i64, i64), step: (i64, i64)) -> i64 {
    let (tb_num, tb_den) = (i128::from(time_base.0), i128::from(time_base.1));
    let (step_num, step_den) = (i128::from(step.0), i128::from(step.1.max(1)));
    let m = step_num * tb_den;
    let d = step_den * tb_num;
    if d == 0 {
        return ts;
    }
    if m % d == 0 {
        return i64::try_from(i128::from(ts) + m / d).unwrap_or(ts);
    }
    if m < d {
        return ts;
    }
    let rescale = |value: i128, from: (i128, i128), to: (i128, i128)| -> i128 {
        let numerator = value * from.0 * to.1;
        let denominator = from.1 * to.0;
        let half = denominator / 2;
        if numerator < 0 {
            -((-numerator + half) / denominator)
        } else {
            (numerator + half) / denominator
        }
    };
    let old = rescale(i128::from(ts), (tb_num, tb_den), (step_num, step_den));
    let old_ts = rescale(old, (step_num, step_den), (tb_num, tb_den));
    let next = rescale(old + 1, (step_num, step_den), (tb_num, tb_den));
    i64::try_from(next + (i128::from(ts) - old_ts)).unwrap_or(ts)
}

/// The samples of an AC-3 or E-AC-3 syncframe: 256 per audio block.
fn ac3_samples(data: &[u8]) -> Option<i64> {
    if data.get(..2)? != [0x0b, 0x77] {
        return None;
    }
    let bitstream_id = data.get(5)? >> 3;
    if bitstream_id <= 10 {
        return Some(1536);
    }
    let byte = *data.get(4)?;
    let blocks = if byte >> 6 == 3 {
        6
    } else {
        [1, 2, 3, 6][usize::from((byte >> 4) & 3)]
    };
    Some(256 * blocks)
}

/// The samples of a DTS core frame: 32 per PCM block (`NBLKS` + 1).
fn dts_samples(data: &[u8]) -> Option<i64> {
    if data.get(..4)? != [0x7f, 0xfe, 0x80, 0x01] {
        return None;
    }
    let blocks = ((u16::from(data[4] & 1) << 6) | u16::from(data[5] >> 2)) + 1;
    Some(32 * i64::from(blocks))
}
