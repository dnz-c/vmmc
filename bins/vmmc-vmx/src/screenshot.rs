//! Screenshots of the display adapter, saved as PNG files.

use std::fs::{self, File};
use std::io::{self, BufWriter};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use machine::{Display, Frame};

/// Where screenshots go, and the counter that names them.
pub struct Screenshots {
    dir: PathBuf,
    next: AtomicU32,
}

impl Screenshots {
    pub fn new(dir: &Path) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        Ok(Self {
            dir: dir.to_owned(),
            next: AtomicU32::new(1),
        })
    }

    /// Saves what `display` shows as `<prefix>-<n>.png`, or `<prefix>.png` when
    /// `numbered` is false. Returns the file, or `None` if no graphics mode is set.
    pub fn save(
        &self,
        display: &Display,
        prefix: &str,
        numbered: bool,
    ) -> io::Result<Option<PathBuf>> {
        let Some(frame) = display.snapshot() else {
            return Ok(None);
        };
        let name = if numbered {
            let n = self.next.fetch_add(1, Ordering::Relaxed);
            format!("{prefix}-{n:04}.png")
        } else {
            format!("{prefix}.png")
        };
        let path = self.dir.join(name);
        write_png(&path, &frame)?;
        Ok(Some(path))
    }
}

pub fn write_png(path: &Path, frame: &Frame) -> io::Result<()> {
    let file = BufWriter::new(File::create(path)?);
    let mut encoder = png::Encoder::new(file, frame.width, frame.height);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::Fast);
    let mut writer = encoder.write_header().map_err(io::Error::other)?;
    writer
        .write_image_data(&frame.rgb)
        .map_err(io::Error::other)?;
    writer.finish().map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_through_png() {
        let dir = tempfile::tempdir().unwrap();
        let frame = Frame {
            width: 3,
            height: 2,
            rgb: (0..18).collect(),
        };
        let path = dir.path().join("frame.png");
        write_png(&path, &frame).unwrap();
        let decoder = png::Decoder::new(io::BufReader::new(File::open(&path).unwrap()));
        let mut reader = decoder.read_info().unwrap();
        let mut pixels = vec![0u8; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut pixels).unwrap();
        assert_eq!((info.width, info.height), (3, 2));
        assert_eq!(&pixels[..info.buffer_size()], &frame.rgb[..]);
    }
}
