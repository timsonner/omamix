//! PipeWire boundary.
//!
//! The default build has no libpipewire dependency. `--features pipewire`
//! compiles the stream setup in [`backend`]. Nothing in this crate connects
//! to the daemon from a unit test.

pub mod contract;
pub mod rt;

#[cfg(feature = "pipewire")]
pub mod backend;

use std::fmt;

pub use rt::{CommandSender, EngineCommand};

/// What the UI thread is allowed to hold.
///
/// [`AudioEngine::command_sender`] is the only UI entry. Applying commands is
/// [`NullEngine::drive`], which the UI does not call. The PipeWire thread will
/// own that side later.
pub trait AudioEngine: Send {
    fn name(&self) -> &'static str;
    fn command_sender(&self) -> CommandSender;
}

/// Engine used by tests and by the UI shell. It does not open a PipeWire client.
pub struct NullEngine {
    queue: rt::CommandQueue,
    shut_down: bool,
}

impl NullEngine {
    pub fn new() -> Self {
        Self {
            queue: rt::CommandQueue::new(),
            shut_down: false,
        }
    }

    /// Audio-thread side. Drains the queue. The UI does not call this.
    pub fn drive(&mut self) -> Result<(), EngineError> {
        while let Some(command) = self.queue.try_recv() {
            match command {
                EngineCommand::Shutdown => self.shut_down = true,
            }
        }
        Ok(())
    }

    pub fn is_shut_down(&self) -> bool {
        self.shut_down
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
}
