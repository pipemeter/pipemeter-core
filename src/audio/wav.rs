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

/// Bytes into the file where each patched length sits, for a plain WAV.
const RIFF_SIZE_AT: u64 = 4;
/// Everything before the samples, for a plain WAV.
const HEADER_LEN: u32 = 44;

/// A Broadcast Wave description chunk, which is a fixed 602 bytes before
/// any coding history. We write it empty: the point of BWF here is that a
/// program expecting one will open the file, not that we have anything to
/// put in it.
const BEXT_LEN: u32 = 602;

/// `RF64`'s size chunk: three 64-bit lengths and an empty table.
const DS64_LEN: u32 = 28;

/// Everything before the samples in an AIFF: FORM, COMM and SSND.
const AIFF_HEADER_LEN: u32 = 54;
/// Where AIFF keeps the numbers that grow: the FORM length, the frame
/// count inside COMM, and the SSND length.
const AIFF_FORM_SIZE_AT: u64 = 4;
const AIFF_FRAMES_AT: u64 = 22;
const AIFF_SSND_SIZE_AT: u64 = 42;

/// Which container the samples are wrapped in.
///
/// All three hold the same PCM in the same order and differ only in what
/// is written before it, which is why they share a writer. What they are
/// for differs: BWF is what a broadcast or post-production tool expects,
/// and RF64 is the one that does not stop at four gigabytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Container {
    /// Plain RIFF/WAVE. Read by everything, and capped at 4 GB.
    #[default]
    Wav,
    /// Apple's interchange format. Big-endian, and a different set of
    /// chunks, but the same samples underneath.
    Aiff,
    /// RIFF/WAVE carrying a Broadcast Wave description chunk.
    Bwf,
    /// The 64-bit RIFF, for takes that outgrow a WAV. Ours, not the
    /// original's - it is the only one of these that solves a problem a
    /// long recording actually runs into.
    Rf64,
}

impl Container {
    /// The file extension, without the dot.
    #[must_use]
    pub fn extension(self) -> &'static str {
        match self {
            // BWF and RF64 are both `.wav` by convention: they are WAV
            // files, and a player that does not know the extra chunks
            // skips them.
            Self::Wav | Self::Bwf | Self::Rf64 => "wav",
            Self::Aiff => "aiff",
        }
    }

    /// Whether samples are stored the other way round.
    ///
    /// AIFF is big-endian and always signed; the RIFF family is
    /// little-endian and stores 8-bit unsigned. Nothing else differs about
    /// the samples themselves.
    fn big_endian(self) -> bool {
        self == Self::Aiff
    }

    /// How the Recorder Options window names it.
    #[must_use]
    pub fn caption(self) -> &'static str {
        match self {
            Self::Wav => "WAV",
            Self::Aiff => "AIFF",
            Self::Bwf => "BWF",
            Self::Rf64 => "RF64",
        }
    }

    /// How it is written to the settings file, and read back.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Wav => "wav",
            Self::Aiff => "aiff",
            Self::Bwf => "bwf",
            Self::Rf64 => "rf64",
        }
    }

    /// The other half of [`Container::as_str`]. Anything unrecognised is a
    /// plain WAV, which every program can read.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        match text {
            "aiff" => Self::Aiff,
            "bwf" => Self::Bwf,
            "rf64" => Self::Rf64,
            _ => Self::Wav,
        }
    }

    /// The next one, for a box that cycles through them.
    #[must_use]
    pub fn next(self) -> Self {
        match self {
            Self::Wav => Self::Aiff,
            Self::Aiff => Self::Bwf,
            Self::Bwf => Self::Rf64,
            Self::Rf64 => Self::Wav,
        }
    }

    /// Everything before the samples.
    fn header_len(self) -> u32 {
        match self {
            Self::Wav => HEADER_LEN,
            // The extra chunk, plus its own name and length.
            Self::Bwf => HEADER_LEN + 8 + BEXT_LEN,
            Self::Rf64 => HEADER_LEN + 8 + DS64_LEN,
            Self::Aiff => AIFF_HEADER_LEN,
        }
    }

    /// Where the data length sits: the last field of the header, whatever
    /// chunks came before it.
    fn data_size_at(self) -> u64 {
        u64::from(self.header_len()) - 4
    }
}

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

    /// The next one, for a box that cycles through them.
    #[must_use]
    pub fn next(self) -> Self {
        match self {
            Self::Bits16 => Self::Bits24,
            Self::Bits24 => Self::Float32,
            Self::Float32 => Self::Bits16,
        }
    }

    /// Encode one sample.
    ///
    /// The integer formats clip rather than wrap: a float buffer can carry
    /// more than full scale - a strip at +6 dB does - and wrapping would
    /// turn a loud passage into noise. Float keeps what it was given,
    /// which is the reason to choose it.
    /// Encode one sample.
    ///
    /// The integer formats clip rather than wrap: a float buffer can carry
    /// more than full scale - a strip at +6 dB does - and wrapping would
    /// turn a loud passage into noise. Float keeps what it was given,
    /// which is the reason to choose it.
    fn encode(self, sample: f32, big_endian: bool, into: &mut Vec<u8>) {
        if big_endian {
            match self {
                Self::Bits16 => into.extend_from_slice(&to_i16(sample).to_be_bytes()),
                Self::Bits24 => into.extend_from_slice(&to_i24_be(sample)),
                Self::Float32 => into.extend_from_slice(&sample.to_be_bytes()),
            }
        } else {
            match self {
                Self::Bits16 => into.extend_from_slice(&to_i16(sample).to_le_bytes()),
                Self::Bits24 => into.extend_from_slice(&to_i24(sample)),
                Self::Float32 => into.extend_from_slice(&sample.to_le_bytes()),
            }
        }
    }
}

/// A file being written to.
#[derive(Debug)]
pub struct Writer {
    file: File,
    channels: u16,
    depth: Depth,
    container: Container,
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
    pub fn create(
        path: &Path,
        rate: u32,
        channels: u16,
        depth: Depth,
        container: Container,
    ) -> io::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut file = File::create(path)?;
        write_header(&mut file, rate, channels, depth, container)?;
        Ok(Self {
            file,
            channels,
            depth,
            container,
            frames: 0,
            rate,
        })
    }

    /// Take the rate and channel count the graph actually negotiated.
    ///
    /// The header is written when the file is opened, from what the recorder
    /// page asked for - but the stream only pins the sample *format* and
    /// leaves rate and layout to negotiation, so what arrives need not match
    /// what was asked. A file labelled 48 kHz stereo holding 44.1 kHz mono
    /// plays at the wrong speed, and nothing about it says why.
    ///
    /// Only meaningful before any samples are written, which is where it is
    /// called; once frames exist the header describes them and rewriting it
    /// would be the corruption it is meant to prevent.
    ///
    /// # Errors
    ///
    /// Fails if the header cannot be rewritten.
    pub fn adopt_format(&mut self, rate: u32, channels: u16) -> io::Result<()> {
        if self.frames > 0 || rate == 0 || channels == 0 {
            return Ok(());
        }
        if rate == self.rate && channels == self.channels {
            return Ok(());
        }
        log::info!(
            "the graph gave the recorder {rate} Hz {channels}ch, not {} Hz {}ch; \
             writing the header it actually holds",
            self.rate,
            self.channels
        );
        self.rate = rate;
        self.channels = channels;
        self.file.seek(SeekFrom::Start(0))?;
        write_header(&mut self.file, rate, channels, self.depth, self.container)?;
        self.file.seek(SeekFrom::End(0))?;
        Ok(())
    }

    /// Append interleaved samples in whatever format this file holds.
    ///
    /// # Errors
    ///
    /// Fails if the write fails.
    pub fn write(&mut self, samples: &[f32]) -> io::Result<()> {
        let mut bytes = Vec::with_capacity(samples.len() * self.depth.bytes() as usize);
        let be = self.container.big_endian();
        for sample in samples {
            self.depth.encode(*sample, be, &mut bytes);
        }
        self.file.write_all(&bytes)?;
        self.frames += samples.len() as u64 / u64::from(self.channels.max(1));
        self.patch_lengths()
    }

    /// Bring the header's lengths up to date with what has been written.
    ///
    /// `RF64` keeps its real lengths in the `ds64` chunk and leaves the
    /// 32-bit ones reading -1 forever, which is the whole point of it: a
    /// take longer than four gigabytes cannot say its own size in 32 bits.
    fn patch_lengths(&mut self) -> io::Result<()> {
        let header = u64::from(self.container.header_len());
        let data = self.frames * u64::from(self.channels) * u64::from(self.depth.bytes());
        let end = SeekFrom::Start(header + data);

        if self.container == Container::Rf64 {
            // riffSize, dataSize and sampleCount, in that order.
            self.file.seek(SeekFrom::Start(20))?;
            self.file.write_all(&(header + data - 8).to_le_bytes())?;
            self.file.write_all(&data.to_le_bytes())?;
            self.file.write_all(&self.frames.to_le_bytes())?;
            self.file.seek(end)?;
            return Ok(());
        }

        if self.container == Container::Aiff {
            let data_u32 = u32::try_from(data).unwrap_or(u32::MAX);
            let frames_u32 = u32::try_from(self.frames).unwrap_or(u32::MAX);
            // FORM size (total file size minus 8: 4 bytes FORM id + 4 bytes size field)
            self.file.seek(SeekFrom::Start(AIFF_FORM_SIZE_AT))?;
            self.file
                .write_all(&(data_u32 + AIFF_HEADER_LEN - 8).to_be_bytes())?;
            // Number of sample frames in COMM chunk
            self.file.seek(SeekFrom::Start(AIFF_FRAMES_AT))?;
            self.file.write_all(&frames_u32.to_be_bytes())?;
            // SSND chunk size (data length + 8 bytes offset & blockSize fields)
            self.file.seek(SeekFrom::Start(AIFF_SSND_SIZE_AT))?;
            self.file.write_all(&(data_u32 + 8).to_be_bytes())?;
            self.file.seek(end)?;
            return Ok(());
        }

        let data = u32::try_from(data).unwrap_or(u32::MAX);
        let header = self.container.header_len();
        self.file.seek(SeekFrom::Start(RIFF_SIZE_AT))?;
        self.file.write_all(&(data + header - 8).to_le_bytes())?;
        self.file
            .seek(SeekFrom::Start(self.container.data_size_at()))?;
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

fn write_header(
    file: &mut File,
    rate: u32,
    channels: u16,
    depth: Depth,
    container: Container,
) -> io::Result<()> {
    if container == Container::Aiff {
        // AIFF Header: FORM (4 + 4 + 4) + COMM (4 + 4 + 18) + SSND (4 + 4 + 8) = 54 bytes
        file.write_all(b"FORM")?;
        file.write_all(&0u32.to_be_bytes())?; // patched in patch_lengths
        file.write_all(b"AIFF")?;

        // COMM chunk: size 18 (channels: 2, numSampleFrames: 4, sampleSize: 2, sampleRate: 10)
        file.write_all(b"COMM")?;
        file.write_all(&18u32.to_be_bytes())?;
        file.write_all(&channels.to_be_bytes())?;
        file.write_all(&0u32.to_be_bytes())?; // numSampleFrames (patched later)
        file.write_all(&depth.bits().to_be_bytes())?;
        file.write_all(&ieee754_extended_80(rate))?;

        // SSND chunk: size = data + 8 (patched in patch_lengths), offset = 0, blockSize = 0
        file.write_all(b"SSND")?;
        file.write_all(&8u32.to_be_bytes())?;
        file.write_all(&0u32.to_be_bytes())?; // offset
        file.write_all(&0u32.to_be_bytes())?; // blockSize
        return Ok(());
    }

    let bytes_per_frame = u32::from(channels) * depth.bytes();
    match container {
        Container::Wav | Container::Bwf => {
            file.write_all(b"RIFF")?;
            file.write_all(&0u32.to_le_bytes())?;
        }
        Container::Rf64 => {
            // The 32-bit sizes read -1 and the real ones live in `ds64`.
            file.write_all(b"RF64")?;
            file.write_all(&u32::MAX.to_le_bytes())?;
        }
        Container::Aiff => unreachable!(),
    }
    file.write_all(b"WAVE")?;
    match container {
        Container::Wav => {}
        Container::Bwf => {
            file.write_all(b"bext")?;
            file.write_all(&BEXT_LEN.to_le_bytes())?;
            file.write_all(&vec![0u8; BEXT_LEN as usize])?;
        }
        Container::Rf64 => {
            file.write_all(b"ds64")?;
            file.write_all(&DS64_LEN.to_le_bytes())?;
            // riffSize, dataSize, sampleCount, then an empty chunk table.
            file.write_all(&0u64.to_le_bytes())?;
            file.write_all(&0u64.to_le_bytes())?;
            file.write_all(&0u64.to_le_bytes())?;
            file.write_all(&0u32.to_le_bytes())?;
        }
        Container::Aiff => unreachable!(),
    }
    file.write_all(b"fmt ")?;
    file.write_all(&16u32.to_le_bytes())?;
    file.write_all(&depth.format_tag().to_le_bytes())?;
    file.write_all(&channels.to_le_bytes())?;
    file.write_all(&rate.to_le_bytes())?;
    file.write_all(&(rate * bytes_per_frame).to_le_bytes())?;
    file.write_all(&u16::try_from(bytes_per_frame).unwrap_or(4).to_le_bytes())?;
    file.write_all(&depth.bits().to_le_bytes())?;
    file.write_all(b"data")?;
    match container {
        Container::Wav | Container::Bwf => file.write_all(&0u32.to_le_bytes()),
        Container::Rf64 => file.write_all(&u32::MAX.to_le_bytes()),
        Container::Aiff => unreachable!(),
    }
}

/// Convert sample rate `u32` to IEEE 754 80-bit extended precision float (Big-Endian).
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn ieee754_extended_80(rate: u32) -> [u8; 10] {
    if rate == 0 {
        return [0u8; 10];
    }
    let mut f = f64::from(rate);
    let mut exp: i32 = 0;
    while f >= 2.0 {
        f /= 2.0;
        exp += 1;
    }
    while f < 1.0 && f > 0.0 {
        f *= 2.0;
        exp -= 1;
    }
    let exponent = u16::try_from(exp + 16383).unwrap_or(0).to_be_bytes();
    let mantissa = (f * 9_223_372_036_854_775_808.0) as u64;
    let mantissa_bytes = mantissa.to_be_bytes();
    [
        exponent[0],
        exponent[1],
        mantissa_bytes[0],
        mantissa_bytes[1],
        mantissa_bytes[2],
        mantissa_bytes[3],
        mantissa_bytes[4],
        mantissa_bytes[5],
        mantissa_bytes[6],
        mantissa_bytes[7],
    ]
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

/// The same, three bytes wide and big-endian.
fn to_i24_be(sample: f32) -> [u8; 3] {
    let clamped = sample.clamp(-1.0, 1.0);
    let value = (clamped * 8_388_607.0) as i32;
    let [_, b, c, d] = value.to_be_bytes();
    [b, c, d]
}

#[cfg(test)]
mod tests {
    use super::{Container, Depth, HEADER_LEN, Writer, to_i16, to_i24};

    /// The header is written when the file is opened, from what was asked
    /// for; the stream then negotiates its own rate and layout. A file that
    /// says 48 kHz stereo while holding 44.1 kHz mono plays at the wrong
    /// speed, and the recorder used to parse the negotiated format and throw
    /// it away.
    #[test]
    fn the_header_learns_what_the_graph_actually_gave() {
        let dir = std::env::temp_dir().join("pipemeter-wav-adopt");
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("take.wav");

        let mut writer =
            Writer::create(&path, 48_000, 2, Depth::Bits16, Container::Wav).expect("writer opens");
        writer.adopt_format(44_100, 1).expect("header rewrites");
        writer.write(&[0.0; 64]).expect("samples land");
        writer.finish().expect("header patched");

        let mut reader = std::fs::File::open(&path).expect("reopen");
        let mut header = [0u8; HEADER_LEN as usize];
        std::io::Read::read_exact(&mut reader, &mut header).expect("header reads");
        let channels = u16::from_le_bytes([header[22], header[23]]);
        let rate = u32::from_le_bytes([header[24], header[25], header[26], header[27]]);
        assert_eq!((rate, channels), (44_100, 1));

        let _ = std::fs::remove_file(&path);
    }

    /// Once samples exist the header describes them, so rewriting it would
    /// be the corruption it is meant to prevent.
    #[test]
    fn a_started_file_keeps_the_header_it_has() {
        let dir = std::env::temp_dir().join("pipemeter-wav-adopt");
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("started.wav");

        let mut writer =
            Writer::create(&path, 48_000, 2, Depth::Bits16, Container::Wav).expect("writer opens");
        writer.write(&[0.0; 64]).expect("samples land");
        writer.adopt_format(44_100, 1).expect("no-op");
        writer.finish().expect("header patched");

        let mut reader = std::fs::File::open(&path).expect("reopen");
        let mut header = [0u8; HEADER_LEN as usize];
        std::io::Read::read_exact(&mut reader, &mut header).expect("header reads");
        let rate = u32::from_le_bytes([header[24], header[25], header[26], header[27]]);
        assert_eq!(rate, 48_000);

        let _ = std::fs::remove_file(&path);
    }

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
            let mut writer =
                Writer::create(&path, 48_000, 2, depth, Container::Wav).expect("creates");
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
        let mut writer =
            Writer::create(&path, 48_000, 1, Depth::Float32, Container::Wav).expect("creates");
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

    /// Each container puts its own chunks before the samples, and the
    /// data has to start exactly where the header says it stops.
    #[test]
    fn every_container_writes_the_header_it_promises() {
        for (container, magic) in [
            (Container::Wav, b"RIFF"),
            (Container::Bwf, b"RIFF"),
            (Container::Rf64, b"RF64"),
        ] {
            let path =
                // Named for the test as well as the container: two tests
                // sharing a path race each other when they run in
                // parallel, which looked exactly like a wrong header.
                std::env::temp_dir().join(format!("pipemeter-header-{}.wav", container.as_str()));
            let mut writer =
                Writer::create(&path, 48_000, 2, Depth::Bits16, container).expect("creates");
            writer.write(&[0.5, -0.5]).expect("writes");
            writer.finish().expect("finishes");

            let b = std::fs::read(&path).expect("reads back");
            assert_eq!(&b[0..4], magic, "{container:?} magic");
            assert_eq!(&b[8..12], b"WAVE", "{container:?} form");
            // One frame of two 16-bit samples after the header.
            assert_eq!(
                b.len() as u32,
                container.header_len() + 4,
                "{container:?} header length"
            );
            let _ = std::fs::remove_file(&path);
        }
    }

    /// BWF is a WAV with a description chunk in front of the format, and
    /// a program that does not know it should skip it and still play.
    #[test]
    fn broadcast_wave_carries_its_description_chunk() {
        let path = std::env::temp_dir().join("pipemeter-bext.wav");
        let mut writer =
            Writer::create(&path, 48_000, 2, Depth::Bits16, Container::Bwf).expect("creates");
        writer.write(&[0.0; 8]).expect("writes");
        writer.finish().expect("finishes");

        let b = std::fs::read(&path).expect("reads back");
        assert_eq!(&b[12..16], b"bext");
        assert_eq!(u32::from_le_bytes(b[16..20].try_into().unwrap()), 602);
        assert_eq!(&b[622..626], b"fmt ", "the format follows the description");
        let _ = std::fs::remove_file(&path);
    }

    /// RF64 exists because a WAV cannot say it is bigger than 4 GB. The
    /// 32-bit lengths stay at -1 and the real ones live in `ds64`.
    #[test]
    fn rf64_keeps_its_lengths_in_sixty_four_bits() {
        let path = std::env::temp_dir().join("pipemeter-rf64.wav");
        let mut writer =
            Writer::create(&path, 48_000, 2, Depth::Bits16, Container::Rf64).expect("creates");
        writer.write(&[0.25; 16]).expect("writes");
        writer.finish().expect("finishes");

        let b = std::fs::read(&path).expect("reads back");
        assert_eq!(&b[12..16], b"ds64");
        assert_eq!(
            u32::from_le_bytes(b[4..8].try_into().unwrap()),
            u32::MAX,
            "the 32-bit RIFF size must stay -1"
        );
        let data = u64::from_le_bytes(b[28..36].try_into().unwrap());
        let frames = u64::from_le_bytes(b[36..44].try_into().unwrap());
        assert_eq!(data, 32, "16 samples of 2 bytes");
        assert_eq!(frames, 8, "two channels");
        assert_eq!(
            b.len() as u64,
            u64::from(Container::Rf64.header_len()) + data
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn container_spellings_round_trip() {
        for container in [
            Container::Wav,
            Container::Aiff,
            Container::Bwf,
            Container::Rf64,
        ] {
            assert_eq!(Container::parse(container.as_str()), container);
        }
        assert_eq!(Container::parse("nonsense"), Container::Wav);
    }

    #[test]
    fn cycling_the_container_comes_back_round() {
        let mut at = Container::Wav;
        for _ in 0..4 {
            at = at.next();
        }
        assert_eq!(at, Container::Wav);
    }

    #[test]
    fn aiff_writes_form_comm_and_ssnd_chunks() {
        let path = std::env::temp_dir().join("pipemeter-aiff-test.aiff");
        let mut writer =
            Writer::create(&path, 48_000, 2, Depth::Bits16, Container::Aiff).expect("creates");
        writer.write(&[0.5, -0.5]).expect("writes");
        writer.finish().expect("finishes");

        let b = std::fs::read(&path).expect("reads back");
        assert_eq!(&b[0..4], b"FORM");
        assert_eq!(&b[8..12], b"AIFF");
        assert_eq!(&b[12..16], b"COMM");
        let comm_len = u32::from_be_bytes(b[16..20].try_into().unwrap());
        assert_eq!(comm_len, 18);
        let channels = u16::from_be_bytes(b[20..22].try_into().unwrap());
        assert_eq!(channels, 2);
        let frames = u32::from_be_bytes(b[22..26].try_into().unwrap());
        assert_eq!(frames, 1);
        let sample_size = u16::from_be_bytes(b[26..28].try_into().unwrap());
        assert_eq!(sample_size, 16);
        assert_eq!(&b[38..42], b"SSND");
        let ssnd_len = u32::from_be_bytes(b[42..46].try_into().unwrap());
        assert_eq!(ssnd_len, 12); // 4 bytes audio + 8 bytes offset/blockSize
        assert_eq!(b.len() as u32, super::AIFF_HEADER_LEN + 4);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_finished_file_declares_its_own_length() {
        let path = std::env::temp_dir().join("pipemeter-wav-test.wav");
        let mut writer =
            Writer::create(&path, 48_000, 2, Depth::Bits16, Container::Wav).expect("creates");
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
        let mut writer =
            Writer::create(&path, 48_000, 2, Depth::Bits16, Container::Wav).expect("creates");
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
        let mut writer =
            Writer::create(&path, 48_000, 2, Depth::Bits16, Container::Wav).expect("creates");
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
        let mut writer =
            Writer::create(&path, 8, 2, Depth::Bits16, Container::Wav).expect("creates");
        writer.write(&[0.0; 32]).expect("writes");
        assert_eq!(writer.duration().as_secs(), 2);
        writer.finish().expect("finishes");
        let _ = std::fs::remove_file(&path);
    }
}
