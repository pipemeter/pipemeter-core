//! Level metering.
//!
//! One capture stream per metered node, reading that node's monitor ports.
//! The process callback runs on `PipeWire`'s realtime thread, so it does the
//! least possible work: a peak per channel, written into a shared map. All
//! smoothing and decay happens on the UI side, where being late costs
//! nothing.
//!
//! Attaching a stream per node is not free, so only nodes actually shown on a
//! strip are metered rather than everything in the graph.

use std::collections::HashMap;
use std::mem;
use std::sync::{Arc, Mutex};

use pipewire::core::CoreRc;
use pipewire::spa;
use pipewire::spa::param::format::{MediaSubtype, MediaType};
use pipewire::spa::param::format_utils;
use pipewire::spa::pod::Pod;
use pipewire::stream::{StreamFlags, StreamListener, StreamRc};

/// What to meter: which node, and whether it is one with a monitor.
#[derive(Debug, Clone)]
pub struct Target {
    pub id: u32,
    pub name: String,
    /// Sinks are read through their monitor; sources are read directly.
    pub is_sink: bool,
}

/// Peak level per channel for each metered node, keyed by node id.
///
/// Shared between `PipeWire`'s realtime threads and the UI thread. The lock
/// is only ever held for a couple of float writes.
pub type Levels = Arc<Mutex<HashMap<u32, (f32, f32)>>>;

/// State the process callback needs.
struct UserData {
    format: spa::param::audio::AudioInfoRaw,
    node_id: u32,
    levels: Levels,
}

/// A live meter. Both halves must be kept alive: dropping either stops it.
///
/// `StreamRc` rather than `StreamBox` because the latter borrows the core,
/// which would make this struct self-referential and unstorable.
pub struct Meter {
    _stream: StreamRc,
    _listener: StreamListener<UserData>,
}

impl std::fmt::Debug for Meter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Meter")
    }
}

/// Start metering `node_id`.
///
/// Returns `None` if the stream could not be created or connected, which is
/// not fatal: the strip simply shows no movement.
pub fn attach(core: &CoreRc, node: &Target, levels: &Levels) -> Option<Meter> {
    let mut props = pipewire::properties::properties! {
        *pipewire::keys::MEDIA_TYPE => "Audio",
        *pipewire::keys::MEDIA_CATEGORY => "Capture",
        *pipewire::keys::MEDIA_ROLE => "Music",
        // By name, not by id. An id is a number the session manager is free
        // to ignore when it thinks it knows better, and it did: every meter
        // ended up on the default sink's monitor, so each strip showed
        // whatever the desktop was playing rather than its own signal.
        *pipewire::keys::TARGET_OBJECT => node.name.as_str(),
        // And having said which node, refuse to be moved off it. Without
        // this the session manager reconnects the stream elsewhere the
        // moment the default device changes.
        *pipewire::keys::NODE_DONT_RECONNECT => "true",
        // Keep our own meters out of the strips' application lists.
        *pipewire::keys::NODE_NAME => "pipemeter_meter",
    };
    // Only a sink has a monitor to read. Asking for one on a source makes
    // the request meaningless, which is how it came to be ignored.
    if node.is_sink {
        props.insert(*pipewire::keys::STREAM_CAPTURE_SINK, "true");
    }

    let stream = StreamRc::new(core.clone(), "pipemeter-meter", props).ok()?;
    let data = UserData {
        format: spa::param::audio::AudioInfoRaw::default(),
        node_id: node.id,
        levels: Arc::clone(levels),
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
            // Ignoring the error leaves the previous format in place, which
            // is better than tearing the meter down over one bad param.
            let _ = user_data.format.parse(param);
        })
        .process(|stream, user_data| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            let Some(data) = datas.first_mut() else {
                return;
            };
            let channels = user_data.format.channels();
            if channels == 0 {
                return;
            }
            let samples_len = data.chunk().size() as usize / mem::size_of::<f32>();
            let Some(samples) = data.data() else { return };

            let peaks = peak_per_channel(samples, samples_len, channels as usize);
            if let Ok(mut map) = user_data.levels.lock() {
                map.insert(user_data.node_id, peaks);
            }
        })
        .register()
        .ok()?;

    // Empty format list: accept whatever the graph is already running at,
    // rather than forcing a conversion just to measure.
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

    Some(Meter {
        _stream: stream,
        _listener: listener,
    })
}

/// Absolute peak of the first two channels in an interleaved f32 buffer.
///
/// Mono is reported on both, so a mono source drives both meter columns
/// rather than leaving one dead.
fn peak_per_channel(bytes: &[u8], samples_len: usize, channels: usize) -> (f32, f32) {
    let peak_of = |channel: usize| {
        let mut max = 0.0_f32;
        let mut i = channel;
        while i < samples_len {
            let start = i * mem::size_of::<f32>();
            let Some(chunk) = bytes.get(start..start + mem::size_of::<f32>()) else {
                break;
            };
            let Ok(word) = chunk.try_into() else { break };
            max = max.max(f32::from_le_bytes(word).abs());
            i += channels;
        }
        max
    };

    let left = peak_of(0);
    let right = if channels > 1 { peak_of(1) } else { left };
    (left, right)
}

#[cfg(test)]
mod tests {
    use super::peak_per_channel;

    fn interleaved(samples: &[f32]) -> Vec<u8> {
        samples.iter().flat_map(|s| s.to_le_bytes()).collect()
    }

    #[test]
    fn stereo_channels_are_read_separately() {
        // L, R, L, R
        let bytes = interleaved(&[0.25, 0.75, -0.5, 0.1]);
        let (l, r) = peak_per_channel(&bytes, 4, 2);
        assert!((l - 0.5).abs() < f32::EPSILON, "left peak was {l}");
        assert!((r - 0.75).abs() < f32::EPSILON, "right peak was {r}");
    }

    #[test]
    fn mono_drives_both_columns() {
        let bytes = interleaved(&[0.4, -0.9]);
        let (l, r) = peak_per_channel(&bytes, 2, 1);
        assert!((l - 0.9).abs() < f32::EPSILON);
        assert!((r - 0.9).abs() < f32::EPSILON);
    }

    #[test]
    fn silence_reads_as_zero() {
        let bytes = interleaved(&[0.0, 0.0]);
        assert_eq!(peak_per_channel(&bytes, 2, 2), (0.0, 0.0));
    }

    #[test]
    fn a_short_buffer_does_not_panic() {
        // Claims more samples than the bytes actually hold.
        let bytes = interleaved(&[0.5]);
        let (l, _) = peak_per_channel(&bytes, 16, 2);
        assert!((l - 0.5).abs() < f32::EPSILON);
    }
}
