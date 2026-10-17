// SPDX-License-Identifier: MPL-2.0

//! DRAM-backed implementation of NVPool memory.

use core::sync::atomic::{AtomicU64, Ordering, compiler_fence};

use ostd::mm::{FrameAllocOptions, HasSize, Segment, VmIo, VmIoOnce};

use super::{
    backend::NvMemoryBackend,
    config::{NvPoolConfig, NvSimMode},
    id::NvPageId,
    layout::control_len,
};
use crate::prelude::*;

const DEFAULT_CHUNK_PAGES: usize = 512;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct DramNvBackendStats {
    pub(crate) persist_calls: u64,
    pub(crate) fence_calls: u64,
}

struct ImageBytes {
    chunks: Vec<Segment<()>>,
    control_chunks: Vec<Segment<()>>,
}

impl ImageBytes {
    fn allocate(capacity_pages: usize, chunk_pages: usize, control_bytes: usize) -> Result<Self> {
        let chunk_count = capacity_pages.div_ceil(chunk_pages);
        let mut chunks = Vec::new();
        chunks.try_reserve_exact(chunk_count).map_err(|_| {
            Error::with_message(Errno::ENOMEM, "NVPool chunk table allocation failed")
        })?;
        let mut remaining = capacity_pages;
        while remaining > 0 {
            let pages = remaining.min(chunk_pages);
            chunks.push(FrameAllocOptions::new().alloc_segment(pages)?);
            remaining -= pages;
        }
        let mut control_chunks = Vec::new();
        let mut remaining = control_bytes.div_ceil(PAGE_SIZE);
        while remaining > 0 {
            let pages = remaining.min(chunk_pages);
            control_chunks.push(FrameAllocOptions::new().alloc_segment(pages)?);
            remaining -= pages;
        }
        Ok(Self {
            chunks,
            control_chunks,
        })
    }

    fn read_data(&self, chunk_pages: usize, offset: usize, output: &mut [u8]) -> Result<()> {
        let mut copied = 0;
        while copied < output.len() {
            let absolute = offset + copied;
            let chunk_index = absolute / (chunk_pages * PAGE_SIZE);
            let chunk_offset = absolute % (chunk_pages * PAGE_SIZE);
            let amount =
                (output.len() - copied).min(self.chunks[chunk_index].size() - chunk_offset);
            self.chunks[chunk_index]
                .read_bytes(chunk_offset, &mut output[copied..copied + amount])?;
            copied += amount;
        }
        Ok(())
    }

    fn write_data(&self, chunk_pages: usize, offset: usize, input: &[u8]) -> Result<()> {
        let mut copied = 0;
        while copied < input.len() {
            let absolute = offset + copied;
            let chunk_index = absolute / (chunk_pages * PAGE_SIZE);
            let chunk_offset = absolute % (chunk_pages * PAGE_SIZE);
            let amount = (input.len() - copied).min(self.chunks[chunk_index].size() - chunk_offset);
            self.chunks[chunk_index].write_bytes(chunk_offset, &input[copied..copied + amount])?;
            copied += amount;
        }
        Ok(())
    }

    fn read_control(&self, chunk_pages: usize, offset: usize, output: &mut [u8]) -> Result<()> {
        read_chunks(&self.control_chunks, chunk_pages, offset, output)
    }

    fn write_control(&self, chunk_pages: usize, offset: usize, input: &[u8]) -> Result<()> {
        write_chunks(&self.control_chunks, chunk_pages, offset, input)
    }

    fn read_control_once(&self, chunk_pages: usize, offset: usize) -> Result<u64> {
        let chunk_bytes = chunk_pages * PAGE_SIZE;
        self.control_chunks[offset / chunk_bytes]
            .read_once(offset % chunk_bytes)
            .map_err(Into::into)
    }

    fn write_control_once(&self, chunk_pages: usize, offset: usize, value: u64) -> Result<()> {
        let chunk_bytes = chunk_pages * PAGE_SIZE;
        self.control_chunks[offset / chunk_bytes]
            .write_once(offset % chunk_bytes, &value)
            .map_err(Into::into)
    }
}

fn read_chunks(
    chunks: &[Segment<()>],
    chunk_pages: usize,
    offset: usize,
    output: &mut [u8],
) -> Result<()> {
    let mut copied = 0;
    while copied < output.len() {
        let absolute = offset + copied;
        let chunk_index = absolute / (chunk_pages * PAGE_SIZE);
        let chunk_offset = absolute % (chunk_pages * PAGE_SIZE);
        let amount = (output.len() - copied).min(chunks[chunk_index].size() - chunk_offset);
        chunks[chunk_index].read_bytes(chunk_offset, &mut output[copied..copied + amount])?;
        copied += amount;
    }
    Ok(())
}

fn write_chunks(
    chunks: &[Segment<()>],
    chunk_pages: usize,
    offset: usize,
    input: &[u8],
) -> Result<()> {
    let mut copied = 0;
    while copied < input.len() {
        let absolute = offset + copied;
        let chunk_index = absolute / (chunk_pages * PAGE_SIZE);
        let chunk_offset = absolute % (chunk_pages * PAGE_SIZE);
        let amount = (input.len() - copied).min(chunks[chunk_index].size() - chunk_offset);
        chunks[chunk_index].write_bytes(chunk_offset, &input[copied..copied + amount])?;
        copied += amount;
    }
    Ok(())
}

struct DramNvImageInner {
    working: ImageBytes,
    durable: Option<ImageBytes>,
    chunk_pages: usize,
    capacity_pages: usize,
    control_bytes: usize,
    epoch: AtomicU64,
    persist_calls: AtomicU64,
    fence_calls: AtomicU64,
    io_lock: SpinLock<()>,
}

#[derive(Clone)]
pub(crate) struct DramNvImage {
    inner: Arc<DramNvImageInner>,
}

impl DramNvImage {
    fn allocate_inner(config: NvPoolConfig, chunk_pages: usize, model: bool) -> Result<Self> {
        if chunk_pages == 0 {
            return_errno_with_message!(Errno::EINVAL, "NVPool chunk size must be nonzero");
        }
        let capacity_pages = config.total_pages();
        let control_bytes = control_len(capacity_pages)?;
        let working = ImageBytes::allocate(capacity_pages, chunk_pages, control_bytes)?;
        let durable = if model || config.mode() == NvSimMode::Model {
            Some(ImageBytes::allocate(
                capacity_pages,
                chunk_pages,
                control_bytes,
            )?)
        } else {
            None
        };
        Ok(Self {
            inner: Arc::new(DramNvImageInner {
                working,
                durable,
                chunk_pages,
                capacity_pages,
                control_bytes,
                epoch: AtomicU64::new(0),
                persist_calls: AtomicU64::new(0),
                fence_calls: AtomicU64::new(0),
                io_lock: SpinLock::new(()),
            }),
        })
    }

    #[cfg(ktest)]
    pub(super) fn allocate_model(config: NvPoolConfig) -> Result<Self> {
        if config.mode() != NvSimMode::Model {
            return_errno_with_message!(Errno::EINVAL, "model image requires nv_sim_mode=model");
        }
        Self::allocate_inner(config, DEFAULT_CHUNK_PAGES, true)
    }

    pub(crate) fn attach(&self) -> DramNvBackend {
        DramNvBackend {
            image: self.clone(),
            epoch: self.inner.epoch.load(Ordering::Acquire),
        }
    }

    #[cfg(ktest)]
    pub(super) fn simulate_crash(&self) {
        let _guard = self.inner.io_lock.lock();
        if let Some(durable) = &self.inner.durable {
            let mut page = [0; PAGE_SIZE];
            for index in 0..self.inner.capacity_pages {
                let offset = index * PAGE_SIZE;
                durable
                    .read_data(self.inner.chunk_pages, offset, &mut page)
                    .unwrap();
                self.inner
                    .working
                    .write_data(self.inner.chunk_pages, offset, &page)
                    .unwrap();
            }
            let mut control = vec![0; self.inner.control_bytes];
            durable
                .read_control(self.inner.chunk_pages, 0, &mut control)
                .unwrap();
            self.inner
                .working
                .write_control(self.inner.chunk_pages, 0, &control)
                .unwrap();
        }
        self.inner.epoch.fetch_add(1, Ordering::Release);
    }
}

pub(crate) struct DramNvBackend {
    image: DramNvImage,
    epoch: u64,
}

impl DramNvBackend {
    pub(crate) fn allocate(config: NvPoolConfig) -> Result<Self> {
        DramNvImage::allocate_inner(config, DEFAULT_CHUNK_PAGES, false).map(|image| image.attach())
    }

    #[cfg(ktest)]
    pub(super) fn allocate_with_chunk_pages(
        config: NvPoolConfig,
        chunk_pages: usize,
    ) -> Result<Self> {
        DramNvImage::allocate_inner(config, chunk_pages, false).map(|image| image.attach())
    }

    pub(crate) fn stats(&self) -> DramNvBackendStats {
        DramNvBackendStats {
            persist_calls: self.image.inner.persist_calls.load(Ordering::Relaxed),
            fence_calls: self.image.inner.fence_calls.load(Ordering::Relaxed),
        }
    }

    fn check_epoch(&self) -> Result<()> {
        if self.epoch != self.image.inner.epoch.load(Ordering::Acquire) {
            return_errno_with_message!(Errno::EIO, "stale NVPool backend after simulated crash");
        }
        Ok(())
    }

    fn validate_data_range(&self, offset: usize, len: usize) -> Result<()> {
        self.check_epoch()?;
        let end = offset
            .checked_add(len)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        if end > self.image.inner.capacity_pages * PAGE_SIZE {
            return_errno_with_message!(Errno::EINVAL, "NVPool data range is out of bounds");
        }
        Ok(())
    }

    fn validate_control_range(&self, offset: usize, len: usize) -> Result<()> {
        self.check_epoch()?;
        let end = offset
            .checked_add(len)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        if end > self.image.inner.control_bytes {
            return_errno_with_message!(Errno::EINVAL, "NVPool control range is out of bounds");
        }
        Ok(())
    }

    fn copy_data_to_durable(&self, offset: usize, len: usize) -> Result<()> {
        let Some(durable) = &self.image.inner.durable else {
            return Ok(());
        };
        let mut page = [0; PAGE_SIZE];
        let mut copied = 0;
        while copied < len {
            let amount = (len - copied).min(PAGE_SIZE);
            self.image.inner.working.read_data(
                self.image.inner.chunk_pages,
                offset + copied,
                &mut page[..amount],
            )?;
            durable.write_data(
                self.image.inner.chunk_pages,
                offset + copied,
                &page[..amount],
            )?;
            copied += amount;
        }
        Ok(())
    }
}

impl NvMemoryBackend for DramNvBackend {
    fn capacity_pages(&self) -> usize {
        self.image.inner.capacity_pages
    }

    fn control_len(&self) -> usize {
        self.image.inner.control_bytes
    }

    fn read_page(&self, id: NvPageId, output: &mut [u8]) -> Result<()> {
        if output.len() != PAGE_SIZE {
            return_errno_with_message!(Errno::EINVAL, "NVPool reads must cover one page");
        }
        let offset = id
            .index()
            .checked_mul(PAGE_SIZE)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        self.validate_data_range(offset, output.len())?;
        let _guard = self.image.inner.io_lock.lock();
        self.image
            .inner
            .working
            .read_data(self.image.inner.chunk_pages, offset, output)
    }

    fn write_page(&self, id: NvPageId, input: &[u8]) -> Result<()> {
        if input.len() != PAGE_SIZE {
            return_errno_with_message!(Errno::EINVAL, "NVPool writes must cover one page");
        }
        let offset = id
            .index()
            .checked_mul(PAGE_SIZE)
            .ok_or_else(|| Error::new(Errno::EOVERFLOW))?;
        self.validate_data_range(offset, input.len())?;
        let _guard = self.image.inner.io_lock.lock();
        self.image
            .inner
            .working
            .write_data(self.image.inner.chunk_pages, offset, input)?;
        if self.image.inner.durable.is_none() {
            compiler_fence(Ordering::Release);
        }
        Ok(())
    }

    fn read_control(&self, offset: usize, output: &mut [u8]) -> Result<()> {
        self.validate_control_range(offset, output.len())?;
        let _guard = self.image.inner.io_lock.lock();
        self.image
            .inner
            .working
            .read_control(self.image.inner.chunk_pages, offset, output)?;
        Ok(())
    }

    fn write_control(&self, offset: usize, input: &[u8]) -> Result<()> {
        self.validate_control_range(offset, input.len())?;
        let _guard = self.image.inner.io_lock.lock();
        self.image
            .inner
            .working
            .write_control(self.image.inner.chunk_pages, offset, input)?;
        if self.image.inner.durable.is_none() {
            compiler_fence(Ordering::Release);
        }
        Ok(())
    }

    fn load_control_u64(&self, offset: usize) -> Result<u64> {
        if !offset.is_multiple_of(align_of::<u64>()) {
            return_errno_with_message!(Errno::EINVAL, "unaligned NVPool atomic load");
        }
        self.validate_control_range(offset, size_of::<u64>())?;
        let _guard = self.image.inner.io_lock.lock();
        self.image
            .inner
            .working
            .read_control_once(self.image.inner.chunk_pages, offset)
    }

    fn store_control_u64(&self, offset: usize, value: u64) -> Result<()> {
        if !offset.is_multiple_of(align_of::<u64>()) {
            return_errno_with_message!(Errno::EINVAL, "unaligned NVPool atomic store");
        }
        self.validate_control_range(offset, size_of::<u64>())?;
        let _guard = self.image.inner.io_lock.lock();
        self.image
            .inner
            .working
            .write_control_once(self.image.inner.chunk_pages, offset, value)
    }

    fn persist(&self, offset: usize, len: usize) -> Result<()> {
        self.validate_data_range(offset, len)?;
        let _guard = self.image.inner.io_lock.lock();
        self.copy_data_to_durable(offset, len)?;
        compiler_fence(Ordering::Release);
        self.image
            .inner
            .persist_calls
            .fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn persist_control(&self, offset: usize, len: usize) -> Result<()> {
        self.validate_control_range(offset, len)?;
        let _guard = self.image.inner.io_lock.lock();
        if let Some(durable) = &self.image.inner.durable {
            let mut bytes = vec![0; len];
            self.image.inner.working.read_control(
                self.image.inner.chunk_pages,
                offset,
                &mut bytes,
            )?;
            durable.write_control(self.image.inner.chunk_pages, offset, &bytes)?;
        }
        compiler_fence(Ordering::Release);
        self.image
            .inner
            .persist_calls
            .fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn fence(&self) -> Result<()> {
        self.check_epoch()?;
        compiler_fence(Ordering::SeqCst);
        self.image.inner.fence_calls.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}
