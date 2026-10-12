//! PipeWire boundary.
//!
//! The default build has no libpipewire dependency. `--features pipewire`
//! compiles the filter backend in [`backend`] and [`engine`]. Unit tests do
//! not connect to the daemon. [`run_headless`] is what opens a client.

pub mod contract;
pub mod rt;

#[cfg(feature = "pipewire")]
pub mod backend;
#[cfg(feature = "pipewire")]
mod engine;
#[cfg(feature = "pipewire")]
mod filter;

#[cfg(feature = "pipewire")]
pub use engine::{HeadlessOptions, run_headless};

use std::fmt;

use omamix_core::params::StripParamBank;

pub use rt::{CommandSender, EngineCommand};

/// What the UI thread is allowed to hold.
///
/// [`AudioEngine::command_sender`] is the only UI entry. Applying commands is
/// the loop thread ([`NullEngine::drive`] in tests, the PipeWire timer when
/// the headless backend is running). `process()` does not drain the channel.
pub trait AudioEngine: Send {
    fn name(&self) -> &'static str;
    fn command_sender(&self) -> CommandSender;
}

/// Engine used by tests and by the UI shell. It does not open a PipeWire client.
pub struct NullEngine {
    queue: rt::CommandQueue,
    shut_down: bool,
    strips: StripParamBank,
}

impl NullEngine {
    pub fn new() -> Self {
        Self {
            queue: rt::CommandQueue::new(),
            shut_down: false,
            strips: StripParamBank::new(),
        }
    }

    /// Audio-thread side. Drains the queue. The UI does not call this.
    pub fn drive(&mut self) -> Result<(), EngineError> {
        while let Some(command) = self.queue.try_recv() {
            match command {
                EngineCommand::Shutdown => self.shut_down = true,
                EngineCommand::SetMasterFaderDb(_) | EngineCommand::SetMonitorMix(_) => {}
                EngineCommand::SetStripParam { slot, update } => {
                    self.strips.apply(slot, update);
                }
            }
        }
        Ok(())
    }

    pub fn is_shut_down(&self) -> bool {
        self.shut_down
    }

    /// Parameters applied by [`Self::drive`]. Defaults are neutral.
    pub fn strips(&self) -> &StripParamBank {
        &self.strips
    }
}

impl Default for NullEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioEngine for NullEngine {
    fn name(&self) -> &'static str {
        "null"
    }

    fn command_sender(&self) -> CommandSender {
        self.queue.sender()
    }
}

#[derive(Debug)]
pub enum EngineError {
    /// The feature is compiled, but no client has been opened.
    NotConnected,
    PipeWire(String),
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EngineError::NotConnected => write!(f, "PipeWire client is not connected"),
            EngineError::PipeWire(message) => write!(f, "PipeWire error: {message}"),
        }
    }
}

impl std::error::Error for EngineError {}

#[cfg(test)]
mod tests {
    use super::*;
    use omamix_core::params::StripParamUpdate;

    #[test]
    fn ui_submit_does_not_run_the_engine_until_drive() {
        let mut engine = NullEngine::new();
        assert_eq!(engine.name(), "null");
        engine.command_sender().submit(EngineCommand::Shutdown);
        assert!(!engine.is_shut_down());
        engine.drive().unwrap();
        assert!(engine.is_shut_down());
        engine.drive().unwrap();
    }

    #[test]
    fn strip_params_apply_on_drive_not_on_submit() {
        let mut engine = NullEngine::new();
        assert!(engine.strips().enabled(1));
        assert!(!engine.strips().mute(1));
        engine
            .command_sender()
            .submit(EngineCommand::SetStripParam {
                slot: 1,
                update: StripParamUpdate::Mute(true),
            });
        assert!(!engine.strips().mute(1));
        engine.drive().unwrap();
        assert!(engine.strips().mute(1));
        assert!(engine.strips().enabled(1));
        assert_eq!(engine.strips().fader_db(1), 0.0);
    }
}
