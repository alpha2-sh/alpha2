//! Pending media readbacks awaiting GPU completion.

use std::collections::VecDeque;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use crate::capture::FrameCapture;
use crate::error_message;
use crate::render_cache::BakedFrame;
use crate::trace::VideoFrameTrace;
use alpha2_media::AudioBlock;
use alpha2_native::Yuv420pFrameBuffer;
use alpha2_render::{PlaneTarget, ReadbackFailure, RenderError, Yuv420pReadback, Yuv420pTargets};
use alpha2_time::MediaTime;

/// One output in export order, waiting to be handed to the encoders.
pub enum PendingMedia {
    /// A rendered frame whose readback may still be mapping.
    Video {
        index: u64,
        time: MediaTime,
        readback: PendingVideoReadback,
        /// Handoff timestamps collected while this frame was being produced.
        /// Left at its default when no trace was requested, and boxed because
        /// the row is the largest part of this variant and the audio one
        /// carries nothing like it.
        trace: Box<VideoFrameTrace>,
        /// The render-cache key to bake this frame under, and whether it is
        /// pinned; `None` when it is not baked.
        bake: Option<(String, bool)>,
    },
    /// A frame an earlier export baked, taken instead of rendered.
    Baked {
        index: u64,
        time: MediaTime,
        frame: Box<BakedFrame>,
        trace: Box<VideoFrameTrace>,
    },
    /// A frame the CPU renderer drew straight into its encoder frame.
    Rendered {
        index: u64,
        time: MediaTime,
        frame: crate::encode::PooledFrame,
        trace: Box<VideoFrameTrace>,
    },
    Audio(AudioBlock),
}

/// A frame's YUV readback and the channel its asynchronous mapping reports on.
pub struct PendingVideoReadback {
    pub readback: Box<Yuv420pReadback>,
    pub mapping: Receiver<(Result<(), RenderError>, Duration)>,
    /// The mapping result and its latency once received, so repeated polls see the same
    /// outcome.
    pub mapping_ready: Option<(Result<(), RenderError>, Duration)>,
}

/// A failed mapping, in the renderer's words, which say at which step.
fn mapping_error(result: Result<(), RenderError>) -> Result<(), String> {
    result.map_err(error_message)
}

const DROPPED: RenderError = RenderError::TextureReadback(ReadbackFailure::CallbackDropped);

impl PendingVideoReadback {
    pub fn new(readback: Yuv420pReadback) -> Self {
        let (sender, mapping) = mpsc::channel();
        let mapping_started = Instant::now();
        readback.map_async(move |mapped| {
            let _ = sender.send((mapped, mapping_started.elapsed()));
        });
        Self {
            readback: Box::new(readback),
            mapping,
            mapping_ready: None,
        }
    }

    pub fn staging_bytes(&self) -> u64 {
        self.readback.staging_bytes()
    }

    /// Polls the device once and reports whether the mapping has completed.
    ///
    /// # Errors
    /// Returns the mapping failure, or a dropped-callback failure when the mapping channel
    /// closed without an answer.
    pub fn try_ready(&mut self) -> Result<bool, String> {
        if let Some((mapped, _)) = self.mapping_ready {
            return mapping_error(mapped).map(|()| true);
        }
        self.readback.poll();
        match self.mapping.try_recv() {
            Ok(mapping) => {
                self.mapping_ready = Some(mapping);
                mapping_error(mapping.0).map(|()| true)
            }
            Err(TryRecvError::Empty) => Ok(false),
            Err(TryRecvError::Disconnected) => mapping_error(Err(DROPPED)).map(|()| false),
        }
    }

    /// Blocks until the mapping completes.
    ///
    /// # Errors
    /// Returns a submission wait failure or the mapping failure.
    pub fn wait_ready(&mut self) -> Result<(), String> {
        if self.try_ready()? {
            return Ok(());
        }
        self.readback.wait_for_submission().map_err(error_message)?;
        let mapping = self
            .mapping
            .recv()
            .unwrap_or((Err(DROPPED), Duration::ZERO));
        self.mapping_ready = Some(mapping);
        mapping_error(mapping.0)
    }

    /// Packs straight into the encoder frame that will carry this output.
    ///
    /// The encode worker then makes no per-frame allocation or copy; see
    /// [`crate::encode::FramePool`].
    ///
    /// With a `capture`, the packed frame is also written there, as encoded.
    pub fn finish_into(
        self,
        frame: &mut Yuv420pFrameBuffer,
        capture: Option<&mut FrameCapture>,
        bake: Option<&mut BakedFrame>,
    ) -> Result<(), String> {
        // A texel readback packs another layout here, which nothing captures or bakes.
        if self.readback.pixel_layout().is_some() {
            let mut planes = frame.begin_write_planes().map_err(error_message)?;
            let mut targets: Vec<PlaneTarget<'_>> = planes
                .iter_mut()
                .map(|(bytes, stride)| PlaneTarget::new(bytes, *stride))
                .collect();
            return self
                .readback
                .finish_into_planes(&mut targets)
                .map_err(error_message);
        }
        let (width, height) = frame.dimensions();
        let planes = frame.begin_write().map_err(error_message)?;
        let (y_stride, chroma_stride) = (planes.y_stride, planes.chroma_stride);
        let mut targets = Yuv420pTargets {
            y: PlaneTarget::new(planes.y, y_stride),
            u: PlaneTarget::new(planes.u, chroma_stride),
            v: PlaneTarget::new(planes.v, chroma_stride),
        };
        self.readback
            .finish_into(&mut targets)
            .map_err(error_message)?;
        if let Some(capture) = capture {
            capture.write(
                (targets.y.bytes(), y_stride),
                (targets.u.bytes(), chroma_stride),
                (targets.v.bytes(), chroma_stride),
            )?;
        }
        if let Some(bake) = bake {
            let chroma = (width.div_ceil(2), height.div_ceil(2));
            bake.y = pack_rows(targets.y.bytes(), y_stride, (width, height));
            bake.u = pack_rows(targets.u.bytes(), chroma_stride, chroma);
            bake.v = pack_rows(targets.v.bytes(), chroma_stride, chroma);
        }
        Ok(())
    }

    /// Time from requesting the mapping to its callback; zero until the mapping is received.
    pub fn mapping_latency(&self) -> Duration {
        self.mapping_ready
            .map_or(Duration::ZERO, |(_, latency)| latency)
    }
}

/// Writes a frame drawn into its encoder frame to `capture`, and into `bake`
/// for the render cache, as [`PendingVideoReadback::finish_into`] does for a
/// readback.
///
/// # Errors
/// Returns the frame's or the capture's failure.
pub fn finish_drawn(
    frame: &mut Yuv420pFrameBuffer,
    capture: Option<&mut FrameCapture>,
    bake: Option<&mut BakedFrame>,
) -> Result<(), String> {
    if capture.is_none() && bake.is_none() {
        return Ok(());
    }
    let (width, height) = frame.dimensions();
    let planes = frame.begin_write().map_err(error_message)?;
    let (y_stride, chroma_stride) = (planes.y_stride, planes.chroma_stride);
    if let Some(capture) = capture {
        capture.write(
            (&*planes.y, y_stride),
            (&*planes.u, chroma_stride),
            (&*planes.v, chroma_stride),
        )?;
    }
    if let Some(bake) = bake {
        let chroma = (width.div_ceil(2), height.div_ceil(2));
        bake.y = pack_rows(planes.y, y_stride, (width, height));
        bake.u = pack_rows(planes.u, chroma_stride, chroma);
        bake.v = pack_rows(planes.v, chroma_stride, chroma);
    }
    Ok(())
}

/// Writes a baked frame's planes into the encoder frame that will carry it,
/// and to `capture` as a rendered frame would be.
pub fn unpack_baked(
    baked: &BakedFrame,
    frame: &mut Yuv420pFrameBuffer,
    capture: Option<&mut FrameCapture>,
) -> Result<(), String> {
    let (width, height) = frame.dimensions();
    let chroma = (width.div_ceil(2), height.div_ceil(2));
    let planes = frame.begin_write().map_err(error_message)?;
    let (y_stride, chroma_stride) = (planes.y_stride, planes.chroma_stride);
    unpack_rows(&baked.y, planes.y, y_stride, (width, height));
    unpack_rows(&baked.u, planes.u, chroma_stride, chroma);
    unpack_rows(&baked.v, planes.v, chroma_stride, chroma);
    if let Some(capture) = capture {
        capture.write(
            (&baked.y, width),
            (&baked.u, chroma.0),
            (&baked.v, chroma.0),
        )?;
    }
    Ok(())
}

/// A plane's visible rows with their padding removed.
///
/// Allocated once at its size and filled a row at a time (the whole plane
/// at once when it has no padding): it runs on the export's coordinator for
/// every frame a render cache bakes, where collecting it byte by byte cost
/// about 5.5 ms a 4K frame against 0.3 ms.
fn pack_rows(plane: &[u8], stride: usize, (width, height): (usize, usize)) -> Vec<u8> {
    let mut packed = Vec::with_capacity(width * height);
    if stride == width {
        packed.extend_from_slice(&plane[..width * height]);
        return packed;
    }
    for row in 0..height {
        packed.extend_from_slice(&plane[row * stride..row * stride + width]);
    }
    packed
}

fn unpack_rows(packed: &[u8], plane: &mut [u8], stride: usize, (width, height): (usize, usize)) {
    for row in 0..height {
        plane[row * stride..row * stride + width]
            .copy_from_slice(&packed[row * width..(row + 1) * width]);
    }
}

/// Reports what the front of the queue is ready to hand off: `Some(false)` for audio,
/// `Some(true)` for a baked frame or a video frame whose mapping completed, and `None` when the
/// queue is empty or the front frame is still mapping.
///
/// # Errors
/// Returns the front frame's mapping failure, tagged with the readback phase and its index.
pub fn ready_pending_media(pending: &mut VecDeque<PendingMedia>) -> Result<Option<bool>, String> {
    match pending.front_mut() {
        Some(PendingMedia::Audio(_)) => Ok(Some(false)),
        Some(PendingMedia::Baked { .. } | PendingMedia::Rendered { .. }) => Ok(Some(true)),
        Some(PendingMedia::Video {
            readback, index, ..
        }) => readback
            .try_ready()
            .map(|ready| ready.then_some(true))
            .map_err(|error| {
                crate::failure::tag_phase(
                    crate::failure::ExportPhase::Readback,
                    Some(*index),
                    error,
                )
            }),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A baked plane is its visible rows, allocated once at their size, because the
    /// coordinator packs one for every frame a render cache bakes and growing the vector
    /// byte by byte was measurably slow.
    #[test]
    fn a_plane_is_packed_into_one_allocation_of_its_visible_rows() {
        let (width, height, stride) = (100, 7, 128);
        let plane: Vec<u8> = (0..stride * height)
            .map(|index| u8::try_from(index % 251).expect("fits"))
            .collect();
        let packed = pack_rows(&plane, stride, (width, height));
        assert_eq!(packed.len(), width * height);
        assert_eq!(packed.capacity(), width * height);
        for row in 0..height {
            assert_eq!(
                packed[row * width..(row + 1) * width],
                plane[row * stride..row * stride + width]
            );
        }
        let unpadded = pack_rows(&plane, width, (width, height));
        assert_eq!(unpadded, plane[..width * height]);
        assert_eq!(unpadded.capacity(), width * height);
        let mut round_trip = vec![0; stride * height];
        unpack_rows(&packed, &mut round_trip, stride, (width, height));
        assert_eq!(pack_rows(&round_trip, stride, (width, height)), packed);
    }
}
