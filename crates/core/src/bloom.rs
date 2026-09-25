//! The logs bloom filter: one definition of the bit derivation, used both to
//! build a bloom and to test one.
//!
//! # Why this is not two functions in two crates
//!
//! A bloom filter has false positives and **no false negatives**, which is the
//! whole reason it is safe to skip a block whose bloom does not match a query.
//! That guarantee holds only while the bits set on insertion are the bits
//! checked on lookup. Two implementations of "which three bits does this
//! 32-byte hash name" that disagree by one would make `eth_getLogs` silently
//! return fewer logs than exist -- the worst shape of bug this RPC can have,
//! because the answer still looks like a valid answer.
//!
//! So the derivation lives here once, `accrue_log` builds with it and
//! `may_contain` tests with it, and `bloom_agrees_with_itself` pins that they
//! are the same bits.

use crate::types::receipt::Log;
use alloy_primitives::Bloom;
use sha3::{Digest, Keccak256};

/// The three bit positions a piece of bloom input names.
///
/// Ethereum's "m3:2048": keccak the input, take the first three big-endian
/// 16-bit words, mask each to 11 bits, and use it as a bit index into a
/// 2,048-bit filter. The `255 - bit / 8` is the byte order: bit 0 is the
/// *last* byte's low bit.
fn bit_positions(data: &[u8]) -> [(usize, u8); 3] {
    let hash = Keccak256::digest(data);
    let mut out = [(0usize, 0u8); 3];
    for (i, slot) in out.iter_mut().enumerate() {
        let bit = (((hash[2 * i] as usize) << 8) | (hash[2 * i + 1] as usize)) & 0x7FF;
        *slot = (255 - bit / 8, 1u8 << (bit % 8));
    }
    out
}

/// Set the three bits this input names.
pub fn accrue(bloom: &mut Bloom, data: &[u8]) {
    for (byte, mask) in bit_positions(data) {
        bloom.0[byte] |= mask;
    }
}

/// Accrue a log's address and every one of its topics.
pub fn accrue_log(bloom: &mut Bloom, log: &Log) {
    accrue(bloom, log.address.as_slice());
    for topic in &log.topics {
        accrue(bloom, topic.as_slice());
    }
}

/// Could this bloom have been built from something containing `data`?
///
/// `false` is certain: the input was definitely never accrued. `true` is a
/// maybe -- the caller must still confirm against the real logs.
pub fn may_contain(bloom: &Bloom, data: &[u8]) -> bool {
    bit_positions(data)
        .into_iter()
        .all(|(byte, mask)| bloom.0[byte] & mask != 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, B256, Bytes};

    fn log_of(address: Address, topics: Vec<B256>) -> Log {
        Log { address, topics, data: Bytes::default() }
    }

    /// The property the whole optimisation rests on: what was accrued is
    /// always found. A single-bit disagreement here would make `eth_getLogs`
    /// drop logs that exist.
    #[test]
    fn bloom_agrees_with_itself() {
        let mut bloom = Bloom::ZERO;
        let address = Address::repeat_byte(0xAB);
        let topics = vec![B256::repeat_byte(0x11), B256::repeat_byte(0x22)];
        accrue_log(&mut bloom, &log_of(address, topics.clone()));

        assert!(may_contain(&bloom, address.as_slice()), "the address must be found");
        for topic in &topics {
            assert!(may_contain(&bloom, topic.as_slice()), "topic {topic} must be found");
        }
    }

    /// Exhaustive on a larger sample, because "no false negatives" is a claim
    /// about *every* input, not the one that was convenient to write down.
    #[test]
    fn nothing_accrued_is_ever_missed() {
        let mut bloom = Bloom::ZERO;
        let mut accrued = Vec::new();
        for i in 0..256u16 {
            let address = Address::repeat_byte(i as u8);
            let topic = B256::repeat_byte((i >> 1) as u8);
            accrue_log(&mut bloom, &log_of(address, vec![topic]));
            accrued.push(address.as_slice().to_vec());
            accrued.push(topic.as_slice().to_vec());
        }
        for item in &accrued {
            assert!(may_contain(&bloom, item), "false negative on {item:?}");
        }
    }

    /// A bloom is useful only if it usually says no. With one log in it, most
    /// unrelated addresses should be rejected -- a filter that always answers
    /// "maybe" is correct and worthless.
    #[test]
    fn an_unrelated_address_is_usually_rejected() {
        let mut bloom = Bloom::ZERO;
        accrue_log(&mut bloom, &log_of(Address::repeat_byte(0xAB), vec![B256::repeat_byte(0x11)]));

        let misses = (0..200u8)
            .map(|i| {
                let mut bytes = [0u8; 20];
                bytes[0] = i;
                bytes[19] = i.wrapping_mul(7);
                Address::from(bytes)
            })
            .filter(|a| !may_contain(&bloom, a.as_slice()))
            .count();
        assert!(misses > 190, "only {misses}/200 unrelated addresses rejected");
    }

    #[test]
    fn an_empty_bloom_contains_nothing() {
        assert!(!may_contain(&Bloom::ZERO, Address::repeat_byte(0x01).as_slice()));
    }
}
