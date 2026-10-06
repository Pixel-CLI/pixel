// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Window weighting for sparse n-gram selection.
//!
//! A `Weigher` assigns a deterministic weight to a fixed-size byte window
//! (`min_gram_len - 1` bytes, i.e. 2 bytes for the default min of 3).
//! The sparse-gram predicate compares these weights; any deterministic
//! function works, but the choice shapes gram boundaries:
//! - `Crc32Weigher`: uniform pseudo-random weights (ClickHouse default).
//! - `FreqTableWeigher`: corpus-frequency-derived weights so rare byte
//!   pairs become boundaries (Cursor's refinement). Table generation is a
//!   post-v1 A/B experiment; the plumbing exists so shards can declare
//!   which weigher built them.

pub trait Weigher: Send + Sync {
    fn weight(&self, window: &[u8]) -> u32;

    /// Stable identifier persisted in shard headers; an index built with
    /// one weigher must never be queried with another.
    fn id(&self) -> &'static str;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Crc32Weigher;

/// CRC32 of every 2-byte window, precomputed once — `crc32fast::hash` builds
/// a hasher per call, which dominates extraction time on 2-byte inputs. The
/// table produces byte-identical weights to `crc32fast::hash(window)`.
fn pair_crc_table() -> &'static [u32; 65536] {
    use std::sync::OnceLock;
    static TABLE: OnceLock<Box<[u32; 65536]>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut t = vec![0u32; 65536].into_boxed_slice();
        for (i, slot) in t.iter_mut().enumerate() {
            *slot = crc32fast::hash(&[(i >> 8) as u8, i as u8]);
        }
        t.try_into().unwrap()
    })
}

impl Weigher for Crc32Weigher {
    #[inline]
    fn weight(&self, window: &[u8]) -> u32 {
        if window.len() == 2 {
            pair_crc_table()[usize::from(window[0]) << 8 | usize::from(window[1])]
        } else {
            crc32fast::hash(window)
        }
    }

    fn id(&self) -> &'static str {
        "crc32"
    }
}

/// Byte-pair frequency table weigher (2-byte windows only).
pub struct FreqTableWeigher {
    table: Box<[u32; 65536]>,
    id: &'static str,
}

impl FreqTableWeigher {
    pub fn new(table: Box<[u32; 65536]>, id: &'static str) -> Self {
        Self { table, id }
    }
}

impl Weigher for FreqTableWeigher {
    #[inline]
    fn weight(&self, window: &[u8]) -> u32 {
        debug_assert_eq!(window.len(), 2);
        self.table[usize::from(window[0]) << 8 | usize::from(window[1])]
    }

    fn id(&self) -> &'static str {
        self.id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_pair_table_matches_the_direct_hash() {
        for pair in [*b"ab", *b"zq", [0x00, 0xff], [0xff, 0x00], [0x7f, 0x80]] {
            assert_eq!(
                Crc32Weigher.weight(&pair),
                crc32fast::hash(&pair),
                "{pair:?}"
            );
        }
    }
}
