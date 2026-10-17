// SPDX-License-Identifier: MPL-2.0

use ostd::sync::{Mutex, WaitQueue};

#[derive(Debug, Default)]
struct FlushState {
    active: bool,
    outstanding_writes: usize,
}

pub(super) struct FlushGate {
    state: Mutex<FlushState>,
    wait_queue: WaitQueue,
}

impl FlushGate {
    pub(super) fn new() -> Self {
        Self {
            state: Mutex::new(FlushState::default()),
            wait_queue: WaitQueue::new(),
        }
    }

    pub(super) fn admit_write(&self) {
        self.wait_queue.wait_until(|| {
            let mut state = self.state.lock();
            if state.active {
                return None;
            }
            state.outstanding_writes += 1;
            Some(())
        });
    }

    pub(super) fn complete_writes(&self, count: usize) {
        let mut state = self.state.lock();
        assert!(state.outstanding_writes >= count);
        state.outstanding_writes -= count;
        let wake = state.outstanding_writes == 0;
        drop(state);
        if wake {
            self.wait_queue.wake_all();
        }
    }

    pub(super) fn begin_flush(&self) {
        self.wait_queue.wait_until(|| {
            let mut state = self.state.lock();
            if state.active {
                return None;
            }
            state.active = true;
            Some(())
        });
        self.wait_queue
            .wait_until(|| (self.state.lock().outstanding_writes == 0).then_some(()));
    }

    pub(super) fn complete_flush(&self) {
        let mut state = self.state.lock();
        assert!(state.active);
        state.active = false;
        drop(state);
        self.wait_queue.wake_all();
    }
}

impl core::fmt::Debug for FlushGate {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("FlushGate")
            .field("state", &*self.state.lock())
            .finish_non_exhaustive()
    }
}
