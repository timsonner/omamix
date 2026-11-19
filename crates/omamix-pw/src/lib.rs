//! PipeWire boundary.
//!
//! The default build has no libpipewire dependency. `--features pipewire`
//! compiles the filter backend in [`backend`] and [`engine`]. Unit tests do
//! not connect to the daemon. [`run_headless`] and the window engine open a client.

pub mod contract;
pub mod rt;

#[cfg(feature = "pipewire")]
pub mod backend;
#[cfg(feature = "pipewire")]
mod engine;
#[cfg(feature = "pipewire")]
mod filter;

#[cfg(feature = "pipewire")]
pub use engine::{HeadlessOptions, WindowEngine, run_headless, run_list};

use std::fmt;

use omamix_core::comp::{CompWriter, open as open_comp};
use omamix_core::eq::{EqWriter, open};
use omamix_core::limit::{LimitWriter, open as open_limit};
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
    eq: EqWriter,
    comp: CompWriter,
    limit: LimitWriter,
}

impl NullEngine {
    pub fn new() -> Self {
        Self {
            queue: rt::CommandQueue::new(),
            shut_down: false,
            strips: StripParamBank::new(),
            eq: open().0,
            comp: open_comp().0,
            limit: open_limit().0,
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
                EngineCommand::SetStripEq { slot, update } => {
                    self.eq.apply(slot, update);
                }
                EngineCommand::SetMasterEq(update) => {
                    self.eq.apply_master(update);
                }
                EngineCommand::SetStripComp { slot, update } => {
                    self.comp.apply(slot, update);
                }
                EngineCommand::SetMasterComp(update) => {
                    self.comp.apply_master(update);
                }
                EngineCommand::SetMasterLimit(update) => {
                    self.limit.apply_master(update);
                }
                EngineCommand::ReplaceScene(_) => {}
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

    /// EQ applied by [`Self::drive`]. Defaults are flat.
    ///
    /// The handle is not `Sync`. Publishing happens in [`Self::drive`], which
    /// has `&mut self`.
    pub fn eq(&self) -> &EqWriter {
        &self.eq
    }

    /// Compressor applied by [`Self::drive`]. Defaults are bypassed.
    ///
    /// The handle is not `Sync`. Publishing happens in [`Self::drive`], which
    /// has `&mut self`.
    pub fn comp(&self) -> &CompWriter {
        &self.comp
    }

    /// Master limiter applied by [`Self::drive`]. It starts on, at -1 dBTP, with 5 ms of lookahead.
    ///
    /// The handle is not `Sync`. Publishing happens in [`Self::drive`], which
    /// has `&mut self`.
    pub fn limit(&self) -> &LimitWriter {
        &self.limit
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
    Usage(String),
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EngineError::NotConnected => write!(f, "PipeWire client is not connected"),
            EngineError::PipeWire(message) => write!(f, "PipeWire error: {message}"),
            EngineError::Usage(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for EngineError {}

#[cfg(test)]
mod tests {
    use super::*;
    use omamix_core::comp::CompUpdate;
    use omamix_core::eq::EqUpdate;
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

    #[test]
    fn eq_applies_on_drive_and_starts_flat() {
        let mut engine = NullEngine::new();
        let before = engine.eq().settings(0).unwrap();
        assert!(!before.hpf_enabled());
        assert_eq!(before.band_db(), [0.0; 10]);
        engine.command_sender().submit(EngineCommand::SetStripEq {
            slot: 0,
            update: EqUpdate::Band {
                index: 4,
                gain_db: 6.0,
            },
        });
        assert_eq!(engine.eq().settings(0).unwrap().band_db()[4], 0.0);
        engine.drive().unwrap();
        assert!((engine.eq().settings(0).unwrap().band_db()[4] - 6.0).abs() < 1.0e-6);
        engine
            .command_sender()
            .submit(EngineCommand::SetMasterEq(EqUpdate::HighPassEnabled(true)));
        engine.drive().unwrap();
        assert!(engine.eq().master_settings().hpf_enabled());
    }

    #[test]
    fn compressor_applies_on_drive_and_starts_bypassed() {
        let mut engine = NullEngine::new();
        assert!(!engine.comp().settings(0).unwrap().enabled());
        assert!(!engine.comp().master_settings().enabled());
        assert_eq!(engine.comp().gain_reduction_db(0), 0.0);
        engine.command_sender().submit(EngineCommand::SetStripComp {
            slot: 0,
            update: CompUpdate::Enabled(true),
        });
        assert!(!engine.comp().settings(0).unwrap().enabled());
        engine.drive().unwrap();
        assert!(engine.comp().settings(0).unwrap().enabled());
        assert_eq!(engine.comp().settings(0).unwrap().ratio(), 4.0);
        engine
            .command_sender()
            .submit(EngineCommand::SetMasterComp(CompUpdate::ThresholdDb(-24.0)));
        engine.drive().unwrap();
        assert!(!engine.comp().master_settings().enabled());
        assert!((engine.comp().master_settings().threshold_db() - (-24.0)).abs() < 1.0e-5);
    }

    #[test]
    fn master_limiter_applies_on_drive_and_starts_on() {
        use omamix_core::limit::LimitUpdate;
        let mut engine = NullEngine::new();
        assert!(engine.limit().master_settings().enabled());
        assert!((engine.limit().master_settings().ceiling_db() - (-1.0)).abs() < 1.0e-5);
        assert!((engine.limit().master_settings().lookahead_ms() - 5.0).abs() < 1.0e-5);
        engine
            .command_sender()
            .submit(EngineCommand::SetMasterLimit(LimitUpdate::LookaheadMs(
                20.0,
            )));
        assert!((engine.limit().master_settings().lookahead_ms() - 5.0).abs() < 1.0e-5);
        engine.drive().unwrap();
        assert!((engine.limit().master_settings().lookahead_ms() - 20.0).abs() < 1.0e-5);
    }
}
