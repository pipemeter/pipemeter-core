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
    pub levels: (f32, f32),
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
            levels: (0.0, 0.0),
        }
    }
}

/// Indices into [`Bus::toggles`].
pub const SEL: usize = 0;
pub const MONO: usize = 1;
pub const EQ: usize = 2;
pub const MUTE: usize = 3;

/// Build the eight buses in their fixed order.
pub fn default_buses() -> Vec<Bus> {
    BUS_NAMES.iter().map(|n| Bus::new(n)).collect()
}
