//! Boundary between the UI thread and the PipeWire loop thread.
//!
//! The UI may [`CommandSender::submit`]. The loop thread drains the queue
//! outside `process()`. The realtime callback only reads atomics. It does not
//! receive on this channel. This module does not start that thread and does
//! not open a client.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};

use omamix_core::db::{self, BelowMin};

/// What the UI is allowed to enqueue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EngineCommand {
    Shutdown,
}

/// Clone of the sending half. Safe to hold on the UI thread.
#[derive(Clone, Debug)]
pub struct CommandSender {
    tx: Sender<EngineCommand>,
}

impl CommandSender {
    /// Non-blocking enqueue. Does not run DSP or touch PipeWire.
    pub fn submit(&self, command: EngineCommand) {
        let _ = self.tx.send(command);
    }
}

/// Owned by the audio side. The UI receives only a [`CommandSender`].
pub struct CommandQueue {
    tx: Sender<EngineCommand>,
    rx: Receiver<EngineCommand>,
}

impl CommandQueue {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        Self { tx, rx }
    }

    pub fn sender(&self) -> CommandSender {
        CommandSender {
            tx: self.tx.clone(),
        }
    }

    pub fn try_recv(&self) -> Option<EngineCommand> {
        self.rx.try_recv().ok()
    }
}

impl Default for CommandQueue {
    fn default() -> Self {
        Self::new()
    }
}

/// Peak and RMS in dB, published without a mutex.
#[derive(Debug)]
pub struct AtomicMeter {
    peak_bits: AtomicU32,
    rms_bits: AtomicU32,
}

impl AtomicMeter {
    pub fn new() -> Self {
        Self {
            peak_bits: AtomicU32::new(f32::NEG_INFINITY.to_bits()),
            rms_bits: AtomicU32::new(f32::NEG_INFINITY.to_bits()),
        }
    }

    pub fn store(&self, peak_db: f32, rms_db: f32) {
        self.peak_bits
            .store(meter_db(peak_db).to_bits(), Ordering::Relaxed);
        self.rms_bits
            .store(meter_db(rms_db).to_bits(), Ordering::Relaxed);
    }

    pub fn load(&self) -> (f32, f32) {
        (
            f32::from_bits(self.peak_bits.load(Ordering::Relaxed)),
            f32::from_bits(self.rms_bits.load(Ordering::Relaxed)),
        )
    }
}

impl Default for AtomicMeter {
    fn default() -> Self {
        Self::new()
    }
}

fn meter_db(db: f32) -> f32 {
    db::sanitize_db(db, f32::NEG_INFINITY, db::MAX_BOOST_DB, BelowMin::Clamp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn submit_does_not_apply_until_the_audio_side_receives() {
        let queue = CommandQueue::new();
        let ui = queue.sender();
        ui.submit(EngineCommand::Shutdown);
        assert_eq!(queue.try_recv(), Some(EngineCommand::Shutdown));
        assert_eq!(queue.try_recv(), None);
    }

    #[test]
    fn meter_store_of_nan_reads_back_as_silence() {
        let meter = AtomicMeter::new();
        meter.store(f32::NAN, f32::INFINITY);
        let (peak, rms) = meter.load();
        assert!(peak.is_infinite() && peak.is_sign_negative());
        assert_eq!(rms, db::MAX_BOOST_DB);
        assert!(peak.to_bits() != f32::NAN.to_bits());
    }
}
