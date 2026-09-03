//! Everything the mixer is, apart from how it looks.
//!
//! The graph, the devices it remembers, the single-instance lock and the
//! paths it keeps its files under. None of it draws anything, and none of
//! it knows that a strip has a fader on it - which is the point: the skin
//! is one way of showing this, not the thing itself.
//!
//! The model came across too: `Strip`, `Bus`, `Mode`, `Section` and the
//! rest live in [`model`]. Only the drawing stayed behind, as free
//! functions in the binary - a binary cannot add inherent methods to a
//! type another crate defines, which is what forced that seam to be a
//! clean one rather than a line drawn through the middle of `Strip`.
//!
//! So a `Strip` here knows its gain, its routing and its EQ, and knows
//! nothing about the fader that shows them.

#![forbid(unsafe_code)]
#![allow(clippy::must_use_candidate)]

pub mod audio;
pub mod defaults;
pub mod devices;
pub mod instance;
pub mod model;
pub mod paths;
