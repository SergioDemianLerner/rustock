//! Block-level consensus rules — the ones that need the block body, not just
//! its header.
//!
//! rskj composes these in `RskContext.getBlockValidationRule` alongside the
//! header rules. Each one *rejects a block*: rustock computing the same state
//! root as rskj for every block rskj accepts is necessary but not sufficient,
//! because a validator that never rejects follows any chain it is given.

use super::{BlockValidator, HeaderValidator, ParentHeaderValidator, ValidationError};
use crate::types::block::Block;
use crate::types::header::Header;
use alloy_primitives::U256;

/// The REMASC contract address (`…01000008`), which the synthetic last
/// transaction of every RSK block is sent to. Mirrors
/// `rustock_execution::precompiles::REMASC_ADDR`, duplicated here because
/// `rustock-core` sits below the execution crate.
const REMASC_ADDR: [u8; 20] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x01, 0x00, 0x00, 0x08,
];

/// rskj `RemascTransaction`: `to == REMASC_ADDR`, zero signature, zero gas
/// limit. Same predicate `BlockProcessor::is_remasc_tx` uses to skip sender
/// recovery.
fn is_remasc_tx(tx: &crate::types::transaction::Transaction) -> bool {
    tx.v == 0
        && tx.r.is_zero()
        && tx.s.is_zero()
        && tx.gas_limit.is_zero()
        && tx.to.len() == 20
        && tx.to.as_ref() == REMASC_ADDR
}

/// rskj `TxsMinGasPriceRule`: every non-REMASC transaction must pay at least
/// the block's `minimumGasPrice`. Unconditional — no activation gate.
pub struct TxsMinGasPriceRule;

impl BlockValidator for TxsMinGasPriceRule {
    fn validate_block(&self, block: &Block) -> Result<(), ValidationError> {
        let min = block.header.minimum_gas_price;
        for tx in &block.transactions {
            if is_remasc_tx(tx) {
                continue;
            }
            if tx.gas_price < min {
                return Err(ValidationError::TxGasPriceBelowMinimum {
                    tx_gas_price: tx.gas_price,
                    block_minimum: min,
                });
            }
        }
        Ok(())
    }
}

/// rskj `BlockTxsMaxGasPriceRule` + `TxGasPriceCap.FOR_BLOCK` (RSKIP252,
/// fingerroot500): no transaction may pay more than 100x the block's
/// `minimumGasPrice`.
///
/// `TxGasPriceCap.isSurpassed` short-circuits on two cases before comparing:
/// a REMASC transaction never surpasses the cap, and neither does anything
/// when the block's minimum gas price is zero.
pub struct BlockTxsMaxGasPriceRule {
    /// First block at which RSKIP252 applies (`fingerroot500`).
    pub rskip252_height: u64,
}

impl BlockTxsMaxGasPriceRule {
    /// `TxGasPriceCap.FOR_BLOCK`.
    pub const FOR_BLOCK_MULTIPLIER: u64 = 100;
}

impl BlockValidator for BlockTxsMaxGasPriceRule {
    fn validate_block(&self, block: &Block) -> Result<(), ValidationError> {
        if block.header.number < self.rskip252_height {
            return Ok(());
        }
        let min = block.header.minimum_gas_price;
        if min.is_zero() {
            return Ok(());
        }
        let cap = min.saturating_mul(U256::from(Self::FOR_BLOCK_MULTIPLIER));
        for tx in &block.transactions {
            if is_remasc_tx(tx) {
                continue;
            }
            if tx.gas_price > cap {
                return Err(ValidationError::TxGasPriceAboveCap {
                    tx_gas_price: tx.gas_price,
                    cap,
                });
            }
        }
        Ok(())
    }
}

/// rskj `RemascValidationRule`: the block's transaction list must be non-empty
/// and its **last** entry must be the REMASC transaction.
pub struct RemascValidationRule;

impl BlockValidator for RemascValidationRule {
    fn validate_block(&self, block: &Block) -> Result<(), ValidationError> {
        match block.transactions.last() {
            Some(tx) if is_remasc_tx(tx) => Ok(()),
            _ => Err(ValidationError::RemascTxMissing),
        }
    }
}

/// rskj `ExtraDataRule`: `extraData` may not exceed
/// `Constants.getMaximumExtraDataSize()` (32 on every RSK network).
pub struct ExtraDataRule {
    pub max_size: usize,
}

impl Default for ExtraDataRule {
    fn default() -> Self {
        Self { max_size: 32 }
    }
}

impl HeaderValidator for ExtraDataRule {
    fn validate(&self, header: &Header) -> Result<(), ValidationError> {
        if header.extra_data.len() > self.max_size {
            return Err(ValidationError::ExtraDataTooLarge {
                max: self.max_size,
                got: header.extra_data.len(),
            });
        }
        Ok(())
    }
}

/// rskj `PrevMinGasPriceRule` + `BlockGasPriceRange`: a block's
/// `minimumGasPrice` may move by at most 1% of its parent's, in either
/// direction, per block.
///
/// `BlockGasPriceRange(center)`:
/// ```text
///   delta = center * 1 / 100        (integer division)
///   delta = 1 when that is zero     (so a zero parent can still rise to 1)
///   upper = center + delta
///   lower = max(0, center - delta)
/// ```
/// and the header's value must lie in `[lower, upper]` inclusive.
pub struct PrevMinGasPriceRule;

impl PrevMinGasPriceRule {
    /// The inclusive `[lower, upper]` band a child's minimum gas price may
    /// occupy, given the parent's.
    pub fn range(parent_min_gas_price: U256) -> (U256, U256) {
        let center = parent_min_gas_price;
        let mut delta = center / U256::from(100u64);
        if delta.is_zero() {
            delta = U256::from(1u64);
        }
        let upper = center.saturating_add(delta);
        let lower = center.saturating_sub(delta);
        (lower, upper)
    }
}

impl ParentHeaderValidator for PrevMinGasPriceRule {
    fn validate_with_parent(&self, header: &Header, parent: &Header) -> Result<(), ValidationError> {
        if header.number == 0 {
            return Ok(());
        }
        let (lower, upper) = Self::range(parent.minimum_gas_price);
        let got = header.minimum_gas_price;
        if got < lower || got > upper {
            return Err(ValidationError::MinGasPriceOutOfRange { lower, upper, got });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::transaction::Transaction;
    use alloy_primitives::{Address, Bytes, B256};

    fn bare_header(number: u64) -> Header {
        Header {
            parent_hash: B256::ZERO,
            ommers_hash: B256::ZERO,
            beneficiary: Address::ZERO,
            state_root: B256::ZERO,
            transactions_root: B256::ZERO,
            receipts_root: B256::ZERO,
            logs_bloom: Default::default(),
            extension_data: None,
            difficulty: U256::ZERO,
            number,
            gas_limit: U256::from(10_000_000),
            gas_used: 0,
            timestamp: 1_700_000_000,
            extra_data: Bytes::default(),
            paid_fees: U256::ZERO,
            minimum_gas_price: U256::ZERO,
            uncle_count: 0,
            umm_root: None,
            bitcoin_merged_mining_header: None,
            bitcoin_merged_mining_merkle_proof: None,
            bitcoin_merged_mining_coinbase_transaction: None,
            cached_hash: None,
            cached_hash_for_merged_mining: None,
        }
    }

    fn remasc_tx() -> Transaction {
        Transaction {
            nonce: 0,
            gas_price: U256::ZERO,
            gas_limit: U256::ZERO,
            to: Bytes::copy_from_slice(&REMASC_ADDR),
            value: U256::ZERO,
            input: Bytes::new(),
            v: 0,
            r: U256::ZERO,
            s: U256::ZERO,
            cached_rlp: None,
        }
    }

    fn user_tx(gas_price: u64) -> Transaction {
        Transaction {
            nonce: 0,
            gas_price: U256::from(gas_price),
            gas_limit: U256::from(21_000),
            to: Bytes::copy_from_slice(&[0xBBu8; 20]),
            value: U256::ZERO,
            input: Bytes::new(),
            v: 27,
            r: U256::from(1u64),
            s: U256::from(1u64),
            cached_rlp: None,
        }
    }

    fn block_at(number: u64, min_gas_price: u64, txs: Vec<Transaction>) -> Block {
        let mut header = bare_header(number);
        header.minimum_gas_price = U256::from(min_gas_price);
        Block { header, transactions: txs, ommers: vec![] }
    }

    /// rskj `TxsMinGasPriceRule`.
    #[test]
    fn txs_min_gas_price_rejects_an_underpriced_tx() {
        let ok = block_at(9_000_000, 100, vec![user_tx(100), remasc_tx()]);
        assert!(TxsMinGasPriceRule.validate_block(&ok).is_ok());

        let under = block_at(9_000_000, 100, vec![user_tx(99), remasc_tx()]);
        assert!(matches!(
            TxsMinGasPriceRule.validate_block(&under),
            Err(ValidationError::TxGasPriceBelowMinimum { .. })
        ));

        // The REMASC transaction pays nothing and is exempt.
        let only_remasc = block_at(9_000_000, 100, vec![remasc_tx()]);
        assert!(TxsMinGasPriceRule.validate_block(&only_remasc).is_ok());
    }

    /// rskj `BlockTxsMaxGasPriceRule` + `TxGasPriceCap.FOR_BLOCK` (RSKIP252).
    #[test]
    fn block_txs_max_gas_price_enforces_the_100x_cap_from_rskip252() {
        let rule = BlockTxsMaxGasPriceRule { rskip252_height: 5_468_000 };

        // At the cap exactly: allowed (`compareTo(cap) > 0`).
        let at_cap = block_at(9_000_000, 100, vec![user_tx(10_000), remasc_tx()]);
        assert!(rule.validate_block(&at_cap).is_ok());

        let over = block_at(9_000_000, 100, vec![user_tx(10_001), remasc_tx()]);
        assert!(matches!(
            rule.validate_block(&over),
            Err(ValidationError::TxGasPriceAboveCap { .. })
        ));

        // One block before fingerroot500 the rule does not apply.
        let pre = block_at(5_467_999, 100, vec![user_tx(10_001), remasc_tx()]);
        assert!(rule.validate_block(&pre).is_ok());

        // `isSurpassed` short-circuits when the block's minimum is zero.
        let zero_min = block_at(9_000_000, 0, vec![user_tx(u64::MAX), remasc_tx()]);
        assert!(rule.validate_block(&zero_min).is_ok());
    }

    /// rskj `RemascValidationRule`.
    #[test]
    fn remasc_rule_requires_remasc_as_the_last_transaction() {
        let ok = block_at(9_000_000, 0, vec![user_tx(0), remasc_tx()]);
        assert!(RemascValidationRule.validate_block(&ok).is_ok());

        // Present but not last.
        let misplaced = block_at(9_000_000, 0, vec![remasc_tx(), user_tx(0)]);
        assert!(matches!(
            RemascValidationRule.validate_block(&misplaced),
            Err(ValidationError::RemascTxMissing)
        ));

        // Absent entirely, and the empty block.
        let absent = block_at(9_000_000, 0, vec![user_tx(0)]);
        assert!(RemascValidationRule.validate_block(&absent).is_err());
        let empty = block_at(9_000_000, 0, vec![]);
        assert!(RemascValidationRule.validate_block(&empty).is_err());
    }

    /// rskj `ExtraDataRule` with `Constants.getMaximumExtraDataSize()` = 32.
    #[test]
    fn extra_data_rule_caps_at_32_bytes() {
        let rule = ExtraDataRule::default();
        assert_eq!(rule.max_size, 32);

        let mut header = bare_header(9_000_000);
        header.extra_data = Bytes::from(vec![0u8; 32]);
        assert!(rule.validate(&header).is_ok());

        header.extra_data = Bytes::from(vec![0u8; 33]);
        assert!(matches!(
            rule.validate(&header),
            Err(ValidationError::ExtraDataTooLarge { max: 32, got: 33 })
        ));
    }

    /// rskj `PrevMinGasPriceRule` + `BlockGasPriceRange`.
    #[test]
    fn prev_min_gas_price_allows_one_percent_either_way() {
        // center 1000 -> delta 10 -> [990, 1010] inclusive.
        let (lower, upper) = PrevMinGasPriceRule::range(U256::from(1000u64));
        assert_eq!(lower, U256::from(990u64));
        assert_eq!(upper, U256::from(1010u64));

        // A zero parent still gets delta 1, so it can rise to 1 (and the lower
        // bound is clamped at zero rather than going negative).
        let (lower, upper) = PrevMinGasPriceRule::range(U256::ZERO);
        assert_eq!(lower, U256::ZERO);
        assert_eq!(upper, U256::from(1u64));

        let mut parent = bare_header(8_999_999);
        parent.minimum_gas_price = U256::from(1000u64);

        let check = |mgp: u64| {
            let mut header = bare_header(9_000_000);
            header.minimum_gas_price = U256::from(mgp);
            PrevMinGasPriceRule.validate_with_parent(&header, &parent)
        };

        assert!(check(990).is_ok());
        assert!(check(1010).is_ok());
        assert!(check(1000).is_ok());
        assert!(matches!(
            check(989),
            Err(ValidationError::MinGasPriceOutOfRange { .. })
        ));
        assert!(matches!(
            check(1011),
            Err(ValidationError::MinGasPriceOutOfRange { .. })
        ));
    }
}
