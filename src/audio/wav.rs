//! Writing a WAV file.
//!
//! Deliberately the plainest thing that works: 16-bit PCM, no compression,
//! no metadata. A recording is something you take somewhere else, and every
//! program that opens audio at all opens this.
//!
//! The header carries two lengths that are not known until the recording
//! stops. They are patched after every batch rather than only at the end, so
//! a take that is interrupted — the mixer killed, the machine losing power —
//! is still a correct file describing everything that reached the disk.
//!
//! That costs two seeks and eight bytes per batch, against a batch that is
//! thousands of samples. Writing them only at the end was the first version,
//! and a `SIGTERM` during testing produced a 2 MB file claiming to be empty,
//! which is exactly the case a recording most needs to survive.

use std::fs::File;
use std::io::{self, Seek, SeekFrom, Write};
use std::path::Path;

/// Bytes into the file where each patched length sits.
const RIFF_SIZE_AT: u64 = 4;
const DATA_SIZE_AT: u64 = 40;
/// Everything before the samples.
const HEADER_LEN: u32 = 44;

/// A file being written to.
#[derive(Debug)]
pub struct Writer {
    file: File,
    channels: u16,
    /// Sample frames written so far, for the header and for the elapsed
    /// time the deck shows.
    frames: u64,
    rate: u32,
}

impl Writer {
    /// Open a file and write its header.
    ///
    /// # Errors
    ///
    /// Fails if the file cannot be created or the header cannot be written.
    pub fn create(path: &Path, rate: u32, channels: u16) -> io::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut file = File::create(path)?;
        write_header(&mut file, rate, channels)?;
        Ok(Self {
            file,
            channels,
            frames: 0,
            rate,
        })
    }

    /// Append interleaved samples, converting to 16-bit.
    ///
    /// # Errors
    ///
    /// Fails if the write fails.
    pub fn write(&mut self, samples: &[f32]) -> io::Result<()> {
        let mut bytes = Vec::with_capacity(samples.len() * 2);
        for sample in samples {
            bytes.extend_from_slice(&to_i16(*sample).to_le_bytes());
        }
        self.file.write_all(&bytes)?;
        self.frames += samples.len() as u64 / u64::from(self.channels.max(1));
        self.patch_lengths()
    }

    /// Bring the two header lengths up to date with what has been written.
    fn patch_lengths(&mut self) -> io::Result<()> {
        let data = u32::try_from(self.frames * u64::from(self.channels) * 2).unwrap_or(u32::MAX);
        let end = SeekFrom::Start(u64::from(HEADER_LEN) + u64::from(data));
        self.file.seek(SeekFrom::Start(RIFF_SIZE_AT))?;
        self.file
            .write_all(&(data + HEADER_LEN - 8).to_le_bytes())?;
        self.file.seek(SeekFrom::Start(DATA_SIZE_AT))?;
        self.file.write_all(&data.to_le_bytes())?;
        self.file.seek(end)?;
        Ok(())
    }

    /// Frames written so far.
    #[must_use]
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// How long the recording is so far.
    ///
    /// Used by the tests rather than by the recorder, which reads the frame
    /// count directly and converts once for display.
    #[must_use]
    pub fn duration(&self) -> std::time::Duration {
        if self.rate == 0 {
            return std::time::Duration::ZERO;
        }
        std::time::Duration::from_secs_f64(self.frames as f64 / f64::from(self.rate))
    }

    /// Patch the two lengths in the header and close the file.
    ///
    /// # Errors
    ///
    /// Fails if seeking or writing fails.
    pub fn finish(mut self) -> io::Result<()> {
        self.patch_lengths()?;
        self.file.flush()
    }
}

fn write_header(file: &mut File, rate: u32, channels: u16) -> io::Result<()> {
    let bytes_per_frame = u32::from(channels) * 2;
    file.write_all(b"RIFF")?;
    file.write_all(&0u32.to_le_bytes())?;
    file.write_all(b"WAVEfmt ")?;
    file.write_all(&16u32.to_le_bytes())?;
    file.write_all(&1u16.to_le_bytes())?;
    file.write_all(&channels.to_le_bytes())?;
    file.write_all(&rate.to_le_bytes())?;
    file.write_all(&(rate * bytes_per_frame).to_le_bytes())?;
    file.write_all(&u16::try_from(bytes_per_frame).unwrap_or(4).to_le_bytes())?;
    file.write_all(&16u16.to_le_bytes())?;
    file.write_all(b"data")?;
    file.write_all(&0u32.to_le_bytes())
}

/// Convert one sample, clipping rather than wrapping.
///
/// A float buffer can carry more than full scale — a strip at +6 dB does —
/// and wrapping would turn a loud passage into noise.
fn to_i16(sample: f32) -> i16 {
    let clamped = sample.clamp(-1.0, 1.0);
    (clamped * 32767.0) as i16
}

#[cfg(test)]
mod tests {
    use super::{HEADER_LEN, Writer, to_i16};

    #[test]
    fn full_scale_maps_to_the_ends_without_wrapping() {
        assert_eq!(to_i16(1.0), 32767);
        assert_eq!(to_i16(-1.0), -32767);
        assert_eq!(to_i16(0.0), 0);
    }

    #[test]
    fn anything_past_full_scale_clips() {
        assert_eq!(to_i16(2.5), 32767);
        assert_eq!(to_i16(-9.0), -32767);
    }

    #[test]
    fn a_finished_file_declares_its_own_length() {
        let path = std::env::temp_dir().join("pipemeter-wav-test.wav");
        let mut writer = Writer::create(&path, 48_000, 2).expect("creates");
        writer
            .write(&[0.0, 0.0, 1.0, -1.0, 0.5, 0.5, 0.0, 0.0])
            .expect("writes");
        assert_eq!(writer.duration().as_secs(), 0);
        writer.finish().expect("finishes");

        let bytes = std::fs::read(&path).expect("reads back");
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");

        let data_len = u32::from_le_bytes(bytes[40..44].try_into().unwrap());
        assert_eq!(data_len, 16);
        assert_eq!(bytes.len() as u32, HEADER_LEN + data_len);

        let riff_len = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
        assert_eq!(riff_len, HEADER_LEN + data_len - 8);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_unfinished_file_still_declares_what_it_holds() {
        let path = std::env::temp_dir().join("pipemeter-wav-abandoned.wav");
        let mut writer = Writer::create(&path, 48_000, 2).expect("creates");
        writer.write(&[0.25; 64]).expect("writes");
        drop(writer);

        let bytes = std::fs::read(&path).expect("reads back");
        let data_len = u32::from_le_bytes(bytes[40..44].try_into().unwrap());
        assert_eq!(data_len, 128, "an abandoned take claimed the wrong length");
        assert_eq!(bytes.len() as u32, HEADER_LEN + data_len);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn writing_after_a_patch_does_not_land_in_the_header() {
        let path = std::env::temp_dir().join("pipemeter-wav-seek.wav");
        let mut writer = Writer::create(&path, 48_000, 2).expect("creates");
        writer.write(&[0.5; 4]).expect("first");
        writer.write(&[0.5; 4]).expect("second");
        writer.finish().expect("finishes");

        let bytes = std::fs::read(&path).expect("reads back");
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[36..40], b"data");
        assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()), 16);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_duration_follows_the_frames_written() {
        let path = std::env::temp_dir().join("pipemeter-wav-duration.wav");
        let mut writer = Writer::create(&path, 8, 2).expect("creates");
        writer.write(&[0.0; 32]).expect("writes");
        assert_eq!(writer.duration().as_secs(), 2);
        writer.finish().expect("finishes");
        let _ = std::fs::remove_file(&path);
    }
}
