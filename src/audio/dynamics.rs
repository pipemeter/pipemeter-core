//! The gate, compressor, limiter and denoiser: what their controls are
//! called and what a knob position means to each.
//!
//! Split out of `eq` because they are a different vocabulary that happens
//! to live in the same chains. Every number here was measured rather than
//! chosen, and the doc comments say against what - `caps` and `PipeWire`
//! agree with their own documentation only sometimes.

use std::path::PathBuf;

/// The brickwall limiter at the end of every strip graph.
///
/// `clamp`, not a compressor. A compressor was tried first and rejected
/// for a specific reason: its threshold is not absolute. Measured against
/// one tone it tracked its dial nicely, and against a tone 3 dB quieter
/// the same setting did nothing at all, so the number could never mean
/// what the reference promises - that the signal "is never going over the
/// threshold". A clamp means exactly that, and measured exactly that:
/// +-0.5 held a loud tone at -6.02 dBFS and +-0.25 at -12.04, which are
/// the arithmetic answers to the hundredth.
///
/// It clips rather than ducking, so it distorts when it acts. That is the
/// honest trade for a guarantee, and a limiter that is doing its job is
/// one you have already set too low.
pub const LIMIT_NODE: &str = "lim";

/// Its two controls, as filter-chain addresses them.
pub const LIMIT_MAX: &str = "lim:Max";
pub const LIMIT_MIN: &str = "lim:Min";

/// The lowest the mixer offers. A clamp is exact anywhere, so this is a
/// judgement about what is useful rather than what works.
pub const LIMIT_MIN_DB: f32 = -40.0;

/// Turn a threshold in dB into the amplitude the clamp holds it at.
#[must_use]
pub fn limit_amplitude(db: f32) -> f32 {
    10.0f32.powf(db / 20.0)
}

/// The compressor's threshold and output gain, as filter-chain addresses
/// them.
pub const COMP_THRESHOLD: &str = "comp:threshold";
pub const COMP_GAIN: &str = "comp:gain (dB)";

/// Where the plugin's 0..1 threshold sits in decibels, and how fast it
/// moves.
///
/// Measured, not guessed. A ten-step staircase from -33 to -6 dBFS was
/// played through the plugin and the onset - the quietest step it still
/// left alone - was read off at each setting:
///
/// | control | 0.80 | 0.75 | 0.70 | 0.60 | 0.50 | 0.45 | 0.40 | 0.35 |
/// | onset   |  -6  |  -9  | -12  | -16  | -21  | -24  | -27  | -30  |
///
/// That is a straight line: fifty decibels per unit, through -6 dB at
/// 0.80. The last three rows were predictions before they were
/// measurements, which is why the line is trusted.
const COMP_DB_PER_UNIT: f32 = 50.0;
const COMP_ANCHOR_T: f32 = 0.80;
const COMP_ANCHOR_DB: f32 = -6.0;

/// Turn a compressor threshold in dB into the plugin's control.
///
/// Clamped at the bottom to where it was actually measured; below that
/// the plugin starts acting on everything and the line was never checked.
#[must_use]
pub fn comp_threshold(db: f32) -> f32 {
    let raw = COMP_ANCHOR_T + (db - COMP_ANCHOR_DB) / COMP_DB_PER_UNIT;
    raw.clamp(0.35, 1.0)
}

/// `caps` Compress runs in mode 0, and the mode is not optional.
///
/// Its default is mode 1, which is not transparent: measured with the
/// strength at zero - no compression asked for at all - it still took a
/// -8.73 dBFS tone to -11.60. Every hardware strip was quietly losing
/// 2.87 dB for as long as the mode went unset. In mode 0 the same
/// configuration returns -8.73 exactly.
///
/// The limiter learned this first and the compressor did not, which is
/// the whole reason it went unnoticed.
/// The controls the AUDIBILITY knobs drive, as filter-chain addresses them.
pub const GATE_CONTROL: &str = "gate:open (dB)";
pub const COMP_CONTROL: &str = "comp:strength";

/// Gate threshold for a knob at rest and at full.
///
/// At rest the gate has to be inaudible rather than merely gentle, so the
/// bottom of the range sits below anything the plugin will act on.
pub const GATE_OPEN_MIN: f32 = -60.0;
pub const GATE_OPEN_MAX: f32 = -12.0;

/// Turn the Gate knob into the threshold it should open at, in dB.
#[must_use]
pub fn gate_open_db(knob: f32) -> f32 {
    GATE_OPEN_MIN + knob.clamp(0.0, 1.0) * (GATE_OPEN_MAX - GATE_OPEN_MIN)
}

/// Where the Gate knob sits for a threshold in dB - the inverse of
/// [`gate_open_db`].
///
/// The knob and the dialog's THRESHOLD are two views of one value, so
/// moving either has to move the other. Without this the dialog could
/// show -30 dB while the knob it shares a strip with sat at the bottom.
#[must_use]
pub fn gate_knob_from_db(db: f32) -> f32 {
    ((db - GATE_OPEN_MIN) / (GATE_OPEN_MAX - GATE_OPEN_MIN)).clamp(0.0, 1.0)
}

/// The gate's attack, as filter-chain addresses it. The plugin takes
/// milliseconds, like the settings file, but only up to five.
pub const GATE_ATTACK: &str = "gate:attack (ms)";

/// Where the gate actually shuts.
///
/// `open (dB)` is where it lets go and `close (dB)` is where it clamps
/// down, and it is the second that decides whether anything is gated at
/// all. This was fixed at -80 dB, which nothing reaches, so the gate
/// opened on the first sound and never closed again: measured against a
/// staircase it passed all ten steps from -33 to -6 dBFS untouched. With
/// a close threshold four decibels under the open one it gates what it
/// should - -33 silenced, the rest through.
pub const GATE_CLOSE: &str = "gate:close (dB)";

/// How far under the open threshold the gate shuts.
///
/// Hysteresis, so a signal sitting on the threshold does not chatter.
const GATE_HYSTERESIS: f32 = 4.0;

/// The close threshold for an open one.
#[must_use]
pub fn gate_close_db(open_db: f32) -> f32 {
    (open_db - GATE_HYSTERESIS).clamp(-80.0, 0.0)
}

/// The denoiser, when one is installed.
///
/// `noise-suppression-for-voice`, which wraps `RNNoise` as a LADSPA plugin.
/// Chosen by measurement and by what this machine can load: against a
/// signal alternating noise with a voice-shaped harmonic stack it took
/// the noise-only stretches from -30.8 to -90.3 dBFS while leaving the
/// voice at -19.6 against -19.0 - separation from 11.8 dB to 70.7. Its
/// only rival, `noise-repellent`, is LV2, and this `PipeWire` has no LV2
/// loader at all: builtin, ebur128, ladspa and sofa, and that is the
/// list.
///
/// It is GPL-3.0, so it is loaded if the user has installed it and never
/// shipped with us.
pub const DENOISER_PLUGIN: &str = "librnnoise_ladspa";
pub const DENOISER_LABEL: &str = "noise_suppressor_mono";

/// Its dry and wet sides, blended so the knob is an amount rather than a
/// switch - the plugin itself has no depth control.
pub const DENOISER_DRY: &str = "dmix:Gain 1";
pub const DENOISER_WET: &str = "dmix:Gain 2";

/// Where a LADSPA plugin might be, including the user's own directory.
///
/// A chain naming a plugin that is not there fails to start at all, which
/// would take the whole strip with it - so the graph asks first.
#[must_use]
pub fn denoiser_available() -> bool {
    denoiser_search_paths().any(|dir| dir.join(format!("{DENOISER_PLUGIN}.so")).exists())
}

pub fn denoiser_search_paths() -> impl Iterator<Item = PathBuf> {
    let from_env = std::env::var("LADSPA_PATH").unwrap_or_default();
    let home = std::env::var("HOME").unwrap_or_default();
    from_env
        .split(':')
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .chain([
            PathBuf::from(&home).join(".ladspa"),
            PathBuf::from("/usr/lib64/ladspa"),
            PathBuf::from("/usr/lib/ladspa"),
        ])
        .collect::<Vec<_>>()
        .into_iter()
}

/// The knob's dry and wet gains. At rest the denoiser is out of the way.
#[must_use]
pub fn denoiser_blend(knob: f32) -> (f32, f32) {
    let wet = knob.clamp(0.0, 1.0);
    (1.0 - wet, wet)
}

/// The gate's dry blend, which is how a floor is made.
///
/// `caps` Noisegate has no depth control - it shuts fully or not at all -
/// so the floor comes from mixing the ungated signal back in, which is
/// what `onjoakimsmind/pipemeeter` does and where the idea came from.
/// Damping in decibels becomes the dry side's gain.
pub const GATE_DRY: &str = "gmix:Gain 1";
pub const GATE_WET: &str = "gmix:Gain 2";

/// The dry gain for a damping in dB, and the wet gain that goes with it.
#[must_use]
pub fn gate_blend(damping_db: f32) -> (f32, f32) {
    let dry = 10.0f32.powf(damping_db.clamp(-80.0, 0.0) / 20.0);
    (dry, (1.0 - dry).clamp(0.0, 1.0))
}

/// The Comp knob is the compressor's strength directly, both being 0..1.
#[must_use]
pub fn comp_strength(knob: f32) -> f32 {
    knob.clamp(0.0, 1.0)
}
