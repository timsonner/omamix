//! SPSC stereo rings for a later scope, spectrum, and loudness read.
//!
//! [`split`] hands out one [`ScopeProducer`], one [`ScopeConsumer`], and a
//! [`ScopeWatch`]. The producer and the consumer are `Send` and not shared:
//! `push` and `pop` take `&mut self`, and neither handle is `Clone`. The
//! watch is an atomic view any thread may hold, for discard and the
//! dropped-frame count. A full ring drops the frames that do not fit and
//! returns. It does not block, allocate, or take a lock.
//!
//! Slots are `AtomicU32` (`f32` bits, `Relaxed`). Only the consumer stores
//! `read`, including when it applies a discard. The producer never moves
//! `read`, so it does not overwrite a range the consumer is copying. A
//! discard makes [`ScopeConsumer::pop`] return 0 and leave the caller's
//! buffers unchanged. A pop that has already started copying finishes that
//! copy and returns the count.
//!
//! Capacity is a fixed slack for that reader, not a device period.

use std::cell::Cell;
use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

/// Frames kept per ring. Several blocks of slack, not a hardware quantum.
pub const SCOPE_FRAMES: usize = 4096;

struct Inner {
    left: Box<[AtomicU32]>,
    right: Box<[AtomicU32]>,
    mask: usize,
    /// Producer stores. Consumer loads.
    write: AtomicUsize,
    /// Consumer stores. Producer loads.
    read: AtomicUsize,
    discard: AtomicBool,
    dropped: AtomicU64,
}

impl Inner {
    fn new(frames: usize) -> Self {
        let cap = frames.max(1).next_power_of_two();
        Self {
            left: (0..cap).map(|_| AtomicU32::new(0)).collect(),
            right: (0..cap).map(|_| AtomicU32::new(0)).collect(),
            mask: cap - 1,
            write: AtomicUsize::new(0),
            read: AtomicUsize::new(0),
            discard: AtomicBool::new(false),
            dropped: AtomicU64::new(0),
        }
    }

    fn capacity(&self) -> usize {
        self.mask + 1
    }
}

/// The only writer. Not `Clone`. Not shared across threads.
pub struct ScopeProducer {
    inner: Arc<Inner>,
    pending: usize,
    pending_dropped: usize,
    _unshared: PhantomData<Cell<()>>,
}

/// The only reader. Not `Clone`. Not shared across threads.
pub struct ScopeConsumer {
    inner: Arc<Inner>,
    _unshared: PhantomData<Cell<()>>,
}

/// Atomic view any thread may hold. Discard and the dropped-frame count.
#[derive(Clone)]
pub struct ScopeWatch {
    inner: Arc<Inner>,
}

/// One producer, one consumer, and a watch that shares their counters.
pub fn split(frames: usize) -> (ScopeProducer, ScopeConsumer, ScopeWatch) {
    let inner = Arc::new(Inner::new(frames));
    (
        ScopeProducer {
            inner: Arc::clone(&inner),
            pending: 0,
            pending_dropped: 0,
            _unshared: PhantomData,
        },
        ScopeConsumer {
            inner: Arc::clone(&inner),
            _unshared: PhantomData,
        },
        ScopeWatch { inner },
    )
}

impl ScopeProducer {
    pub fn capacity(&self) -> usize {
        self.inner.capacity()
    }

    /// Frames this ring has dropped since it was created.
    pub fn dropped_frames(&self) -> u64 {
        self.inner.dropped.load(Ordering::Relaxed)
    }

    /// Keep up to `frames`. The rest are dropped. Does not publish yet.
    pub(crate) fn reserve(&mut self, frames: usize) -> usize {
        let write = self.inner.write.load(Ordering::Relaxed);
        let read = self.inner.read.load(Ordering::Acquire);
        let used = write.wrapping_sub(read);
        let free = if used < self.inner.capacity() {
            self.inner.capacity() - used
        } else {
            0
        };
        let kept = frames.min(free);
        self.pending = kept;
        self.pending_dropped = frames - kept;
        kept
    }

    pub(crate) fn write_at(&mut self, offset: usize, left: f32, right: f32) {
        if offset >= self.pending {
            return;
        }
        let write = self.inner.write.load(Ordering::Relaxed);
        let index = write.wrapping_add(offset) & self.inner.mask;
        self.inner.left[index].store(finite_sample(left).to_bits(), Ordering::Relaxed);
        self.inner.right[index].store(finite_sample(right).to_bits(), Ordering::Relaxed);
    }

    pub(crate) fn commit(&mut self) {
        let kept = self.pending;
        let dropped = self.pending_dropped;
        self.pending = 0;
        self.pending_dropped = 0;
        if dropped > 0 {
            self.inner
                .dropped
                .fetch_add(dropped as u64, Ordering::Relaxed);
        }
        if kept == 0 {
            return;
        }
        let write = self.inner.write.load(Ordering::Relaxed);
        self.inner
            .write
            .store(write.wrapping_add(kept), Ordering::Release);
    }

    /// Push stereo frames. Returns how many were dropped. Never blocks.
    pub fn push(&mut self, left: &[f32], right: &[f32]) -> usize {
        let frames = left.len().min(right.len());
        let kept = self.reserve(frames);
        for index in 0..kept {
            self.write_at(index, left[index], right[index]);
        }
        self.commit();
        frames - kept
    }
}

impl ScopeConsumer {
    pub fn dropped_frames(&self) -> u64 {
        self.inner.dropped.load(Ordering::Relaxed)
    }

    /// Pop up to the buffer length.
    ///
    /// A pending discard moves `read` to `write` and returns 0. The caller's
    /// buffers are left unchanged.
    pub fn pop(&mut self, left: &mut [f32], right: &mut [f32]) -> usize {
        if self.inner.discard.swap(false, Ordering::AcqRel) {
            self.snap_read();
            return 0;
        }
        let read = self.inner.read.load(Ordering::Relaxed);
        let write = self.inner.write.load(Ordering::Acquire);
        let available = write.wrapping_sub(read);
        if available > self.inner.capacity() {
            return 0;
        }
        let count = available.min(left.len()).min(right.len());
        if count == 0 {
            return 0;
        }
        for index in 0..count {
            let slot = read.wrapping_add(index) & self.inner.mask;
            left[index] = f32::from_bits(self.inner.left[slot].load(Ordering::Relaxed));
            right[index] = f32::from_bits(self.inner.right[slot].load(Ordering::Relaxed));
        }
        self.inner
            .read
            .store(read.wrapping_add(count), Ordering::Release);
        count
    }

    fn snap_read(&self) {
        let write = self.inner.write.load(Ordering::Acquire);
        self.inner.read.store(write, Ordering::Release);
    }
}

impl ScopeWatch {
    pub fn request_discard(&self) {
        self.inner.discard.store(true, Ordering::Release);
    }

    pub fn dropped_frames(&self) -> u64 {
        self.inner.dropped.load(Ordering::Relaxed)
    }
}

fn finite_sample(sample: f32) -> f32 {
    if sample.is_finite() { sample } else { 0.0 }
}

/// Per-strip writers. Slot `n` matches the tap slot.
pub struct ScopeBank {
    slots: [ScopeProducer; crate::params::STRIP_SLOT_COUNT],
}

/// Per-strip readers. Paired with a [`ScopeBank`] from [`ScopeBank::open`].
pub struct ScopeReadBank {
    slots: [ScopeConsumer; crate::params::STRIP_SLOT_COUNT],
}

impl ScopeBank {
    pub fn new() -> Self {
        Self::with_capacity(SCOPE_FRAMES)
    }

    /// Writer half only. The reader is dropped. Fine when nothing pops.
    pub fn with_capacity(frames: usize) -> Self {
        let (writers, _readers, _watches) = Self::open(frames);
        writers
    }

    pub fn open(
        frames: usize,
    ) -> (
        Self,
        ScopeReadBank,
        [ScopeWatch; crate::params::STRIP_SLOT_COUNT],
    ) {
        let mut producers = Vec::with_capacity(crate::params::STRIP_SLOT_COUNT);
        let mut consumers = Vec::with_capacity(crate::params::STRIP_SLOT_COUNT);
        let mut watches = Vec::with_capacity(crate::params::STRIP_SLOT_COUNT);
        for _ in 0..crate::params::STRIP_SLOT_COUNT {
            let (producer, consumer, watch) = split(frames);
            producers.push(producer);
            consumers.push(consumer);
            watches.push(watch);
        }
        let slots: [ScopeProducer; crate::params::STRIP_SLOT_COUNT] =
            producers.try_into().ok().expect("strip count");
        let readers: [ScopeConsumer; crate::params::STRIP_SLOT_COUNT] =
            consumers.try_into().ok().expect("strip count");
        let watch_slots: [ScopeWatch; crate::params::STRIP_SLOT_COUNT] =
            watches.try_into().ok().expect("strip count");
        (
            Self { slots },
            ScopeReadBank { slots: readers },
            watch_slots,
        )
    }

    pub(crate) fn reserve(&mut self, slot: usize, frames: usize) -> usize {
        self.slots
            .get_mut(slot)
            .map(|ring| ring.reserve(frames))
            .unwrap_or(0)
    }

    pub(crate) fn write_at(&mut self, slot: usize, offset: usize, left: f32, right: f32) {
        if let Some(ring) = self.slots.get_mut(slot) {
            ring.write_at(offset, left, right);
        }
    }

    pub(crate) fn commit(&mut self, slot: usize) {
        if let Some(ring) = self.slots.get_mut(slot) {
            ring.commit();
        }
    }

    pub fn push(&mut self, slot: usize, left: &[f32], right: &[f32]) -> usize {
        self.slots
            .get_mut(slot)
            .map(|ring| ring.push(left, right))
            .unwrap_or(left.len().min(right.len()))
    }

    pub fn dropped_frames(&self, slot: usize) -> u64 {
        self.slots
            .get(slot)
            .map(ScopeProducer::dropped_frames)
            .unwrap_or(0)
    }
}

impl Default for ScopeBank {
    fn default() -> Self {
        Self::new()
    }
}

impl ScopeReadBank {
    pub fn pop(&mut self, slot: usize, left: &mut [f32], right: &mut [f32]) -> usize {
        self.slots
            .get_mut(slot)
            .map(|ring| ring.pop(left, right))
            .unwrap_or(0)
    }

    pub fn dropped_frames(&self, slot: usize) -> u64 {
        self.slots
            .get(slot)
            .map(ScopeConsumer::dropped_frames)
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;
    use std::thread;

    use super::*;

    #[test]
    fn push_then_pop_roundtrips_and_partial_overflow_drops_the_tail() {
        let (mut ring, mut read, watch) = split(4);
        assert_eq!(ring.capacity(), 4);
        let first = [0.25f32, -0.5, f32::NAN];
        assert_eq!(ring.push(&first, &first), 0);
        let second = [1.0f32, 2.0, 3.0];
        assert_eq!(ring.push(&second, &second), 2);
        assert_eq!(watch.dropped_frames(), 2);
        assert_eq!(read.dropped_frames(), 2);
        let mut left = [0.0f32; 4];
        let mut right = [0.0f32; 4];
        assert_eq!(read.pop(&mut left, &mut right), 4);
        assert_eq!(left[0], 0.25);
        assert_eq!(left[1], -0.5);
        assert_eq!(left[2], 0.0);
        assert_eq!(left[3], 1.0);
        let mut left = [7.0f32; 4];
        let mut right = [7.0f32; 4];
        assert_eq!(read.pop(&mut left, &mut right), 0);
        assert_eq!(left, [7.0; 4]);
        assert_eq!(right, [7.0; 4]);
    }

    #[test]
    fn a_stalled_reader_does_not_block_the_writer() {
        let (mut ring, mut read, watch) = split(8);
        let first: Vec<f32> = (0..8).map(|index| index as f32 + 1.0).collect();
        assert_eq!(ring.push(&first, &first), 0);
        let extra = [99.0f32; 8];
        for _ in 0..1_000 {
            assert_eq!(ring.push(&extra, &extra), 8);
        }
        assert_eq!(watch.dropped_frames(), 8_000);
        let mut left = [0.0f32; 8];
        let mut right = [0.0f32; 8];
        assert_eq!(read.pop(&mut left, &mut right), 8);
        assert_eq!(left, first.as_slice());
        assert_eq!(right, first.as_slice());
        let mut left = [7.0f32; 8];
        assert_eq!(read.pop(&mut left, &mut right), 0);
        assert_eq!(left, [7.0; 8]);
    }

    #[test]
    fn discard_hides_unread_frames_until_the_next_push() {
        let (mut ring, mut read, watch) = split(4);
        let frame = [0.5f32, -0.25];
        assert_eq!(ring.push(&frame, &frame), 0);
        watch.request_discard();
        let mut left = [1.0f32; 2];
        let mut right = [1.0f32; 2];
        assert_eq!(read.pop(&mut left, &mut right), 0);
        assert_eq!(left, [1.0; 2]);
        assert_eq!(right, [1.0; 2]);
        assert_eq!(ring.push(&[0.125], &[0.125]), 0);
        assert_eq!(read.pop(&mut left, &mut right), 1);
        assert_eq!(left[0], 0.125);
        assert_eq!(right[0], 0.125);
    }

    #[test]
    fn concurrent_discard_does_not_tear_a_pop() {
        let (mut ring, mut read, watch) = split(8);
        assert_eq!(ring.push(&[1.0; 8], &[1.0; 8]), 0);
        let stop = Arc::new(AtomicBool::new(false));
        let discarder = {
            let watch = watch.clone();
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    watch.request_discard();
                }
            })
        };
        let producer = thread::spawn(move || {
            for _ in 0..2_000 {
                let _ = ring.push(&[2.0; 8], &[2.0; 8]);
            }
        });
        for _ in 0..2_000 {
            let mut left = [42.0f32; 8];
            let mut right = [42.0f32; 8];
            let count = read.pop(&mut left, &mut right);
            if count == 0 {
                assert_eq!(left, [42.0; 8]);
                assert_eq!(right, [42.0; 8]);
            } else {
                for index in 0..count {
                    assert!(left[index] == 1.0 || left[index] == 2.0, "{}", left[index]);
                    assert_eq!(left[index], right[index]);
                }
            }
        }
        stop.store(true, Ordering::Relaxed);
        discarder.join().unwrap();
        producer.join().unwrap();
    }
}
