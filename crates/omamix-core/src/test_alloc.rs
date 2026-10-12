//! Counting allocator for the realtime no-alloc test.
//!
//! The count lives in a `thread_local` cell. Two tests on different threads
//! do not share it, so `process_allocates_nothing` cannot race with
//! `counting_allocator_sees_a_deliberate_allocation`. `arm` touches the cell
//! before the counted section, so the first access is not inside `alloc`.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

#[derive(Clone, Copy)]
struct AllocTrack {
    armed: bool,
    /// Set while `note` updates the cell, so a re-entrant alloc does not recurse.
    busy: bool,
    count: u32,
}

thread_local! {
    static TRACK: Cell<AllocTrack> = const {
        Cell::new(AllocTrack {
            armed: false,
            busy: false,
            count: 0,
        })
    };
}

fn note() {
    let Ok(state) = TRACK.try_with(|track| {
        let state = track.get();
        if state.busy || !state.armed {
            return None;
        }
        track.set(AllocTrack {
            busy: true,
            ..state
        });
        Some(state)
    }) else {
        return;
    };
    let Some(state) = state else {
        return;
    };
    let _ = TRACK.try_with(|track| {
        track.set(AllocTrack {
            armed: state.armed,
            busy: false,
            count: state.count.wrapping_add(1),
        });
    });
}

struct CountingAlloc;

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note();
        unsafe { System.alloc(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note();
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc;

/// Start counting allocations on this thread. The count starts at zero.
pub fn arm() {
    TRACK.with(|track| {
        track.set(AllocTrack {
            armed: true,
            busy: false,
            count: 0,
        });
    });
}

/// Stop counting and return how many allocations this thread made since [`arm`].
pub fn disarm() -> usize {
    TRACK.with(|track| {
        let count = track.get().count;
        track.set(AllocTrack {
            armed: false,
            busy: false,
            count: 0,
        });
        count as usize
    })
}
