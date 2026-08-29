//! Everything the mixer is, apart from how it looks.
//!
//! The graph, the devices it remembers, the single-instance lock and the
//! paths it keeps its files under. None of it draws anything, and none of
//! it knows that a strip has a fader on it - which is the point: the skin
//! is one way of showing this, not the thing itself.
//!
//! What is not here yet is the model - `Strip`, `Bus`, the panel's state -
//! which still lives in the binary alongside the code that draws it. That
//! is the next seam and the harder one, because those types mix what a
//! strip *is* with how it is painted.

#![forbid(unsafe_code)]
// Every public item here is public only because the binary is a separate
// crate now, not because this is an API anyone else consumes. Annotating
// a hundred internal getters would be noise that hides the ones where
// dropping the result is a real mistake.
#![allow(clippy::must_use_candidate)]

pub mod audio;
pub mod devices;
pub mod instance;
pub mod model;
pub mod paths;
