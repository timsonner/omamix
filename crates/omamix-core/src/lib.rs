//! Mixer model that does not link PipeWire.
//!
//! Levels, routing keys, and the DSP chain live here so `cargo test` can run
//! with no PipeWire daemon and no libpipewire.

pub mod comp;
pub mod db;
pub mod dsp;
pub mod effect;
pub mod eq;
pub mod fader;
pub mod graph;
pub mod limit;
pub mod link;
pub mod live_cap;
pub mod metadata;
pub mod meter;
pub mod params;
pub mod route;
pub mod scene_file;
pub mod scope;

#[cfg(test)]
mod scene_check;
#[cfg(test)]
mod test_alloc;

/// Print one measured line with the Cargo profile and where it ran.
///
/// `debug_assertions` is on for the test profile and off for `--release`.
/// The machine token is `OMAMIX_MACHINE`, or `unlabelled` when that variable
/// is unset or empty. Timing lines stay report-only.
#[cfg(test)]
pub(crate) fn measured(message: &str) {
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    let machine = std::env::var("OMAMIX_MACHINE").unwrap_or_default();
    let machine = if machine.is_empty() {
        "unlabelled"
    } else {
        machine.as_str()
    };
    eprintln!("{message} profile {profile} machine {machine}");
}

pub use comp::static_reduction_db;
pub use db::{cubic_to_db, db_to_cubic, db_to_linear, linear_to_db};
pub use eq::response_db;
pub use graph::{MasterBus, Scene, Strip, StripId};
pub use route::{PlaybackStream, RouteKey, SourceKind};
pub use scene_file::{
    SceneFileError, default_scene_path, load_scene_file, save_scene_file, scene_config_path,
    scene_from_toml, scene_to_toml,
};
