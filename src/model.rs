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
            // Composite and TV Mix are down-mixes onto a stereo pair, and
            // the three extract modes carry one channel spread over the pair,
            // so all of them read as stereo.
            _ => 2,
        }
    }
    pub fn index(self) -> u32 {
        MODES.iter().position(|m| *m == self).unwrap_or(0) as u32
    }
}
