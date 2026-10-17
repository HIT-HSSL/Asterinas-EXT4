// SPDX-License-Identifier: MPL-2.0

//! Reconstruction of volatile NVPool indexes from committed control-image entries.

use super::{
    capacity::PoolClass,
    frame::{MetaKey, ResidentRecord},
    persist::PersistEngine,
};
use crate::prelude::*;

pub(super) struct RecoveredEntry {
    pub(super) index: usize,
    pub(super) class: PoolClass,
    pub(super) key: MetaKey,
    pub(super) version: u64,
}

pub(super) fn scan(
    engine: &PersistEngine,
    capacity_pages: usize,
) -> Result<(Vec<RecoveredEntry>, BTreeMap<MetaKey, ResidentRecord>)> {
    engine.read_header()?;
    let mut candidates = BTreeMap::<MetaKey, RecoveredEntry>::new();
    let mut data_entries = Vec::new();
    let mut stale = Vec::new();
    let mut resident = BTreeMap::new();
    for index in 0..capacity_pages {
        let Some((class, namespace, home_block, version, _slot_bitmap)) =
            engine.read_entry(index)?
        else {
            continue;
        };
        let key = MetaKey::new(namespace, home_block);
        let candidate = RecoveredEntry {
            index,
            class,
            key,
            version,
        };
        if class == PoolClass::Data {
            data_entries.push(candidate);
            continue;
        }
        if let Some(current) = candidates.get(&key) {
            if current.version == version {
                return_errno_with_message!(Errno::EINVAL, "duplicate NVPool metadata version");
            }
            if current.version > version {
                stale.push(index);
                continue;
            }
            stale.push(current.index);
        }
        candidates.insert(key, candidate);
    }
    for index in stale {
        engine.clear_frame(index, None)?;
    }
    let metadata_entries = candidates.into_values().collect::<Vec<_>>();
    for entry in &metadata_entries {
        resident.insert(
            entry.key,
            ResidentRecord {
                index: entry.index,
                version: entry.version,
            },
        );
    }
    let mut entries = metadata_entries;
    entries.extend(data_entries);
    Ok((entries, resident))
}
