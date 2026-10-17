// SPDX-License-Identifier: MPL-2.0

//! Capacity-bounded memory backend for persistent filesystem state.

#![expect(
    dead_code,
    reason = "Task 7-8 interfaces are consumed by later NVPool tasks"
)]

use spin::Once;

pub(crate) mod backend;
pub(crate) mod capacity;
pub(crate) mod config;
pub(crate) mod data_page;
pub(crate) mod dram;
pub(crate) mod fault;
pub(crate) mod frame;
pub(crate) mod id;
pub(crate) mod layout;
pub(crate) mod persist;
pub(crate) mod reclaim;
pub(crate) mod recover;

use self::{config::NvPoolConfig, dram::DramNvBackend, frame::NvPool};
use crate::prelude::*;

static NV_POOL: Once<Arc<NvPool>> = Once::new();

pub(crate) fn pool() -> Option<&'static Arc<NvPool>> {
    NV_POOL.get()
}

pub(super) fn init() {
    let Some(config) = NvPoolConfig::from_command_line()
        .unwrap_or_else(|error| panic!("invalid NVPool configuration: {:?}", error))
    else {
        return;
    };

    let total_pages = config.total_pages();
    let mode = config.mode();
    NV_POOL.call_once(|| {
        let backend: Arc<dyn backend::NvMemoryBackend> = Arc::new(
            DramNvBackend::allocate(config)
                .unwrap_or_else(|error| panic!("failed to allocate NVPool: {:?}", error)),
        );
        Arc::new(
            NvPool::format(backend, config, None)
                .unwrap_or_else(|error| panic!("failed to initialize NVPool: {:?}", error)),
        )
    });
    ostd::info!(
        "initialized DRAM-backed NVPool with {} pages ({} MiB), mode {:?}",
        total_pages,
        total_pages * PAGE_SIZE / (1024 * 1024),
        mode
    );
}

#[cfg(ktest)]
mod tests;
