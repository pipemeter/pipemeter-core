//! Pure-Rust audio file player for the tape deck.
//!
//! Decodes audio files using `symphonia` (WAV, MP3, FLAC, OGG Vorbis, AAC, M4A, ALAC)
//! and feeds raw 32-bit float stereo PCM samples into a `PipeWire` playback stream
//! targeted at a specified mixer bus node (`pipemeter_bus_a1`..`a5`, `pipemeter_bus_b1`..`b3`).

use std::collections::VecDeque;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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

/// Shared playback buffer and control state.
#[derive(Debug, Default)]
struct Shared {
    /// Interleaved 32-bit float stereo samples waiting to be sent to `PipeWire`.
    pcm_queue: Mutex<VecDeque<f32>>,
    /// Current playback frame counter.
    position_frames: AtomicU64,
    /// Total frames in the audio stream, if known.
    total_frames: AtomicU64,
    /// Nominal sample rate of the audio file.
    sample_rate: AtomicU64,
    /// Play/Pause state.
    playing: AtomicBool,
    /// Termination signal.
    stopping: AtomicBool,
    /// Reached end of stream.
    ended: AtomicBool,
    /// Seek request in seconds, if requested.
    seek_request: Mutex<Option<f64>>,
}

/// State for the `PipeWire` realtime process callback.
struct UserData {
    shared: Arc<Shared>,
}

/// Active in-process player instance. Dropping stops playback and frees `PipeWire` streams.
pub struct Player {
    shared: Arc<Shared>,
    decoder_thread: Option<std::thread::JoinHandle<()>>,
    path: PathBuf,
    _streams: Vec<StreamRc>,
    _listeners: Vec<StreamListener<UserData>>,
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

    /// Loaded file path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Start playing `path` into one or more `target_nodes`.
pub fn start(core: &CoreRc, target_nodes: &[String], path: &Path) -> Option<Player> {
    if target_nodes.is_empty() {
        return None;
    }
    let file = File::open(path)
        .map_err(|err| log::warn!("could not open audio file {}: {err}", path.display()))
        .ok()?;

    let shared = Arc::new(Shared::default());
    let (tx_ready, rx_ready) = std::sync::mpsc::channel();

    let decode_shared = Arc::clone(&shared);
    let decode_path = path.to_owned();

    let decoder_thread = std::thread::Builder::new()
        .name("pipemeter-player".to_owned())
        .spawn(move || {
            run_decoder(file, &decode_path, &decode_shared, &tx_ready);
        })
        .ok()?;

    let (rate, channels) = rx_ready
        .recv_timeout(Duration::from_secs(2))
        .map_err(|err| log::warn!("player decoder init timed out: {err}"))
        .ok()?;

    let mut streams = Vec::new();
    let mut listeners = Vec::new();

    for target in target_nodes {
        let props = pipewire::properties::properties! {
            *pipewire::keys::MEDIA_TYPE => "Audio",
            *pipewire::keys::MEDIA_CATEGORY => "Playback",
            *pipewire::keys::MEDIA_ROLE => "Music",
            *pipewire::keys::TARGET_OBJECT => target.as_str(),
            *pipewire::keys::NODE_NAME => "pipemeter_deck_player",
        };

        let Ok(stream) = StreamRc::new(core.clone(), "pipemeter-deck-player", props) else {
            continue;
        };
        let data = UserData {
            shared: Arc::clone(&shared),
        };

        let Ok(listener) = stream
            .add_local_listener_with_user_data(data)
            .process(|stream, user_data| {
                process_audio_stream(stream, user_data);
            })
            .register()
        else {
            continue;
        };

        if connect_playback(&stream, rate, channels).is_some() {
            log::info!(
                "playing {} ({rate} Hz, {channels} ch) into {target}",
                path.display()
            );
            streams.push(stream);
            listeners.push(listener);
        }
    }

    if streams.is_empty() {
        return None;
    }

    Some(Player {
        shared,
        decoder_thread: Some(decoder_thread),
        path: path.to_owned(),
        _streams: streams,
        _listeners: listeners,
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

    if !user_data.shared.playing.load(Ordering::Relaxed) {
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

        if let Ok(mut q) = user_data.shared.pcm_queue.lock() {
            while out_idx + 1 < sample_capacity && !q.is_empty() {
                let left = q.pop_front().unwrap_or(0.0);
                let right = q.pop_front().unwrap_or(0.0);
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
    user_data
        .shared
        .position_frames
        .fetch_add(frames_written, Ordering::Relaxed);

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

fn run_decoder(
    file: File,
    path: &Path,
    shared: &Arc<Shared>,
    tx_ready: &std::sync::mpsc::Sender<(u32, u16)>,
) {
    let mss = MediaSourceStream::new(Box::new(file), MediaSourceStreamOptions::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
        hint.with_extension(ext);
    }

    let probed = match symphonia::default::get_probe().format(
        &hint,
        mss,
        &FormatOptions::default(),
        &MetadataOptions::default(),
    ) {
        Ok(p) => p,
        Err(err) => {
            log::warn!("failed to probe audio format {}: {err}", path.display());
            return;
        }
    };

    let mut format = probed.format;
    let Some(track) = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
    else {
        log::warn!("no valid audio track found in {}", path.display());
        return;
    };

    let track_id = track.id;
    let rate = track.codec_params.sample_rate.unwrap_or(48000);
    let channels = track.codec_params.channels.map_or(2, |c| c.count() as u16);
    let n_frames = track.codec_params.n_frames.unwrap_or(0);

    shared.sample_rate.store(u64::from(rate), Ordering::Relaxed);
    shared.total_frames.store(n_frames, Ordering::Relaxed);

    let mut decoder = match symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
    {
        Ok(d) => d,
        Err(err) => {
            log::warn!("failed to create decoder for {}: {err}", path.display());
            return;
        }
    };

    let _ = tx_ready.send((rate, channels.max(1)));

    while !shared.stopping.load(Ordering::Relaxed) {
        handle_seek(&mut format, track_id, shared);

        let queue_len = shared.pcm_queue.lock().map_or(0, |q| q.len());
        if queue_len > (rate as usize * 2) {
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
            if let Ok(mut q) = shared.pcm_queue.lock() {
                q.clear();
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

    if let Ok(mut q) = shared.pcm_queue.lock() {
        if stereo_buf.is_empty() {
            for sample in mono_buf {
                q.push_back(sample);
                q.push_back(sample);
            }
        } else {
            q.extend(stereo_buf);
        }
    }
}
