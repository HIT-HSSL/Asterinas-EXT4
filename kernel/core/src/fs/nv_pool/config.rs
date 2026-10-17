// SPDX-License-Identifier: MPL-2.0

//! Configuration for the simulated NVPool.

use core::str::FromStr;

use aster_cmdline::{parse::ParseParamValue, types::MetricU64};
use spin::Once;

use crate::prelude::*;

#[derive(Debug)]
struct RawParam(String);

impl FromStr for RawParam {
    type Err = core::convert::Infallible;

    fn from_str(value: &str) -> core::result::Result<Self, Self::Err> {
        Ok(Self(value.to_string()))
    }
}

static NV_POOL_SIZE: Once<RawParam> = Once::new();
aster_cmdline::define_kv_param!("nv_pool_size", NV_POOL_SIZE);

static NV_META_RESERVE: Once<RawParam> = Once::new();
aster_cmdline::define_kv_param!("nv_meta_reserve", NV_META_RESERVE);

static NV_DATA_LIMIT: Once<RawParam> = Once::new();
aster_cmdline::define_kv_param!("nv_data_limit", NV_DATA_LIMIT);

static NV_SIM_MODE: Once<RawParam> = Once::new();
aster_cmdline::define_kv_param!("nv_sim_mode", NV_SIM_MODE);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NvSimMode {
    Fast,
    Model,
}

impl FromStr for NvSimMode {
    type Err = ();

    fn from_str(value: &str) -> core::result::Result<Self, Self::Err> {
        match value {
            "fast" => Ok(Self::Fast),
            "model" => Ok(Self::Model),
            _ => Err(()),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct NvPoolConfig {
    total_pages: usize,
    meta_reserve_pages: usize,
    data_limit_pages: usize,
    mode: NvSimMode,
}

impl NvPoolConfig {
    pub(super) fn from_command_line() -> Result<Option<Self>> {
        Self::from_params(
            NV_POOL_SIZE.get().map(|value| value.0.as_str()),
            NV_META_RESERVE.get().map(|value| value.0.as_str()),
            NV_DATA_LIMIT.get().map(|value| value.0.as_str()),
            NV_SIM_MODE.get().map(|value| value.0.as_str()),
        )
    }

    pub(crate) fn from_params(
        total: Option<&str>,
        meta_reserve: Option<&str>,
        data_limit: Option<&str>,
        mode: Option<&str>,
    ) -> Result<Option<Self>> {
        fn parse_bytes(value: Option<&str>, name: &'static str) -> Result<Option<usize>> {
            value
                .map(|raw| {
                    let bytes = MetricU64::parse_param(raw)
                        .map_err(|_| Error::with_message(Errno::EINVAL, name))?
                        .0;
                    usize::try_from(bytes).map_err(|_| Error::with_message(Errno::EOVERFLOW, name))
                })
                .transpose()
        }

        let mode = mode
            .map(|value| {
                value
                    .parse::<NvSimMode>()
                    .map_err(|_| Error::with_message(Errno::EINVAL, "invalid nv_sim_mode value"))
            })
            .transpose()?;
        Self::from_bytes(
            parse_bytes(total, "invalid nv_pool_size value")?,
            parse_bytes(meta_reserve, "invalid nv_meta_reserve value")?,
            parse_bytes(data_limit, "invalid nv_data_limit value")?,
            mode,
        )
    }

    pub(crate) fn from_bytes(
        total_bytes: Option<usize>,
        meta_reserve_bytes: Option<usize>,
        data_limit_bytes: Option<usize>,
        mode: Option<NvSimMode>,
    ) -> Result<Option<Self>> {
        let Some(total_bytes) = total_bytes else {
            if meta_reserve_bytes.is_some() || data_limit_bytes.is_some() || mode.is_some() {
                return_errno_with_message!(Errno::EINVAL, "NVPool options require nv_pool_size");
            }
            return Ok(None);
        };
        let Some(meta_reserve_bytes) = meta_reserve_bytes else {
            return_errno_with_message!(Errno::EINVAL, "nv_meta_reserve is required");
        };
        let Some(data_limit_bytes) = data_limit_bytes else {
            return_errno_with_message!(Errno::EINVAL, "nv_data_limit is required");
        };

        if total_bytes == 0
            || meta_reserve_bytes == 0
            || !total_bytes.is_multiple_of(PAGE_SIZE)
            || !meta_reserve_bytes.is_multiple_of(PAGE_SIZE)
            || !data_limit_bytes.is_multiple_of(PAGE_SIZE)
        {
            return_errno_with_message!(
                Errno::EINVAL,
                "NVPool total and metadata reserve must be nonzero; all capacities must be page aligned"
            );
        }

        let total_pages = total_bytes / PAGE_SIZE;
        let meta_reserve_pages = meta_reserve_bytes / PAGE_SIZE;
        let data_limit_pages = data_limit_bytes / PAGE_SIZE;
        if total_pages > u32::MAX as usize + 1 {
            return_errno_with_message!(
                Errno::EOVERFLOW,
                "NVPool capacity exceeds the pool-local ID space"
            );
        }
        if meta_reserve_pages > total_pages || data_limit_pages > total_pages - meta_reserve_pages {
            return_errno_with_message!(
                Errno::EINVAL,
                "NVPool metadata reserve and data limit exceed total capacity"
            );
        }

        Ok(Some(Self {
            total_pages,
            meta_reserve_pages,
            data_limit_pages,
            mode: mode.unwrap_or(NvSimMode::Fast),
        }))
    }

    pub(crate) fn total_pages(self) -> usize {
        self.total_pages
    }

    pub(crate) fn meta_reserve_pages(self) -> usize {
        self.meta_reserve_pages
    }

    pub(crate) fn data_limit_pages(self) -> usize {
        self.data_limit_pages
    }

    pub(crate) fn mode(self) -> NvSimMode {
        self.mode
    }
}
