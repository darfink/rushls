//! Thread-local allocation measurements for hot-path regression tests.
//!
//! The allocator is process-wide, but counting is opt-in on the calling
//! thread. Allocation tests must therefore use a current-thread executor and
//! avoid running unrelated tasks while the measured future is enabled.

use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
    future::Future,
};

thread_local! {
    /// `None` outside a measurement, otherwise the current allocation count.
    static ALLOCATIONS: Cell<Option<usize>> = const { Cell::new(None) };
}

struct CountingAllocator;

// SAFETY: every operation delegates to `System` with its original arguments;
// the thread-local side effect neither retains nor dereferences allocations.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record();
        // SAFETY: delegated under the caller's `GlobalAlloc` contract.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: delegated under the caller's `GlobalAlloc` contract.
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record();
        // SAFETY: delegated under the caller's `GlobalAlloc` contract.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        record();
        // SAFETY: delegated under the caller's `GlobalAlloc` contract.
        unsafe { System.realloc(pointer, layout, size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn record() {
    let _ = ALLOCATIONS.try_with(|count| {
        if let Some(current) = count.get() {
            count.set(Some(current.saturating_add(1)));
        }
    });
}

struct Measurement;

impl Measurement {
    fn begin() -> Self {
        ALLOCATIONS.with(|count| {
            assert_eq!(
                count.replace(Some(0)),
                None,
                "allocation measurements cannot nest"
            );
        });
        Self
    }

    fn finish(self) -> usize {
        let count = ALLOCATIONS.with(|count| count.replace(None).expect("measurement is active"));
        drop(self);
        count
    }
}

impl Drop for Measurement {
    fn drop(&mut self) {
        ALLOCATIONS.with(|count| count.set(None));
    }
}

pub fn count<T>(operation: impl FnOnce() -> T) -> (T, usize) {
    let measurement = Measurement::begin();
    let output = operation();
    let count = measurement.finish();
    (output, count)
}

pub async fn count_async<F: Future>(future: F) -> (F::Output, usize) {
    let measurement = Measurement::begin();
    let output = future.await;
    let count = measurement.finish();
    (output, count)
}
