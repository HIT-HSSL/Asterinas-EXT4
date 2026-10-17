// SPDX-License-Identifier: MPL-2.0

//! Capacity accounting and pressure policy for NVPool objects.

use super::config::NvPoolConfig;
use crate::prelude::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PoolClass {
    Metadata,
    Data,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct NvPoolStats {
    pub(crate) total_pages: usize,
    pub(crate) free_pages: usize,
    pub(crate) metadata_pages: usize,
    pub(crate) data_pages: usize,
    pub(crate) pinned_frames: usize,
    pub(crate) high_water_events: u64,
    pub(crate) backpressure_events: u64,
}

pub(super) struct CapacityState {
    total_pages: usize,
    data_limit_pages: usize,
    high_water_pages: usize,
    metadata_pages: usize,
    data_pages: usize,
    pinned_frames: usize,
    above_high_water: bool,
    high_water_events: u64,
    backpressure_events: u64,
}

impl CapacityState {
    pub(super) fn new(config: NvPoolConfig) -> Self {
        let total_pages = config.total_pages();
        let data_limit_pages = config.data_limit_pages();
        let total_high_water_pages = (total_pages * 3).div_ceil(4);
        let high_water_pages = if data_limit_pages == 0 {
            total_high_water_pages
        } else {
            total_high_water_pages.min(data_limit_pages)
        };
        Self {
            total_pages,
            data_limit_pages,
            high_water_pages,
            metadata_pages: 0,
            data_pages: 0,
            pinned_frames: 0,
            above_high_water: false,
            high_water_events: 0,
            backpressure_events: 0,
        }
    }

    pub(super) fn before_allocate(&mut self, class: PoolClass) -> Result<()> {
        if self.used_pages() == self.total_pages {
            return_errno_with_message!(Errno::ENOSPC, "NVPool capacity is exhausted");
        }
        if class == PoolClass::Data && self.data_pages == self.data_limit_pages {
            self.backpressure_events += 1;
            return_errno_with_message!(Errno::EAGAIN, "NVPool data hard limit reached");
        }
        Ok(())
    }

    pub(super) fn account_allocate(&mut self, class: PoolClass) -> bool {
        match class {
            PoolClass::Metadata => self.metadata_pages += 1,
            PoolClass::Data => self.data_pages += 1,
        }

        if !self.above_high_water && self.used_pages() >= self.high_water_pages {
            self.above_high_water = true;
            self.high_water_events += 1;
            true
        } else {
            false
        }
    }

    pub(super) fn account_recovered(&mut self, class: PoolClass) -> Result<()> {
        self.before_allocate(class)?;
        let _ = self.account_allocate(class);
        Ok(())
    }

    pub(super) fn account_free(&mut self, class: PoolClass) {
        match class {
            PoolClass::Metadata => self.metadata_pages -= 1,
            PoolClass::Data => self.data_pages -= 1,
        }
        if self.used_pages() < self.high_water_pages {
            self.above_high_water = false;
        }
    }

    pub(super) fn account_pin(&mut self) -> Result<()> {
        self.pinned_frames = self
            .pinned_frames
            .checked_add(1)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        Ok(())
    }

    pub(super) fn account_unpin(&mut self) {
        self.pinned_frames -= 1;
    }

    pub(super) fn stats(&self) -> NvPoolStats {
        NvPoolStats {
            total_pages: self.total_pages,
            free_pages: self.total_pages - self.used_pages(),
            metadata_pages: self.metadata_pages,
            data_pages: self.data_pages,
            pinned_frames: self.pinned_frames,
            high_water_events: self.high_water_events,
            backpressure_events: self.backpressure_events,
        }
    }

    fn used_pages(&self) -> usize {
        self.metadata_pages + self.data_pages
    }
}
