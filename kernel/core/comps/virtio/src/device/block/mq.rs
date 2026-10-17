// SPDX-License-Identifier: MPL-2.0

use core::sync::atomic::{AtomicUsize, Ordering};

use crate::device::VirtioDeviceError;

pub(super) const DRIVER_MAX_QUEUES: u16 = 64;

pub(super) fn effective_queue_count(
    mq: bool,
    config_queues: u16,
    transport_queues: u16,
    cpu_count: usize,
) -> Result<u16, VirtioDeviceError> {
    if cpu_count == 0 || (mq && (config_queues == 0 || transport_queues == 0)) {
        return Err(VirtioDeviceError::InvalidQueueArgs);
    }

    if !mq {
        return Ok(1);
    }

    Ok(config_queues
        .min(transport_queues)
        .min(DRIVER_MAX_QUEUES)
        .min(cpu_count.min(usize::from(u16::MAX)) as u16))
}

#[derive(Debug)]
pub(super) struct QueueRouter {
    next: AtomicUsize,
    queue_count: usize,
}

impl QueueRouter {
    pub(super) fn new(queue_count: usize) -> Self {
        assert!(queue_count > 0);
        Self {
            next: AtomicUsize::new(0),
            queue_count,
        }
    }

    pub(super) fn select(&self) -> usize {
        self.next.fetch_add(1, Ordering::Relaxed) % self.queue_count
    }
}

#[cfg(ktest)]
mod tests {
    use ostd::prelude::ktest;

    use super::{QueueRouter, effective_queue_count};
    use crate::device::VirtioDeviceError;

    #[ktest]
    fn mq_disabled_uses_one_queue() {
        assert_eq!(effective_queue_count(false, 8, 8, 8).unwrap(), 1);
    }

    #[ktest]
    fn mq_enabled_limits_queue_count_to_cpu_count() {
        assert_eq!(effective_queue_count(true, 8, 8, 4).unwrap(), 4);
    }

    #[ktest]
    fn mq_enabled_limits_queue_count_to_driver_cap() {
        assert_eq!(effective_queue_count(true, 128, 128, 128).unwrap(), 64);
    }

    #[ktest]
    fn mq_enabled_rejects_zero_config_queue_count() {
        assert!(matches!(
            effective_queue_count(true, 0, 8, 8),
            Err(VirtioDeviceError::InvalidQueueArgs)
        ));
    }

    #[ktest]
    fn mq_enabled_rejects_zero_transport_queue_count() {
        assert!(matches!(
            effective_queue_count(true, 8, 0, 8),
            Err(VirtioDeviceError::InvalidQueueArgs)
        ));
    }

    #[ktest]
    fn zero_cpu_count_is_rejected() {
        assert!(matches!(
            effective_queue_count(true, 8, 8, 0),
            Err(VirtioDeviceError::InvalidQueueArgs)
        ));
    }

    #[ktest]
    fn router_cycles_through_available_queues() {
        let router = QueueRouter::new(4);
        let selected: [usize; 10] = core::array::from_fn(|_| router.select());

        assert_eq!(selected, [0, 1, 2, 3, 0, 1, 2, 3, 0, 1]);
    }
}
