//! Mixer model that does not link PipeWire.
//!
//! Levels, routing keys, and the DSP chain live here so `cargo test` can run
//! with no PipeWire daemon and no libpipewire.

pub mod db;
pub mod dsp;
pub mod fader;
pub mod graph;
pub mod link;
pub mod metadata;
pub mod params;
pub mod route;

#[cfg(test)]
mod test_alloc;

pub use db::{cubic_to_db, db_to_cubic, db_to_linear, linear_to_db};
pub use graph::{MasterBus, Scene, Strip, StripId};
pub use route::{PlaybackStream, RouteKey, SourceKind};
