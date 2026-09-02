//! Pure-Rust audio file player for the tape deck.
//!
//! Decodes audio files using `symphonia` (WAV, MP3, FLAC, OGG Vorbis, AAC, M4A, ALAC)
//! and feeds raw 32-bit float stereo PCM samples into a `PipeWire` playback stream
//! targeted at a specified mixer bus node (`pipemeter_bus_a1`..`a5`, `pipemeter_bus_b1`..`b3`).

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pipewire::core::CoreRc;
use pipewire::spa;
use pipewire::spa::pod::Pod;
use pipewire::stream::{StreamFlags, StreamListener, StreamRc};
use symphonia::core::audio::{AudioBufferRef, Signal};
use symphonia::core::codecs::{CODEC_TYPE_NULL, DecoderOptions};
use symphonia::core::errors::Error as SymphError;
use symphonia::core::formats::{FormatOptions, SeekMode, SeekTo};
use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use symphonia::core::units::Time;

/// Status and metadata of the player.
#[derive(Debug, Clone, Copy, Default)]
pub struct Status {
    pub position_frames: u64,
    pub total_frames: Option<u64>,
    pub sample_rate: u32,
    pub channels: u16,
    pub is_playing: bool,
    pub is_ended: bool,
}

impl Status {
    #[must_use]
    pub fn position_seconds(self) -> f64 {
        if self.sample_rate == 0 {
            0.0
        } else {
            self.position_frames as f64 / f64::from(self.sample_rate)
        }
    }

    #[must_use]
    pub fn duration_seconds(self) -> Option<f64> {
        let frames = self.total_frames?;
        if self.sample_rate == 0 {
            None
        } else {
            Some(frames as f64 / f64::from(self.sample_rate))
        }
    }
}

/// Read-only handle to a player's live atomics, usable from any thread.
///
/// Cloned out of [`Player::status_handle`] and kept by the [`Backend`] so the
/// UI can poll position and duration each frame without crossing the
/// `PipeWire` thread boundary.
#[derive(Debug, Clone)]
pub struct StatusHandle {
    position_frames: Arc<AtomicU64>,
    total_frames: Arc<AtomicU64>,
    sample_rate: Arc<AtomicU64>,
    playing: Arc<AtomicBool>,
    ended: Arc<AtomicBool>,
}

impl StatusHandle {
    /// Read an instantaneous snapshot. All loads are `Relaxed`; this is only
    /// for UI display, not for synchronisation.
    #[must_use]
    pub fn snapshot(&self) -> Status {
        let total = self.total_frames.load(Ordering::Relaxed);
        Status {
            position_frames: self.position_frames.load(Ordering::Relaxed),
            total_frames: if total == 0 { None } else { Some(total) },
            sample_rate: self.sample_rate.load(Ordering::Relaxed) as u32,
            channels: 2,
            is_playing: self.playing.load(Ordering::Relaxed),
            is_ended: self.ended.load(Ordering::Relaxed),
        }
    }
}

/// Shared playback buffer and control state.
#[derive(Debug)]
struct Shared {
    /// Per-target stereo PCM queues to broadcast audio frames to all active targets.
    stream_queues: Mutex<HashMap<String, Arc<Mutex<VecDeque<f32>>>>>,
    /// Current playback frame counter.
    position_frames: Arc<AtomicU64>,
    /// Total frames in the audio stream, if known.
    total_frames: Arc<AtomicU64>,
    /// Nominal sample rate of the audio file.
    sample_rate: Arc<AtomicU64>,
    /// Linear amplitude gain as IEEE 754 float bits.
    gain: Arc<AtomicU32>,
    /// Play/Pause state.
    playing: Arc<AtomicBool>,
    /// Termination signal.
    stopping: AtomicBool,
    /// Reached end of stream.
    ended: Arc<AtomicBool>,
    /// Seek request in seconds, if requested.
    seek_request: Mutex<Option<f64>>,
    /// Which stream advances `position_frames`.
    ///
    /// Every target renders the same audio, so if each one counted the
    /// frames it wrote the position would run at the number of outputs
    /// times real time - two speakers and the counter moves twice as
    /// fast. Exactly one stream counts, named here rather than fixed at
    /// creation so that turning off whichever output happens to be first
    /// hands the job to another instead of stopping the clock.
    primary_stream: Arc<AtomicU64>,
}

/// State for each `PipeWire` realtime process callback stream.
struct UserData {
    pcm_queue: Arc<Mutex<VecDeque<f32>>>,
    playing: Arc<AtomicBool>,
    position_frames: Arc<AtomicU64>,
    gain: Arc<AtomicU32>,
    /// This stream's identity, compared against `Shared::primary_stream`
    /// to decide whether it is the one advancing the position.
    id: u64,
    primary_stream: Arc<AtomicU64>,
}

struct TargetStream {
    target: String,
    id: u64,
    _stream: StreamRc,
    _listener: StreamListener<UserData>,
}

/// Active in-process player instance. Dropping stops playback and frees `PipeWire` streams.
pub struct Player {
    shared: Arc<Shared>,
    decoder_thread: Option<std::thread::JoinHandle<()>>,
    path: PathBuf,
    sample_rate: u32,
    channels: u16,
    target_streams: Vec<TargetStream>,
    /// Handed out to each new stream so the primary can be named.
    next_stream_id: u64,
}

impl std::fmt::Debug for Player {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Player")
            .field("path", &self.path)
            .field("status", &self.status())
            .finish_non_exhaustive()
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.shared.stopping.store(true, Ordering::Relaxed);
        if let Some(handle) = self.decoder_thread.take() {
            let _ = handle.join();
        }
    }
}

impl Player {
    /// Get current playback status.
    #[must_use]
    pub fn status(&self) -> Status {
        let total = self.shared.total_frames.load(Ordering::Relaxed);
        Status {
            position_frames: self.shared.position_frames.load(Ordering::Relaxed),
            total_frames: if total == 0 { None } else { Some(total) },
            sample_rate: self.shared.sample_rate.load(Ordering::Relaxed) as u32,
            channels: 2,
            is_playing: self.shared.playing.load(Ordering::Relaxed),
            is_ended: self.shared.ended.load(Ordering::Relaxed),
        }
    }

    /// Clone a lightweight handle to the player's live atomics.
    ///
    /// The handle can be kept by any thread and polled cheaply every frame
    /// via [`StatusHandle::snapshot`] — no locks, no thread hops.
    #[must_use]
    pub fn status_handle(&self) -> StatusHandle {
        StatusHandle {
            position_frames: Arc::clone(&self.shared.position_frames),
            total_frames: Arc::clone(&self.shared.total_frames),
            sample_rate: Arc::clone(&self.shared.sample_rate),
            playing: Arc::clone(&self.shared.playing),
            ended: Arc::clone(&self.shared.ended),
        }
    }

    /// Start or resume playback.
    pub fn play(&self) {
        self.shared.playing.store(true, Ordering::Relaxed);
    }

    /// Pause playback.
    pub fn pause(&self) {
        self.shared.playing.store(false, Ordering::Relaxed);
    }

    /// Seek to a time offset in seconds.
    pub fn seek(&self, seconds: f64) {
        if let Ok(mut req) = self.shared.seek_request.lock() {
            *req = Some(seconds.max(0.0));
        }
    }

    /// Update playback gain live in dB.
    pub fn set_gain(&self, gain_db: f32) {
        let linear_gain = if gain_db <= -60.0 {
            0.0
        } else {
            10.0_f32.powf(gain_db / 20.0)
        };
        self.shared
            .gain
            .store(linear_gain.to_bits(), Ordering::Relaxed);
    }

    /// Update playback targets dynamically without interrupting playback or resetting position.
    pub fn set_targets(&mut self, core: &CoreRc, target_nodes: &[String]) {
        self.target_streams
            .retain(|ts| target_nodes.contains(&ts.target));
        if let Ok(mut map) = self.shared.stream_queues.lock() {
            map.retain(|target, _| target_nodes.contains(target));
        }

        for target in target_nodes {
            if self.target_streams.iter().any(|ts| &ts.target == target) {
                continue;
            }

            let q = Arc::new(Mutex::new(VecDeque::new()));
            let id = self.next_stream_id;
            self.next_stream_id += 1;
            let data = UserData {
                pcm_queue: Arc::clone(&q),
                playing: Arc::clone(&self.shared.playing),
                position_frames: Arc::clone(&self.shared.position_frames),
                gain: Arc::clone(&self.shared.gain),
                id,
                primary_stream: Arc::clone(&self.shared.primary_stream),
            };

            if let Some((stream, listener)) =
                create_stream_for_target(core, target, data, self.sample_rate, self.channels)
            {
                log::info!(
                    "adding target {target} to active playback of {}",
                    self.path.display()
                );
                if let Ok(mut map) = self.shared.stream_queues.lock() {
                    map.insert(target.clone(), q);
                }
                self.target_streams.push(TargetStream {
                    target: target.clone(),
                    id,
                    _stream: stream,
                    _listener: listener,
                });
            }
        }

        // Whoever was counting may have just been switched off. Hand the
        // job to a survivor rather than leaving the position frozen with
        // audio still playing.
        let primary = self.shared.primary_stream.load(Ordering::Relaxed);
        if !self.target_streams.iter().any(|ts| ts.id == primary)
            && let Some(first) = self.target_streams.first()
        {
            log::debug!("position tracking moves to {}", first.target);
            self.shared
                .primary_stream
                .store(first.id, Ordering::Relaxed);
        }
    }

    /// Loaded file path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn probe_audio(
    path: &Path,
) -> Option<(
    symphonia::core::probe::ProbeResult,
    symphonia::core::formats::Track,
    u32,
    u16,
    u64,
)> {
    let file = File::open(path)
        .map_err(|err| log::warn!("could not open audio file {}: {err}", path.display()))
        .ok()?;

    let mss = MediaSourceStream::new(Box::new(file), MediaSourceStreamOptions::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
        hint.with_extension(ext);
    }

    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|err| log::warn!("failed to probe audio format {}: {err}", path.display()))
        .ok()?;

    let track = probed
        .format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .cloned()?;

    let rate = track.codec_params.sample_rate.unwrap_or(48000);
    let channels = track.codec_params.channels.map_or(2, |c| c.count() as u16);
    let n_frames = track.codec_params.n_frames.unwrap_or(0);

    Some((probed, track, rate, channels, n_frames))
}

fn create_stream_for_target(
    core: &CoreRc,
    target: &str,
    data: UserData,
    rate: u32,
    channels: u16,
) -> Option<(StreamRc, StreamListener<UserData>)> {
    let mut props = pipewire::properties::properties! {
        *pipewire::keys::MEDIA_TYPE => "Audio",
        *pipewire::keys::MEDIA_CATEGORY => "Playback",
        *pipewire::keys::MEDIA_ROLE => "Music",
        *pipewire::keys::TARGET_OBJECT => target,
        *pipewire::keys::NODE_NAME => "pipemeter_deck_player",
        *pipewire::keys::NODE_DESCRIPTION => format!("PipeMeter Player -> {target}"),
    };

    if target.starts_with("pipemeter_") {
        props.insert("stream.dont-reconnect", "true");
        props.insert("stream.capture.sink", "true");
    }

    let stream = StreamRc::new(core.clone(), "pipemeter-deck-player", props).ok()?;
    let listener = stream
        .add_local_listener_with_user_data(data)
        .process(|stream, user_data| {
            process_audio_stream(stream, user_data);
        })
        .register()
        .ok()?;

    connect_playback(&stream, rate, channels)?;
    Some((stream, listener))
}

/// Start playing `path` into one or more `target_nodes` with `gain_db` attenuation/boost.
pub fn start(core: &CoreRc, target_nodes: &[String], path: &Path, gain_db: f32) -> Option<Player> {
    if target_nodes.is_empty() {
        return None;
    }

    let (probed, track, rate, channels, n_frames) = probe_audio(path)?;

    let mut stream_map = HashMap::new();
    let mut target_streams = Vec::new();

    let playing = Arc::new(AtomicBool::new(false));
    let position_frames = Arc::new(AtomicU64::new(0));

    let linear_gain = if gain_db <= -60.0 {
        0.0
    } else {
        10.0_f32.powf(gain_db / 20.0)
    };
    let gain = Arc::new(AtomicU32::new(linear_gain.to_bits()));
    let primary_stream = Arc::new(AtomicU64::new(0));

    for (id, target) in (0u64..).zip(target_nodes) {
        let q = Arc::new(Mutex::new(VecDeque::new()));
        let data = UserData {
            pcm_queue: Arc::clone(&q),
            playing: Arc::clone(&playing),
            position_frames: Arc::clone(&position_frames),
            gain: Arc::clone(&gain),
            id,
            primary_stream: Arc::clone(&primary_stream),
        };

        if let Some((stream, listener)) =
            create_stream_for_target(core, target, data, rate, channels)
        {
            log::info!(
                "playing {} ({rate} Hz, {channels} ch, gain {gain_db:.1} dB) into {target}",
                path.display()
            );
            stream_map.insert(target.clone(), q);
            target_streams.push(TargetStream {
                target: target.clone(),
                id,
                _stream: stream,
                _listener: listener,
            });
        }
    }

    if target_streams.is_empty() {
        return None;
    }

    let shared = Arc::new(Shared {
        stream_queues: Mutex::new(stream_map),
        position_frames,
        total_frames: Arc::new(AtomicU64::new(n_frames)),
        sample_rate: Arc::new(AtomicU64::new(u64::from(rate))),
        gain,
        playing,
        stopping: AtomicBool::new(false),
        ended: Arc::new(AtomicBool::new(false)),
        seek_request: Mutex::new(None),
        primary_stream,
    });

    let decode_shared = Arc::clone(&shared);
    let decode_path = path.to_owned();

    let decoder_thread = std::thread::Builder::new()
        .name("pipemeter-player".to_owned())
        .spawn(move || {
            run_decoder_probed(probed.format, track.id, rate, &decode_path, &decode_shared);
        })
        .ok()?;

    Some(Player {
        shared,
        decoder_thread: Some(decoder_thread),
        path: path.to_owned(),
        sample_rate: rate,
        channels,
        next_stream_id: target_streams.len() as u64,
        target_streams,
    })
}

fn process_audio_stream(stream: &pipewire::stream::Stream, user_data: &mut UserData) {
    let Some(mut buffer) = stream.dequeue_buffer() else {
        return;
    };
    let datas = buffer.datas_mut();
    let Some(data) = datas.first_mut() else {
        return;
    };

    if !user_data.playing.load(Ordering::Relaxed) {
        if let Some(bytes) = data.data() {
            bytes.fill(0);
            let bytes_len = bytes.len() as u32;
            let chunk = data.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.size_mut() = bytes_len;
            *chunk.stride_mut() = 8;
        }
        return;
    }

    let mut out_idx = 0;
    let frame_capacity;

    if let Some(bytes) = data.data() {
        let sample_capacity = bytes.len() / std::mem::size_of::<f32>();
        frame_capacity = sample_capacity / 2;
        let gain = f32::from_bits(user_data.gain.load(Ordering::Relaxed));

        if let Ok(mut q) = user_data.pcm_queue.lock() {
            while out_idx + 1 < sample_capacity && !q.is_empty() {
                let left = q.pop_front().unwrap_or(0.0) * gain;
                let right = q.pop_front().unwrap_or(0.0) * gain;
                let left_bytes = left.to_le_bytes();
                let right_bytes = right.to_le_bytes();
                let byte_offset = out_idx * 4;
                if byte_offset + 8 <= bytes.len() {
                    bytes[byte_offset..byte_offset + 4].copy_from_slice(&left_bytes);
                    bytes[byte_offset + 4..byte_offset + 8].copy_from_slice(&right_bytes);
                    out_idx += 2;
                }
            }
        }

        if out_idx * 4 < bytes.len() {
            bytes[out_idx * 4..].fill(0);
        }
    } else {
        return;
    }

    let frames_written = (out_idx / 2) as u64;
    // Only the stream currently named primary advances the position;
    // every target renders the same audio, so counting them all would run
    // the clock at the number of outputs times real time.
    if user_data.id == user_data.primary_stream.load(Ordering::Relaxed) {
        user_data
            .position_frames
            .fetch_add(frames_written, Ordering::Relaxed);
    }

    let chunk = data.chunk_mut();
    *chunk.offset_mut() = 0;
    *chunk.size_mut() = (frame_capacity * 8) as u32;
    *chunk.stride_mut() = 8;
}

fn connect_playback(stream: &StreamRc, rate: u32, channels: u16) -> Option<()> {
    let mut audio_info = spa::param::audio::AudioInfoRaw::new();
    audio_info.set_format(spa::param::audio::AudioFormat::F32LE);
    audio_info.set_rate(rate);
    audio_info.set_channels(u32::from(channels.min(2)));

    let obj = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: audio_info.into(),
    };
    let values: Vec<u8> = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(obj),
    )
    .ok()?
    .0
    .into_inner();
    let mut params = [Pod::from_bytes(&values)?];

    stream
        .connect(
            spa::utils::Direction::Output,
            None,
            StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .ok()?;

    Some(())
}

fn run_decoder_probed(
    mut format: Box<dyn symphonia::core::formats::FormatReader>,
    track_id: u32,
    rate: u32,
    path: &Path,
    shared: &Arc<Shared>,
) {
    let Some(track) = format.tracks().iter().find(|t| t.id == track_id) else {
        return;
    };

    let mut decoder = match symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
    {
        Ok(d) => d,
        Err(err) => {
            log::warn!("failed to create decoder for {}: {err}", path.display());
            return;
        }
    };

    while !shared.stopping.load(Ordering::Relaxed) {
        handle_seek(&mut format, track_id, shared);

        let max_q = shared.stream_queues.lock().map_or(0, |map| {
            map.values()
                .map(|q| q.lock().map_or(0, |q| q.len()))
                .max()
                .unwrap_or(0)
        });

        if max_q > (rate as usize * 2) {
            std::thread::sleep(Duration::from_millis(15));
            continue;
        }

        let packet = match format.next_packet() {
            Ok(pkt) => pkt,
            Err(SymphError::IoError(err)) if err.kind() == std::io::ErrorKind::UnexpectedEof => {
                shared.ended.store(true, Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            Err(err) => {
                log::debug!("decode stream finished or error: {err}");
                shared.ended.store(true, Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
        };

        if packet.track_id() == track_id
            && let Ok(decoded) = decoder.decode(&packet)
        {
            append_samples(&decoded, shared);
        }
    }
}

fn handle_seek(
    format: &mut Box<dyn symphonia::core::formats::FormatReader>,
    track_id: u32,
    shared: &Arc<Shared>,
) {
    let seek_target = shared.seek_request.lock().ok().and_then(|mut r| r.take());
    if let Some(target_sec) = seek_target {
        let time = Time::from(target_sec);
        if let Ok(seeked_to) = format.seek(
            SeekMode::Coarse,
            SeekTo::Time {
                time,
                track_id: Some(track_id),
            },
        ) {
            shared
                .position_frames
                .store(seeked_to.actual_ts, Ordering::Relaxed);
            if let Ok(map) = shared.stream_queues.lock() {
                for q in map.values() {
                    if let Ok(mut locked) = q.lock() {
                        locked.clear();
                    }
                }
            }
        }
    }
}

fn append_samples(decoded: &AudioBufferRef<'_>, shared: &Shared) {
    let mut mono_buf = Vec::new();
    let mut stereo_buf = Vec::new();

    if let AudioBufferRef::F32(buf) = decoded {
        let planes = buf.planes();
        let n_frames = buf.frames();
        if planes.planes().len() >= 2 {
            let left = planes.planes()[0];
            let right = planes.planes()[1];
            for i in 0..n_frames {
                stereo_buf.push(left[i]);
                stereo_buf.push(right[i]);
            }
        } else if !planes.planes().is_empty() {
            let mono = planes.planes()[0];
            for &s in mono.iter().take(n_frames) {
                mono_buf.push(s);
            }
        }
    } else {
        let spec = *decoded.spec();
        let mut sample_buf =
            symphonia::core::audio::SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
        sample_buf.copy_interleaved_ref(decoded.clone());
        let samples = sample_buf.samples();
        if spec.channels.count() >= 2 {
            for chunk in samples.chunks_exact(spec.channels.count()) {
                stereo_buf.push(chunk[0]);
                stereo_buf.push(chunk[1]);
            }
        } else {
            for &s in samples {
                mono_buf.push(s);
            }
        }
    }

    if let Ok(map) = shared.stream_queues.lock() {
        for queue in map.values() {
            if let Ok(mut q) = queue.lock() {
                if stereo_buf.is_empty() {
                    for sample in &mono_buf {
                        q.push_back(*sample);
                        q.push_back(*sample);
                    }
                } else {
                    for sample in &stereo_buf {
                        q.push_back(*sample);
                    }
                }
            }
        }
    }
}
