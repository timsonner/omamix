//! Boundary between the UI thread and the PipeWire loop thread.
//!
//! The UI may [`CommandSender::submit`]. The loop thread drains the queue
//! outside `process()`. `process()` writes [`AtomicMeter`] and reads
//! [`AtomicParams`]. It does not receive on this channel. This module does
//! not start that thread and does not open a client.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU32, Ordering, fence};
use std::sync::mpsc::{self, Receiver, Sender};

use omamix_core::db::{self, BelowMin};
use omamix_core::fader;
use omamix_core::params::StripParamUpdate;

/// What the UI or the headless CLI is allowed to enqueue.
///
/// The loop thread applies these. `process()` does not.
#[derive(Clone, Debug, PartialEq)]
pub enum EngineCommand {
    Shutdown,
    /// Master fader, in dB. Sanitized on the loop thread into [`AtomicParams`].
    SetMasterFaderDb(f32),
    /// Publish the monitor playback stream. Off unless the CLI asks.
    SetMonitorMix(bool),
    /// One strip field. The loop thread stores it in [`omamix_core::params::StripParamBank`].
    /// `process` does not read this command; it loads the atomics.
    SetStripParam {
        slot: u16,
        update: StripParamUpdate,
    },
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

    pub fn split(self) -> (CommandSender, Receiver<EngineCommand>) {
        (
            CommandSender {
                tx: self.tx.clone(),
            },
            self.rx,
        )
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

/// Fader and trim the realtime callback is allowed to read.
///
/// The loop thread writes. `process()` only loads. The default master gain is
/// unity: there is no built-in mic boost.
#[derive(Debug)]
pub struct AtomicParams {
    master_linear: AtomicU32,
}

impl AtomicParams {
    pub fn new() -> Self {
        Self {
            master_linear: AtomicU32::new(1.0f32.to_bits()),
        }
    }

    pub fn set_master_db(&self, db: f32) {
        let db = fader::sanitize_fader_db(db);
        let linear = db::db_to_linear(db);
        self.master_linear
            .store(linear.to_bits(), Ordering::Relaxed);
    }

    pub fn master_linear(&self) -> f32 {
        f32::from_bits(self.master_linear.load(Ordering::Relaxed))
    }
}

impl Default for AtomicParams {
    fn default() -> Self {
        Self::new()
    }
}

/// Latest mix quantum, written by the filter `process()` and read by a
/// headphone or monitor stream `process()`.
///
/// Not a second mixer. The send stream copies this slot into its buffer and
/// does not sum strips. A quantum longer than [`MIX_SLOT_CAP`] keeps the first
/// frames. The slot repeats or skips a block when the send's driver and the
/// filter's driver are not the same cycle.
pub const MIX_SLOT_CAP: usize = 2048;

pub struct MixSlot {
    seq: AtomicU32,
    frames: AtomicU32,
    left: UnsafeCell<[f32; MIX_SLOT_CAP]>,
    right: UnsafeCell<[f32; MIX_SLOT_CAP]>,
}

impl MixSlot {
    pub fn new() -> Self {
        Self {
            seq: AtomicU32::new(0),
            frames: AtomicU32::new(0),
            left: UnsafeCell::new([0.0; MIX_SLOT_CAP]),
            right: UnsafeCell::new([0.0; MIX_SLOT_CAP]),
        }
    }

    pub fn write(&self, left: &[f32], right: &[f32]) {
        let n = left.len().min(right.len()).min(MIX_SLOT_CAP);
        let seq = self.seq.load(Ordering::Relaxed);
        // Odd sequence: a reader must retry. The release fence publishes that
        // odd value before the sample stores.
        self.seq.store(seq.wrapping_add(1) | 1, Ordering::Relaxed);
        fence(Ordering::Release);
        unsafe {
            let dst_l = &mut *self.left.get();
            let dst_r = &mut *self.right.get();
            dst_l[..n].copy_from_slice(&left[..n]);
            dst_r[..n].copy_from_slice(&right[..n]);
        }
        self.frames.store(n as u32, Ordering::Relaxed);
        self.seq.store(seq.wrapping_add(2) & !1, Ordering::Release);
    }

    /// Copy up to `left.len()` frames. Returns how many were copied.
    pub fn read(&self, left: &mut [f32], right: &mut [f32]) -> usize {
        let cap = left.len().min(right.len());
        for _ in 0..8 {
            let start = self.seq.load(Ordering::Acquire);
            if start & 1 == 1 {
                continue;
            }
            let n = (self.frames.load(Ordering::Relaxed) as usize).min(cap);
            unsafe {
                let src_l = &*self.left.get();
                let src_r = &*self.right.get();
                left[..n].copy_from_slice(&src_l[..n]);
                right[..n].copy_from_slice(&src_r[..n]);
            }
            // Acquire the writer's release fence before trusting the second sequence.
            fence(Ordering::Acquire);
            let end = self.seq.load(Ordering::Relaxed);
            if start == end && end & 1 == 0 {
                return n;
            }
        }
        0
    }
}

impl Default for MixSlot {
    fn default() -> Self {
        Self::new()
    }
}

// The slot is a seqlock. Readers and the single writer do not use a mutex.
unsafe impl Sync for MixSlot {}

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

    #[test]
    fn params_default_to_unity_and_nan_master_is_silence() {
        let params = AtomicParams::new();
        assert_eq!(params.master_linear(), 1.0);
        params.set_master_db(f32::NAN);
        assert_eq!(params.master_linear(), 0.0);
        params.set_master_db(0.0);
        assert_eq!(params.master_linear(), 1.0);
    }

    #[test]
    fn mix_slot_roundtrip_keeps_the_quantum() {
        let slot = MixSlot::new();
        slot.write(&[0.25, -0.5], &[0.0, 1.0]);
        let mut left = [0.0; 4];
        let mut right = [0.0; 4];
        assert_eq!(slot.read(&mut left, &mut right), 2);
        assert_eq!(left[0], 0.25);
        assert_eq!(left[1], -0.5);
        assert_eq!(right[1], 1.0);
    }
}
