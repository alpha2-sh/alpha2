//! Test instrumentation: the exported frames, exactly as encoded.
//!
//! The codec's loss is larger than the renderer differences that matter, so with
//! `ALPHA2_EXPORT_FRAMES_Y4M` set an export also writes each 8-bit I420 frame it hands the encoder
//! to a YUV4MPEG2 file, which `scripts/test-host-cli-parity.mjs` compares with the browser
//! engine's frames.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use alpha2_media::ChromaLocation;
use alpha2_render::RenderSize;
use alpha2_time::FrameRate;

use crate::error_message;

pub struct FrameCapture {
    writer: BufWriter<File>,
    width: usize,
    height: usize,
}

impl FrameCapture {
    /// The chroma siting follows `chroma`: `420jpeg` (centred), `420mpeg2` (left column) or
    /// `420paldv` (top-left). The range is limited, as `ColorMetadata::EXPORT` codes it.
    ///
    /// # Errors
    /// Returns the I/O error that stopped the file or its header.
    pub fn create(
        path: &Path,
        size: RenderSize,
        rate: FrameRate,
        chroma: ChromaLocation,
    ) -> Result<Self, String> {
        let siting = match chroma {
            ChromaLocation::Unspecified | ChromaLocation::Center => "420jpeg",
            ChromaLocation::Left => "420mpeg2",
            ChromaLocation::TopLeft => "420paldv",
        };
        let file = File::create(path).map_err(error_message)?;
        let mut writer = BufWriter::new(file);
        writeln!(
            writer,
            "YUV4MPEG2 W{} H{} F{}:{} Ip A1:1 C{siting} XCOLORRANGE=LIMITED",
            size.width,
            size.height,
            rate.numerator(),
            rate.denominator()
        )
        .map_err(error_message)?;
        Ok(Self {
            writer,
            width: usize::try_from(size.width).map_err(error_message)?,
            height: usize::try_from(size.height).map_err(error_message)?,
        })
    }

    /// # Errors
    /// Returns an error when a plane is shorter than the frame or the write fails.
    pub fn write(
        &mut self,
        y: (&[u8], usize),
        u: (&[u8], usize),
        v: (&[u8], usize),
    ) -> Result<(), String> {
        self.writer.write_all(b"FRAME\n").map_err(error_message)?;
        let chroma = (self.width.div_ceil(2), self.height.div_ceil(2));
        for ((plane, stride), (width, height)) in
            [(y, (self.width, self.height)), (u, chroma), (v, chroma)]
        {
            for row in 0..height {
                let start = row * stride;
                let bytes = plane
                    .get(start..start + width)
                    .ok_or_else(|| "a captured plane is shorter than its frame".to_owned())?;
                self.writer.write_all(bytes).map_err(error_message)?;
            }
        }
        Ok(())
    }

    pub fn finish(mut self) -> Result<(), String> {
        self.writer.flush().map_err(error_message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_are_written_row_by_row_without_their_padding() {
        let path = std::env::temp_dir().join(format!("alpha2-capture-{}.y4m", std::process::id()));
        let mut capture = FrameCapture::create(
            &path,
            RenderSize::new(4, 2).expect("size"),
            FrameRate::new(24, 1).expect("rate"),
            ChromaLocation::Left,
        )
        .expect("capture");
        // Rows padded to 6 and 3 bytes, as an encoder frame's may be.
        let y = [1, 2, 3, 4, 0, 0, 5, 6, 7, 8];
        let (u, v) = ([9, 10, 0], [11, 12, 0]);
        capture.write((&y, 6), (&u, 3), (&v, 3)).expect("frame");
        capture.finish().expect("flush");
        let bytes = std::fs::read(&path).expect("read");
        std::fs::remove_file(&path).expect("cleanup");
        let header = b"YUV4MPEG2 W4 H2 F24:1 Ip A1:1 C420mpeg2 XCOLORRANGE=LIMITED\nFRAME\n";
        assert_eq!(&bytes[..header.len()], header);
        assert_eq!(
            &bytes[header.len()..],
            [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]
        );
    }
}
