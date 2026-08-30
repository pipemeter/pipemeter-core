//! Writing a WAV file.
//!
//! Deliberately plain: uncompressed, no metadata. A recording is something
//! you take somewhere else, and every program that opens audio at all opens
//! this.
//!
//! Three sample formats, which is what the Recorder Options window offers:
//! 16- and 24-bit integer PCM, and 32-bit float. The float one is not just
//! a wider integer - it carries samples past full scale, which the mixer
//! really does produce when a strip is pushed, and which the integer
//! formats have to clip away.
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

/// How a sample is stored.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Depth {
    /// 16-bit integer: what every program reads, and the default.
    #[default]
    Bits16,
    /// 24-bit integer, for a take that will be worked on afterwards.
    Bits24,
    /// 32-bit float, which is the only one that survives a sample past
    /// full scale rather than clipping it.
    Float32,
}

impl Depth {
    /// Bytes one sample takes on disk.
    #[must_use]
    pub fn bytes(self) -> u32 {
        match self {
            Self::Bits16 => 2,
            Self::Bits24 => 3,
            Self::Float32 => 4,
        }
    }

    /// The `bits per sample` the header declares.
    #[must_use]
    pub fn bits(self) -> u16 {
        u16::try_from(self.bytes() * 8).unwrap_or(16)
    }

    /// The WAV format tag: 1 for integer PCM, 3 for IEEE float.
    ///
    /// Getting this wrong is the difference between a file that opens and
    /// one that plays as noise, so it travels with the depth rather than
    /// being decided at the header.
    #[must_use]
    pub fn format_tag(self) -> u16 {
        match self {
            Self::Bits16 | Self::Bits24 => 1,
            Self::Float32 => 3,
        }
    }

    /// How the Recorder Options window names it.
    #[must_use]
    pub fn caption(self) -> &'static str {
        match self {
            Self::Bits16 => "16 bits",
            Self::Bits24 => "24 bits",
            Self::Float32 => "32 bits (float)",
        }
    }

    /// How it is written to the settings file, and read back.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bits16 => "16",
            Self::Bits24 => "24",
            Self::Float32 => "32f",
        }
    }

    /// The other half of [`Depth::as_str`]. Anything unrecognised is 16-bit,
    /// which is the one every program can read.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        match text {
            "24" => Self::Bits24,
            "32f" => Self::Float32,
            _ => Self::Bits16,
        }
    }

    /// Encode one sample.
    ///
    /// The integer formats clip rather than wrap: a float buffer can carry
    /// more than full scale - a strip at +6 dB does - and wrapping would
    /// turn a loud passage into noise. Float keeps what it was given,
    /// which is the reason to choose it.
    fn encode(self, sample: f32, into: &mut Vec<u8>) {
        match self {
            Self::Bits16 => into.extend_from_slice(&to_i16(sample).to_le_bytes()),
            Self::Bits24 => into.extend_from_slice(&to_i24(sample)),
            Self::Float32 => into.extend_from_slice(&sample.to_le_bytes()),
        }
    }
}

/// A file being written to.
#[derive(Debug)]
pub struct Writer {
    file: File,
    channels: u16,
    depth: Depth,
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
    pub fn create(path: &Path, rate: u32, channels: u16, depth: Depth) -> io::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut file = File::create(path)?;
        write_header(&mut file, rate, channels, depth)?;
        Ok(Self {
            file,
            channels,
            depth,
            frames: 0,
            rate,
        })
    }

    /// Append interleaved samples in whatever format this file holds.
    ///
    /// # Errors
    ///
    /// Fails if the write fails.
    pub fn write(&mut self, samples: &[f32]) -> io::Result<()> {
        let mut bytes = Vec::with_capacity(samples.len() * self.depth.bytes() as usize);
        for sample in samples {
            self.depth.encode(*sample, &mut bytes);
        }
        self.file.write_all(&bytes)?;
        self.frames += samples.len() as u64 / u64::from(self.channels.max(1));
        self.patch_lengths()
    }

    /// Bring the two header lengths up to date with what has been written.
    fn patch_lengths(&mut self) -> io::Result<()> {
        let data = u32::try_from(
            self.frames * u64::from(self.channels) * u64::from(self.depth.bytes()),
        )
        .unwrap_or(u32::MAX);
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

fn write_header(file: &mut File, rate: u32, channels: u16, depth: Depth) -> io::Result<()> {
    let bytes_per_frame = u32::from(channels) * depth.bytes();
    file.write_all(b"RIFF")?;
    file.write_all(&0u32.to_le_bytes())?;
    file.write_all(b"WAVEfmt ")?;
    file.write_all(&16u32.to_le_bytes())?;
    file.write_all(&depth.format_tag().to_le_bytes())?;
    file.write_all(&channels.to_le_bytes())?;
    file.write_all(&rate.to_le_bytes())?;
    file.write_all(&(rate * bytes_per_frame).to_le_bytes())?;
    file.write_all(&u16::try_from(bytes_per_frame).unwrap_or(4).to_le_bytes())?;
    file.write_all(&depth.bits().to_le_bytes())?;
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

/// The same, three bytes wide and little-endian.
fn to_i24(sample: f32) -> [u8; 3] {
    let clamped = sample.clamp(-1.0, 1.0);
    let value = (clamped * 8_388_607.0) as i32;
    let [a, b, c, _] = value.to_le_bytes();
    [a, b, c]
}

#[cfg(test)]
mod tests {
    use super::{Depth, HEADER_LEN, Writer, to_i16, to_i24};

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

    /// The header has to agree with the bytes: a 24-bit file that says 16
    /// plays as noise, and one whose block align is wrong plays at the
    /// wrong speed.
    #[test]
    fn each_depth_describes_itself_correctly() {
        for (depth, tag, bits, bytes) in [
            (Depth::Bits16, 1u16, 16u16, 2u32),
            (Depth::Bits24, 1, 24, 3),
            (Depth::Float32, 3, 32, 4),
        ] {
            let path = std::env::temp_dir().join(format!("pipemeter-wav-{}.wav", depth.as_str()));
            let mut writer = Writer::create(&path, 48_000, 2, depth).expect("creates");
            writer.write(&[0.5, -0.5]).expect("writes");
            writer.finish().expect("finishes");

            let b = std::fs::read(&path).expect("reads back");
            let at = |i: usize| u16::from_le_bytes(b[i..i + 2].try_into().unwrap());
            assert_eq!(at(20), tag, "{depth:?} format tag");
            assert_eq!(at(34), bits, "{depth:?} bits per sample");
            assert_eq!(u32::from(at(32)), 2 * bytes, "{depth:?} block align");
            assert_eq!(
                u32::from_le_bytes(b[28..32].try_into().unwrap()),
                48_000 * 2 * bytes,
                "{depth:?} byte rate"
            );

            // One frame of two samples, and nothing else.
            let data = u32::from_le_bytes(b[40..44].try_into().unwrap());
            assert_eq!(data, 2 * bytes, "{depth:?} data length");
            assert_eq!(b.len() as u32, HEADER_LEN + data);
            let _ = std::fs::remove_file(&path);
        }
    }

    #[test]
    fn twenty_four_bit_clips_at_full_scale() {
        assert_eq!(to_i24(1.0), [0xff, 0xff, 0x7f]);
        assert_eq!(to_i24(0.0), [0, 0, 0]);
        assert_eq!(to_i24(4.0), to_i24(1.0), "past full scale should clip");
    }

    /// The reason to choose float: it is the only one that keeps a sample
    /// the integer formats would have to clip away.
    #[test]
    fn float_keeps_what_is_past_full_scale() {
        let path = std::env::temp_dir().join("pipemeter-wav-float-hot.wav");
        let mut writer = Writer::create(&path, 48_000, 1, Depth::Float32).expect("creates");
        writer.write(&[2.5]).expect("writes");
        writer.finish().expect("finishes");

        let b = std::fs::read(&path).expect("reads back");
        let sample = f32::from_le_bytes(b[44..48].try_into().unwrap());
        assert!((sample - 2.5).abs() < f32::EPSILON, "got {sample}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_spelling_round_trips() {
        for depth in [Depth::Bits16, Depth::Bits24, Depth::Float32] {
            assert_eq!(Depth::parse(depth.as_str()), depth);
        }
        assert_eq!(Depth::parse("nonsense"), Depth::Bits16);
    }

    #[test]
    fn a_finished_file_declares_its_own_length() {
        let path = std::env::temp_dir().join("pipemeter-wav-test.wav");
        let mut writer = Writer::create(&path, 48_000, 2, Depth::Bits16).expect("creates");
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
        let mut writer = Writer::create(&path, 48_000, 2, Depth::Bits16).expect("creates");
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
        let mut writer = Writer::create(&path, 48_000, 2, Depth::Bits16).expect("creates");
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
        let mut writer = Writer::create(&path, 8, 2, Depth::Bits16).expect("creates");
        writer.write(&[0.0; 32]).expect("writes");
        assert_eq!(writer.duration().as_secs(), 2);
        writer.finish().expect("finishes");
        let _ = std::fs::remove_file(&path);
    }
}
