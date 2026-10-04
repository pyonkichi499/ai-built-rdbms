//! The mapping table `BufferTag -> frame`, split into 16 partitions
//! (`m2.md` §6.3 item 1). At most one partition lock is held at a time, and
//! no I/O is done while holding it.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Mutex;

use super::frame::FrameId;
use crate::storage::smgr::BufferTag;

pub(super) const NUM_PARTITIONS: usize = 16;

pub(super) type Partition = Mutex<HashMap<BufferTag, FrameId>>;

#[derive(Debug)]
pub(super) struct MappingTable {
    parts: [Partition; NUM_PARTITIONS],
}

impl MappingTable {
    pub(super) fn new() -> MappingTable {
        MappingTable {
            parts: std::array::from_fn(|_| Mutex::new(HashMap::new())),
        }
    }

    pub(super) fn part(&self, tag: &BufferTag) -> &Partition {
        let mut h = DefaultHasher::new();
        tag.hash(&mut h);
        // The modulus keeps the value below NUM_PARTITIONS.
        #[allow(clippy::cast_possible_truncation)]
        let idx = (h.finish() % NUM_PARTITIONS as u64) as usize;
        &self.parts[idx]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::smgr::{ForkNumber, RelFileLocator, RelFileNumber};

    #[test]
    fn same_tag_same_partition_and_spread() {
        let t = MappingTable::new();
        let tag = |b| BufferTag {
            rel: RelFileLocator {
                spc_oid: 1663,
                db_oid: 5,
                rel_number: RelFileNumber(16384),
            },
            fork: ForkNumber::Main,
            block: b,
        };
        assert!(std::ptr::eq(t.part(&tag(3)), t.part(&tag(3))));
        let used: std::collections::HashSet<usize> = (0..200)
            .map(|b| std::ptr::from_ref::<Partition>(t.part(&tag(b))) as usize)
            .collect();
        assert!(used.len() > 8);
    }
}
