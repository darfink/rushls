//! Opt-in process allocator measurements for instrumented soak binaries.
//! Requested Rust heap bytes exclude allocator overhead and native libraries.

use std::{
    alloc::{GlobalAlloc, Layout, System},
    sync::atomic::{AtomicU64, Ordering::Relaxed},
};

static CALLS: AtomicU64 = AtomicU64::new(0);
static ALLOCATED: AtomicU64 = AtomicU64::new(0);
static FREED: AtomicU64 = AtomicU64::new(0);

pub struct CountingAllocator;

fn allocated(bytes: usize) {
    CALLS.fetch_add(1, Relaxed);
    ALLOCATED.fetch_add(bytes as u64, Relaxed);
}

// SAFETY: pointers and layouts pass unchanged to System. Accounting uses only
// atomics, never allocates, and records successful operations only.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller supplies a valid allocation layout.
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            allocated(layout.size());
        }
        pointer
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller supplies a valid allocation layout.
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if !pointer.is_null() {
            allocated(layout.size());
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: the caller supplies the original allocation and layout.
        unsafe { System.dealloc(pointer, layout) };
        FREED.fetch_add(layout.size() as u64, Relaxed);
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        // SAFETY: the caller supplies a valid allocation and replacement size.
        let replacement = unsafe { System.realloc(pointer, layout, size) };
        if !replacement.is_null() {
            allocated(size);
            FREED.fetch_add(layout.size() as u64, Relaxed);
        }
        replacement
    }
}

/// Separate cumulative counters avoid claiming an atomic live-heap snapshot.
pub fn snapshot() -> (u64, u64, u64) {
    (
        CALLS.load(Relaxed),
        ALLOCATED.load(Relaxed),
        FREED.load(Relaxed),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_successful_allocations_and_replacement_bytes()
    -> Result<(), Box<dyn std::error::Error>> {
        let before = snapshot();
        let original = Layout::from_size_align(16, 8)?;
        let grown = Layout::from_size_align(32, 8)?;
        let zeroed = Layout::from_size_align(24, 8)?;
        // SAFETY: layouts are valid, returned pointers are checked before use,
        // and every allocation is freed once with its corresponding layout.
        unsafe {
            let pointer = CountingAllocator.alloc(original);
            assert!(!pointer.is_null());
            pointer.write(42);
            let replacement = CountingAllocator.realloc(pointer, original, grown.size());
            assert!(!replacement.is_null());
            assert_eq!(replacement.read(), 42);
            CountingAllocator.dealloc(replacement, grown);
            let pointer = CountingAllocator.alloc_zeroed(zeroed);
            assert!(!pointer.is_null());
            assert!(
                std::slice::from_raw_parts(pointer, zeroed.size())
                    .iter()
                    .all(|byte| *byte == 0)
            );
            CountingAllocator.dealloc(pointer, zeroed);
        }
        let after = snapshot();
        assert_eq!(after.0 - before.0, 3);
        assert_eq!(after.1 - before.1, 72);
        assert_eq!(after.2 - before.2, 72);
        Ok(())
    }
}
