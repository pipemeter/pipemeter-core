//! What the mixer is, as opposed to how it looks.
//!
//! The first piece of the model to move out of the skin. A bus mode is a
//! good place to start because the split is unusually clear in it: which
//! twelve modes exist, what they are called on the wire and how they are
//! numbered in a settings file are all facts about the mixer, while the
//! caption that wraps to two lines and the colour it is drawn in are facts
//! about the window. Only the first half is here.

/// Mix mode of an output bus. The caption is what the button shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Amix,
    Bmix,
    Repeat,
    Composite,
    TvMix,
    UpMix21,
    UpMix41,
    UpMix61,
    CenterOnly,
    LfeOnly,
    RearOnly,
}

/// Every mode, in the order the remote API numbers them.
///
/// Not a guess: the API lists exactly these twelve in this order, and the
/// indicator grid on the button is twelve cells lighting the one at the
/// mode's own index — which is how the two were checked against each other.
/// `BusMode` in a settings file is an index into this.
pub const MODES: [Mode; 12] = [
    Mode::Normal,
    Mode::Amix,
    Mode::Bmix,
    Mode::Repeat,
    Mode::Composite,
    Mode::TvMix,
    Mode::UpMix21,
    Mode::UpMix41,
    Mode::UpMix61,
    Mode::CenterOnly,
    Mode::LfeOnly,
    Mode::RearOnly,
];

impl Mode {
    /// The next mode round, which is what clicking the button does.
    ///
    /// Cycling order is the order of `MODES`, so it matches the grid of
    /// dots the button draws and a settings file's numbering.
    #[must_use]
    pub fn next(self) -> Self {
        MODES[(self.index() as usize + 1) % MODES.len()]
    }

    pub fn remote_name(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Amix => "amix",
            Self::Bmix => "bmix",
            Self::Repeat => "repeat",
            Self::Composite => "composite",
            Self::TvMix => "tvmix",
            Self::UpMix21 => "upmix21",
            Self::UpMix41 => "upmix41",
            Self::UpMix61 => "upmix61",
            Self::CenterOnly => "centeronly",
            Self::LfeOnly => "lfeonly",
            Self::RearOnly => "rearonly",
        }
    }
    pub fn from_remote_name(name: &str) -> Option<Self> {
        let name = name.to_ascii_lowercase();
        MODES.into_iter().find(|mode| mode.remote_name() == name)
    }
    pub fn from_index(index: u32) -> Self {
        MODES.get(index as usize).copied().unwrap_or(Self::Normal)
    }
    pub fn channels(self) -> usize {
        match self {
            Self::UpMix21 => 3,
            Self::UpMix41 => 5,
            Self::UpMix61 => 7,
            _ => 2,
        }
    }
    pub fn index(self) -> u32 {
        MODES.iter().position(|m| *m == self).unwrap_or(0) as u32
    }
}

/// Bus names, in fixed order.
pub const BUS_NAMES: [&str; 8] = ["A1", "A2", "A3", "A4", "A5", "B1", "B2", "B3"];

/// State of one output bus.
#[derive(Debug, Clone)]
pub struct Bus {
    pub name: &'static str,
    /// The name the user gave this bus. Shown up its fader; absent means
    /// "Fader Gain", the same rule the input strips follow.
    pub label: Option<String>,
    /// The `PipeWire` node this bus feeds. The B buses are our own null
    /// sinks; the A buses get one when a hardware out is assigned from the
    /// device picker, so this is owned rather than static.
    pub node_name: Option<String>,
    /// Human name of the device assigned to this bus, shown up the fader.
    pub device: Option<String>,
    /// Whether that device is assigned but not currently in the graph. The
    /// bus keeps its assignment either way and says so in red.
    pub device_missing: bool,
    pub mode: Mode,
    /// The bus this one is monitoring, if any.
    ///
    /// A monitoring bus carries whatever its target carries, which is what
    /// makes a pair of headphones on A1 able to hear the speakers on A2
    /// without routing every strip twice. It replaces the mix mode rather
    /// than sitting beside it - the original's button shows "Mon A2" where
    /// the mode would be, filled cream, with no channel dots.
    pub monitor: Option<usize>,
    /// SEL, mono, EQ and Mute, in that order. An array rather than four named
    /// flags so the struct stays under clippy's bool limit, and because the
    /// three lower ones are drawn from one loop anyway.
    pub toggles: [bool; 4],
    pub gain_db: f32,
    /// FX return knobs, reverb and delay.
    pub fx_return: [f32; 2],
    /// The six cells of the master EQ.
    ///
    /// Always present, whether or not the EQ is switched on: the toggle in
    /// `toggles` is what silences it, and a cell keeps its setting across
    /// being switched off and on again the way an effect keeps its
    /// character.
    pub eq_cells: [EqCell; crate::audio::eq::BUS_BANDS],
    pub levels: (f32, f32),
}

/// One cell of a bus's parametric EQ.
///
/// The defaults are the original's, read off a real settings file rather
/// than chosen; `audio::eq::BUS_FREQUENCIES` is where they live and why.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EqCell {
    pub freq: f32,
    pub q: f32,
    pub gain_db: f32,
    pub on: bool,
}

impl Default for EqCell {
    fn default() -> Self {
        Self {
            freq: crate::audio::eq::BUS_FREQUENCIES[0],
            q: crate::audio::eq::BUS_Q,
            gain_db: 0.0,
            on: true,
        }
    }
}

/// The six cells a bus starts with, spread across the reference centres.
#[must_use]
pub fn default_eq_cells() -> [EqCell; crate::audio::eq::BUS_BANDS] {
    let mut cells = [EqCell::default(); crate::audio::eq::BUS_BANDS];
    for (cell, freq) in cells.iter_mut().zip(crate::audio::eq::BUS_FREQUENCIES) {
        cell.freq = freq;
    }
    cells
}

impl Bus {
    /// The four FX returns as the settings file stores them: reverb, delay,
    /// then the two external ones. Only the first two have knobs here, so
    /// the others go out as they came in — which is why the caller passes
    /// what it read rather than this making them up.
    #[must_use]
    pub fn fx_return_all(&self) -> [f32; 4] {
        [self.fx_return[0], self.fx_return[1], 0.0, 0.0]
    }

    /// What runs up the fader: the name the user gave the bus, else the
    /// device on it, else the default. The original shows the device, which
    /// is what makes a wall of eight faders readable at a glance.
    #[must_use]
    pub fn legend(&self) -> &str {
        self.label
            .as_deref()
            .or(self.device.as_deref())
            .unwrap_or("Fader Gain")
    }

    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            label: None,
            device: None,
            device_missing: false,
            monitor: None,
            node_name: match name {
                "B1" => Some("pipemeter_b1".to_owned()),
                "B2" => Some("pipemeter_b2".to_owned()),
                "B3" => Some("pipemeter_b3".to_owned()),
                _ => None,
            },
            mode: Mode::Normal,
            toggles: [false; 4],
            gain_db: 0.0,
            fx_return: [0.0; 2],
            eq_cells: default_eq_cells(),
            levels: (0.0, 0.0),
        }
    }
}

/// Where the limiter rests: the top of the fader range, which is where
/// the original leaves it and means no limiting at all.
pub const LIMIT_OFF: f32 = 12.0;

/// Indices into [`Bus::toggles`].
pub const SEL: usize = 0;
pub const MONO: usize = 1;
pub const EQ: usize = 2;
pub const MUTE: usize = 3;

/// Build the eight buses in their fixed order.
pub fn default_buses() -> Vec<Bus> {
    BUS_NAMES.iter().map(|n| Bus::new(n)).collect()
}

/// Which part of a strip an entry acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    /// Everything below, at once.
    All,
    ParametricEq,
    Pan,
    Compressor,
    Gate,
    Denoiser,
    FxSend,
}

/// Everything about an input strip that is not how it is drawn.
///
/// Its indices sit in their own module because a bus has a `MUTE` too, and
/// the two are different positions in different arrays.
pub mod strip {
    /// Indices into [`super::Strip::flags`].
    pub const MUTE: usize = 0;
    pub const SOLO: usize = 1;
    pub const MONO: usize = 2;
    pub const EQ_ON: usize = 3;

    /// Indices into [`super::Strip::fx_post`].
    pub const POST_REVERB: usize = 0;
    pub const POST_DELAY: usize = 1;
    pub const POST_SEND1: usize = 2;
    pub const POST_SEND2: usize = 3;
}

/// Which layout an input strip uses. Hardware and virtual strips differ in
/// everything between the header and the fader row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Hardware,
    Virtual,
}

/// An application playing into a virtual strip.
#[derive(Debug, Clone)]
pub struct AppEntry {
    /// The stream's `PipeWire` node, so the slider and the M button act on
    /// the application itself rather than on a number the mixer keeps to
    /// itself.
    pub node: u32,
    pub name: String,
    /// 0.0..=1.0.
    pub volume: f32,
    pub muted: bool,
    /// Peak per channel, as for a strip.
    ///
    /// Read from a capture stream of our own, targeted at the application's
    /// node. `PipeWire` publishes no peak for a playback stream and a stream
    /// has no monitor, which is why this was first written off as
    /// impossible - but the desktop's own volume applet meters applications,
    /// so it plainly is not, and the session manager will link a capture
    /// stream to another stream's output when asked by name.
    pub levels: (f32, f32),
}

/// State of one input strip.
#[derive(Debug, Clone)]
pub struct Strip {
    pub kind: Kind,
    /// The `PipeWire` node backing this strip, for virtual strips. Hardware
    /// strips get one when a device is assigned to them.
    pub node_name: Option<String>,
    /// Output node of this strip's EQ chain, when it has one: what the
    /// routing matrix reads the strip from.
    pub eq_node: Option<String>,
    /// Input node of the same chain. The band controls live here, on the
    /// end that owns the filter graph, not on the end the audio leaves by.
    pub eq_controls: Option<String>,
    pub name: String,
    /// The name the user (or an import) gave this strip, as opposed to its
    /// default heading. Shown on the fader; absent means "Fader Gain".
    pub label: Option<String>,
    /// Device assigned to this strip, shown as the header's second line.
    pub device: String,
    /// Whether that device is assigned but not currently in the graph.
    ///
    /// Drawn red when it is. The assignment is kept either way - a headset
    /// that is charging has not stopped being the one you route to.
    pub device_missing: bool,
    /// Routing assignments, indexed to match [`BUS_NAMES`].
    pub buses: [bool; 8],
    /// Fader position in dB.
    pub gain_db: f32,
    /// Where the brickwall limiter holds the signal, in dB.
    ///
    /// The reference sets this by dragging on the strip's own VU meter and
    /// stores it as `dblimit`. It rests at the top of the fader range,
    /// which is the same as no limiting - so a strip that has never been
    /// touched is not quietly being squashed.
    pub limit_db: f32,
    /// Mute, solo, mono and EQ-on. An array rather than four named flags,
    /// matching how [`Bus`] stores its own. Index with the
    /// constants below.
    pub flags: [bool; 4],
    /// Current levels per channel, 0.0..=1.0, for the meter.
    pub levels: (f32, f32),

    /// pan handle, each axis 0.0..=1.0 with (0,0) at bottom-left.
    /// Handle position per face. The three faces control different things,
    /// so dragging one must not move the others.
    pub pad: [(f32, f32); 3],
    /// Which face that pad is showing.
    pub panel: PanelView,
    /// Knob positions, each 0.0..=1.0.
    pub comp: f32,
    pub gate: f32,
    /// Fader level per scene. The eight buttons in the banner recall these,
    /// which is what they are for: a scene is a set of fader positions, not
    /// a snapshot of the whole mixer.
    pub layers: [f32; 8],
    /// Whether this virtual strip's first button is the karaoke one. Only
    /// one strip carries it, and which one comes from the settings file.
    pub karaoke: bool,
    /// The third AUDIBILITY knob. Potato has three where the smaller
    /// editions have two, and this is the one that is easy to miss.
    pub denoiser: f32,
    pub reverb: f32,
    pub delay: f32,
    pub send1: f32,
    pub send2: f32,
    /// EQUALIZER knobs, virtual strips only.
    pub eq: [f32; 3],
    /// Front/Rear pan handle, virtual strips only.
    pub pan: (f32, f32),
    /// Applications playing into this strip, virtual strips only.
    pub apps: Vec<AppEntry>,
    /// Whether the application list has taken over the whole strip.
    ///
    /// Right-clicking the list folds the EQUALIZER and the Front/Rear pad
    /// away so the applications run from under the device name down to the
    /// fader, which is what the original does when a strip has more of them
    /// than the leftover space holds.
    pub apps_expanded: bool,
    /// Pre/post toggles flanking the two FX bands, in the order
    /// reverb, delay, send 1, send 2. Kept as an array rather than four
    /// named flags so the struct stays readable.
    pub fx_post: [bool; 4],
}

/// Which face the pan pad is showing.
///
/// Right-clicking the pad cycles them, as in the original. Each has its own
/// legends and handle; the axes underneath are the same two numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PanelView {
    #[default]
    Voice,
    Modulation,
    Position,
}

impl PanelView {
    /// The next face in the cycle.
    #[must_use]
    pub fn next(self) -> Self {
        match self {
            Self::Voice => Self::Modulation,
            Self::Modulation => Self::Position,
            Self::Position => Self::Voice,
        }
    }

    /// How the settings file numbers the faces.
    #[must_use]
    pub fn from_index(index: usize) -> Self {
        match index {
            1 => Self::Modulation,
            2 => Self::Position,
            _ => Self::Voice,
        }
    }

    /// The other half of [`Self::from_index`].
    #[must_use]
    pub fn index(self) -> usize {
        match self {
            Self::Voice => 0,
            Self::Modulation => 1,
            Self::Position => 2,
        }
    }
}

/// A strip's settings, lifted out so they can be copied onto another.
///
/// Deliberately not the whole [`Strip`]: the name, the device and the node
/// behind it belong to *that* strip and copying them would be nonsense.
/// What travels is how the strip is set up, not what it is.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub eq: [f32; 3],
    pub pad: [(f32, f32); 3],
    pub panel: PanelView,
    pub comp: f32,
    pub gate: f32,
    pub denoiser: f32,
    pub sends: [f32; 4],
    pub fx_post: [bool; 4],
}

impl Strip {
    pub fn new(kind: Kind, name: impl Into<String>, device: impl Into<String>) -> Self {
        Self {
            kind,
            node_name: None,
            eq_node: None,
            eq_controls: None,
            name: name.into(),
            label: None,
            device: device.into(),
            device_missing: false,
            buses: [false; 8],
            gain_db: 0.0,
            limit_db: LIMIT_OFF,
            flags: [false; 4],
            levels: (0.0, 0.0),
            pad: [(0.5, 0.5); 3],
            panel: PanelView::default(),
            comp: 0.0,
            gate: 0.0,
            layers: [0.0; 8],
            karaoke: false,
            denoiser: 0.0,
            reverb: 0.0,
            delay: 0.0,
            send1: 0.0,
            send2: 0.0,
            eq: [0.5; 3],
            pan: (0.5, 0.5),
            apps: Vec::new(),
            apps_expanded: false,
            fx_post: [false; 4],
        }
    }
    /// The filter-chain node the routing matrix should read this strip from,
    /// once one has been started for it. Absent on hardware strips, which
    /// have no EQ.
    ///
    /// Held separately from `node_name` because the two are different nodes
    /// doing different jobs: the fader and the meter stay on the sink, only
    /// the routing moves to the far end of the chain.
    pub fn eq_source(&self) -> Option<&str> {
        self.eq_node.as_deref()
    }
    #[must_use]
    pub fn backed_by(mut self, node_name: &str) -> Self {
        self.node_name = Some(node_name.to_owned());
        self
    }

    /// Everything a copy takes from this strip.
    #[must_use]
    pub fn settings(&self) -> Settings {
        Settings {
            eq: self.eq,
            pad: self.pad,
            panel: self.panel,
            comp: self.comp,
            gate: self.gate,
            denoiser: self.denoiser,
            sends: [self.reverb, self.delay, self.send1, self.send2],
            fx_post: self.fx_post,
        }
    }

    /// Apply one section of another strip's settings.
    pub fn apply(&mut self, from: &Settings, section: Section) {
        let all = section == Section::All;
        if all || section == Section::ParametricEq {
            self.eq = from.eq;
        }
        if all || section == Section::Pan {
            self.pad = from.pad;
            self.panel = from.panel;
        }
        if all || section == Section::Compressor {
            self.comp = from.comp;
        }
        if all || section == Section::Gate {
            self.gate = from.gate;
        }
        if all || section == Section::Denoiser {
            self.denoiser = from.denoiser;
        }
        if all || section == Section::FxSend {
            [self.reverb, self.delay, self.send1, self.send2] = from.sends;
            self.fx_post = from.fx_post;
        }
    }

    /// Put one section back to how a fresh strip has it.
    pub fn reset(&mut self, section: Section) {
        let fresh = Self::new(self.kind, self.name.clone(), self.device.clone());
        self.apply(&fresh.settings(), section);
    }
}
