//! Lightweight non-cryptographic randomness for choosing turns and prompts.
//! No additional dependency or secure seed is needed for a party game.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static DRAW: AtomicU64 = AtomicU64::new(0);

pub(super) fn index(len: usize) -> usize {
    debug_assert!(len > 0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    let mut value = nanos ^ DRAW.fetch_add(0x9e3779b97f4a7c15, Ordering::Relaxed);
    // SplitMix64 mixing step: avoid successive requests choosing the same item.
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d049bb133111eb);
    ((value ^ (value >> 31)) % len as u64) as usize
}

#[cfg(test)]
mod tests {
    #[test]
    fn index_is_in_range() {
        for size in 1..32 {
            for _ in 0..100 {
                assert!(super::index(size) < size);
            }
        }
    }
}
