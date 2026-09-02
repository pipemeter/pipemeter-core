//! Recording a bus to a file.
//!
//! One capture stream, the same shape as a meter, but keeping the samples
//! instead of measuring them.
//!
//! The process callback runs on `PipeWire`'s realtime thread, where writing
//! to a file is not allowed: a disk that stalls would stall the audio graph
//! and every other application in it. So the callback only appends to a
//! buffer, and a plain thread drains it to disk.
//!
//! The drain swaps the whole buffer out under the lock and writes outside it,
//! so the realtime side never waits on a write — only on a pointer swap. A
//! lock-free ring would be better still and is what this should become if it
//! ever misbehaves under load; a mutex held for a `mem::take` is a long way
//! short of a disk write, and is honest about being the simpler thing.

use std::mem;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use pipewire::core::CoreRc;
use pipewire::spa;
use pipewire::spa::param::format::{MediaSubtype, MediaType};
use pipewire::spa::param::format_utils;
use pipewire::spa::pod::Pod;
use pipewire::stream::{StreamFlags, StreamListener, StreamRc};

use super::wav;

/// Samples waiting to be written, and whether the writer should stop.
#[derive(Debug, Default)]
struct Shared {
    pending: Mutex<Vec<f32>>,
    stopping: AtomicBool,
    /// Frames written, so the deck can show elapsed time without reaching
    /// into the writer thread.
    frames: AtomicU64,
    /// The rate and channel count the graph settled on, once it has.
    ///
    /// Read by the writer thread before its first write. The stream pins
    /// only the sample format and leaves these to negotiation - which is
    /// deliberate, an earlier version pinned them and got a file of zeros -
    /// so the header has to learn what was actually agreed.
    negotiated: Mutex<Option<(u32, u16)>>,
}

/// State the process callback needs.
struct UserData {
    format: spa::param::audio::AudioInfoRaw,
    shared: Arc<Shared>,
}

/// A recording in progress. Dropping it stops and finalises the file.
pub struct Recorder {
    shared: Arc<Shared>,
    writer: Option<std::thread::JoinHandle<()>>,
    path: PathBuf,
    _stream: StreamRc,
    _listener: StreamListener<UserData>,
}

impl std::fmt::Debug for Recorder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Recorder")
            .field("shared", &self.shared)
            .field("writer", &self.writer.is_some())
            .field("path", &self.path)
            .field("_stream", &"<pipewire>")
            .field("_listener", &"<pipewire>")
            .finish()
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.shared.stopping.store(true, Ordering::Relaxed);
        if let Some(handle) = self.writer.take() {
            let _ = handle.join();
        }
    }
}

/// Start recording `node` to `path`.
///
/// `is_sink` says which way the node faces: a sink is read through its
/// monitor, a source directly. It is not a detail that can be guessed and
/// left to the session manager to sort out - see below.
///
/// Returns `None` if the stream or the file could not be opened, which is
/// not fatal: the deck reports that nothing is recording.
pub fn start(
    core: &CoreRc,
    node: &str,
    path: &Path,
    rate: u32,
    depth: wav::Depth,
    container: wav::Container,
    is_sink: bool,
) -> Option<Recorder> {
    const CHANNELS: u16 = 2;

    let writer = wav::Writer::create(path, rate, CHANNELS, depth, container)
        .map_err(|err| log::warn!("could not record to {}: {err}", path.display()))
        .ok()?;

    let shared = Arc::new(Shared::default());
    // `stream.capture.sink` used to be set unconditionally, which asked to
    // record the monitor of a node that is often not a sink at all: a B bus
    // is declared `Audio/Source/Virtual`. The session manager cannot honour
    // that, and rather than refusing it quietly substituted the default
    // sink's monitor - so arming B1 recorded the VAIO strip instead, at the
    // right length and with no hint anything was wrong.
    //
    // So it is set only when the node really is a sink, and the stream is
    // told not to reconnect: if the target cannot be had, the take should
    // come out empty and obviously broken rather than plausibly wrong.
    let mut props = pipewire::properties::properties! {
        *pipewire::keys::MEDIA_TYPE => "Audio",
        *pipewire::keys::MEDIA_CATEGORY => "Capture",
        *pipewire::keys::MEDIA_ROLE => "Production",
        *pipewire::keys::NODE_DONT_RECONNECT => "true",
        *pipewire::keys::TARGET_OBJECT => node,
        *pipewire::keys::NODE_NAME => "pipemeter_recorder",
    };
    if is_sink {
        props.insert(*pipewire::keys::STREAM_CAPTURE_SINK, "true");
    }

    let stream = StreamRc::new(core.clone(), "pipemeter-recorder", props).ok()?;
    let data = UserData {
        format: spa::param::audio::AudioInfoRaw::default(),
        shared: Arc::clone(&shared),
    };

    let listener = stream
        .add_local_listener_with_user_data(data)
        .param_changed(|_, user_data, id, param| {
            let Some(param) = param else { return };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            let Ok((media_type, media_subtype)) = format_utils::parse_format(param) else {
                return;
            };
            if media_type != MediaType::Audio || media_subtype != MediaSubtype::Raw {
                return;
            }
            let _ = user_data.format.parse(param);
            // The meters read this off the same callback to interpret their
            // buffers; the recorder used to parse it and drop it on the
            // floor, which is how a file could claim a rate it did not hold.
            if let Ok(mut negotiated) = user_data.shared.negotiated.lock() {
                *negotiated = Some((
                    user_data.format.rate(),
                    u16::try_from(user_data.format.channels()).unwrap_or(2),
                ));
            }
        })
        .process(|stream, user_data| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            let Some(data) = datas.first_mut() else {
                return;
            };
            let len = data.chunk().size() as usize / mem::size_of::<f32>();
            let Some(bytes) = data.data() else { return };

            if let Ok(mut pending) = user_data.shared.pending.lock() {
                pending.reserve(len);
                for chunk in bytes.chunks_exact(mem::size_of::<f32>()).take(len) {
                    let sample = f32::from_le_bytes(chunk.try_into().unwrap_or([0; 4]));
                    pending.push(sample);
                }
            }
        })
        .register()
        .ok()?;

    let writer_thread = spawn_writer(Arc::clone(&shared), writer)?;

    connect(&stream)?;

    log::info!("recording to {}", path.display());
    Some(Recorder {
        shared,
        writer: Some(writer_thread),
        path: path.to_owned(),
        _stream: stream,
        _listener: listener,
    })
}

/// Join the stream to the graph.
///
/// Only the sample format is asked for. Pinning the rate and the channel
/// count as well produced a stream that connected to the right node,
/// negotiated, ran, and delivered nothing but zeros - which is what the
/// meters avoid by asking for the format alone and letting the graph say
/// the rest.
/// Drain captured samples to disk on a thread of their own.
///
/// The `PipeWire` callback must not block, so it only appends to a buffer;
/// everything that touches the file happens here. Split from `start` to keep
/// it inside the line limit.
fn spawn_writer(
    drain: Arc<Shared>,
    mut writer: wav::Writer,
) -> Option<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("pipemeter-recorder".to_owned())
        .spawn(move || {
            loop {
                let batch = {
                    let Ok(mut pending) = drain.pending.lock() else {
                        break;
                    };
                    mem::take(&mut *pending)
                };
                if batch.is_empty() {
                    if drain.stopping.load(Ordering::Relaxed) {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    continue;
                }
                // Before the first samples land, so the header describes
                // what the file is about to hold rather than what was asked
                // for.
                if let Ok(negotiated) = drain.negotiated.lock()
                    && let Some((rate, channels)) = *negotiated
                    && let Err(err) = writer.adopt_format(rate, channels)
                {
                    log::warn!("could not correct the recording's header: {err}");
                }
                if writer.write(&batch).is_err() {
                    break;
                }
                let frames = writer.frames();
                drain.frames.store(frames, Ordering::Relaxed);
            }
            if let Err(err) = writer.finish() {
                log::warn!("could not finish the recording: {err}");
            }
        })
        .ok()
}

fn connect(stream: &StreamRc) -> Option<()> {
    let mut audio_info = spa::param::audio::AudioInfoRaw::new();
    audio_info.set_format(spa::param::audio::AudioFormat::F32LE);
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
            spa::utils::Direction::Input,
            None,
            StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
            &mut params,
        )
        .ok()?;
    Some(())
}
