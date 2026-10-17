// SPDX-License-Identifier: MPL-2.0

//! Storage-independent access to NVPool memory.

use crate::{fs::nv_pool::id::NvPageId, prelude::*};

pub(crate) trait NvMemoryBackend: Send + Sync {
    fn capacity_pages(&self) -> usize;

    fn control_len(&self) -> usize;

    fn read_page(&self, id: NvPageId, output: &mut [u8]) -> Result<()>;

    fn write_page(&self, id: NvPageId, input: &[u8]) -> Result<()>;

    fn read_control(&self, offset: usize, output: &mut [u8]) -> Result<()>;

    fn write_control(&self, offset: usize, input: &[u8]) -> Result<()>;

    fn load_control_u64(&self, offset: usize) -> Result<u64>;

    fn store_control_u64(&self, offset: usize, value: u64) -> Result<()>;

    fn persist(&self, offset: usize, len: usize) -> Result<()>;

    fn persist_control(&self, offset: usize, len: usize) -> Result<()>;

    fn fence(&self) -> Result<()>;
}
