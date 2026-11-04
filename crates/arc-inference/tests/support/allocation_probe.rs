// Test-binary-only allocator. Refuses one allocation on the calling thread and tracks
// previously allocated pointers in a fixed stack-sized table. Never exhausts memory;
// unrelated test threads and all allocations outside `refuse` use System unchanged.
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

#[derive(Clone, Copy)]
struct State {
    active: bool,
    fail_at: usize,
    attempts: usize,
    pointers: [usize; 32],
    tracking_overflow: bool,
}

const IDLE: State = State {
    active: false,
    fail_at: 0,
    attempts: 0,
    pointers: [0; 32],
    tracking_overflow: false,
};
thread_local! { static STATE: Cell<State> = const { Cell::new(IDLE) }; }

struct ProbeAllocator;
#[global_allocator]
static ALLOCATOR: ProbeAllocator = ProbeAllocator;

// SAFETY: every non-refused allocation and every deallocation is delegated to System
// with the original layout. Bookkeeping uses TLS with no allocation and never dereferences
// the recorded addresses. Null refusal obeys GlobalAlloc's allocation-failure contract.
unsafe impl GlobalAlloc for ProbeAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let refuse = STATE
            .try_with(|cell| {
                let mut state = cell.get();
                if !state.active {
                    return false;
                }
                let refuse = state.attempts == state.fail_at;
                state.attempts += 1;
                cell.set(state);
                refuse
            })
            .unwrap_or(false);
        if refuse {
            return std::ptr::null_mut();
        }
        // SAFETY: the caller supplied a valid allocation layout.
        let ptr = unsafe { System.alloc(layout) };
        let _ = STATE.try_with(|cell| {
            let mut state = cell.get();
            if state.active && !ptr.is_null() {
                if let Some(slot) = state.pointers.iter_mut().find(|p| **p == 0) {
                    *slot = ptr as usize;
                } else {
                    state.tracking_overflow = true;
                }
                cell.set(state);
            }
        });
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let _ = STATE.try_with(|cell| {
            let mut state = cell.get();
            if state.active {
                for slot in &mut state.pointers {
                    if *slot == ptr as usize {
                        *slot = 0;
                    }
                }
                cell.set(state);
            }
        });
        // SAFETY: allocations are delegated to System, which owns this pointer/layout.
        unsafe {
            System.dealloc(ptr, layout);
        }
    }
}

#[derive(Debug)]
pub struct Observation {
    pub attempts: usize,
    pub live_allocations: usize,
}

/// Only use with synchronous, allocation-minimal reservation calls. Disable before
/// formatting/asserting/submitting. Reset on unwind as well as successful return.
pub fn refuse<T>(fail_at: usize, call: impl FnOnce() -> T) -> (T, Observation) {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            STATE.with(|cell| cell.set(IDLE));
        }
    }
    STATE.with(|cell| {
        assert!(!cell.get().active, "nested allocation probe");
        cell.set(State {
            active: true,
            fail_at,
            ..IDLE
        });
    });
    let reset = Reset;
    let result = call();
    let state = STATE.with(Cell::get);
    drop(reset);
    assert!(
        !state.tracking_overflow,
        "allocation probe tracking capacity exceeded"
    );
    (
        result,
        Observation {
            attempts: state.attempts,
            live_allocations: state.pointers.iter().filter(|p| **p != 0).count(),
        },
    )
}
