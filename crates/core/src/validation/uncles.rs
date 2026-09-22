//! Uncle (ommer) validation — rskj `BlockUnclesValidationRule`.
//!
//! The ommers *hash* is already checked in `BlockProcessor::process_block`
//! (rskj's `BlockUnclesHashValidationRule`), which proves the list matches the
//! header field. That says nothing about whether the uncles themselves are
//! admissible, which is what this rule decides.

use super::{ValidationError, HeaderValidator, ParentHeaderValidator};
use crate::types::block::Block;
use crate::types::header::Header;
use alloy_primitives::B256;
use std::collections::HashSet;

/// rskj `Constants.getUncleListLimit()`.
pub const UNCLE_LIST_LIMIT: usize = 10;
/// rskj `Constants.getUncleGenerationLimit()`.
pub const UNCLE_GENERATION_LIMIT: u64 = 7;

/// The ancestry lookups `FamilyUtils` performs against rskj's block store.
/// Implemented by the caller that owns the store.
pub trait AncestorSource {
    /// The header stored under `hash`, if any.
    fn header(&self, hash: B256) -> Option<Header>;
    /// The uncle headers of the block stored under `hash`.
    fn uncles_of(&self, hash: B256) -> Vec<Header>;
}

/// rskj `FamilyUtils.getAncestors`: walk parents from `parent_hash` while the
/// block number stays at or above `block_number - limit`, collecting hashes.
pub fn ancestors<S: AncestorSource + ?Sized>(
    store: &S,
    block_number: u64,
    parent_hash: B256,
    limit: u64,
) -> HashSet<B256> {
    let mut out = HashSet::new();
    let floor = block_number.saturating_sub(limit);
    let mut cursor = Some(parent_hash);
    while let Some(hash) = cursor {
        let Some(header) = store.header(hash) else { break };
        if header.number < floor {
            break;
        }
        out.insert(hash);
        cursor = Some(header.parent_hash);
        if header.number == 0 {
            break;
        }
    }
    out
}

/// rskj `FamilyUtils.getUsedUncles`: the uncles already included by the same
/// stretch of ancestors.
pub fn used_uncles<S: AncestorSource + ?Sized>(
    store: &S,
    block_number: u64,
    parent_hash: B256,
    limit: u64,
) -> HashSet<B256> {
    let mut out = HashSet::new();
    let floor = block_number.saturating_sub(limit);
    let mut cursor = Some(parent_hash);
    while let Some(hash) = cursor {
        let Some(header) = store.header(hash) else { break };
        if header.number < floor {
            break;
        }
        for uncle in store.uncles_of(hash) {
            out.insert(uncle.hash());
        }
        cursor = Some(header.parent_hash);
        if header.number == 0 {
            break;
        }
    }
    out
}

/// rskj `BlockUnclesValidationRule.validateUncleList`.
///
/// `header_rules` and `parent_rules` are the same per-uncle checks rskj passes
/// in: `BlockHeaderCompositeRule(PoW, forkDetection, timestamp, gasUsed)` and
/// `BlockHeaderParentCompositeRule(prevMinGasPrice, parentNumber, timestamp,
/// difficulty, parentGasLimit)`.
pub struct UnclesValidationRule<'a, S: AncestorSource + ?Sized> {
    pub store: &'a S,
    pub uncle_list_limit: usize,
    pub uncle_generation_limit: u64,
    pub header_rules: &'a [Box<dyn HeaderValidator>],
    pub parent_rules: &'a [Box<dyn ParentHeaderValidator>],
}

impl<S: AncestorSource + ?Sized> UnclesValidationRule<'_, S> {
    pub fn validate(&self, block: &Block) -> Result<(), ValidationError> {
        if block.ommers.is_empty() {
            return Ok(());
        }

        let number = block.header.number;
        let ancestors = ancestors(
            self.store,
            number,
            block.header.parent_hash,
            self.uncle_generation_limit,
        );
        let used = used_uncles(
            self.store,
            number,
            block.header.parent_hash,
            self.uncle_generation_limit,
        );

        if block.ommers.len() > self.uncle_list_limit {
            return Err(ValidationError::TooManyUncles {
                max: self.uncle_list_limit,
                got: block.ommers.len(),
            });
        }

        let mut seen: HashSet<B256> = HashSet::new();
        for uncle in &block.ommers {
            // rskj: `this.validations.isValid(uncle)` then validateParentNumber.
            for rule in self.header_rules {
                rule.validate(uncle)?;
            }

            // validateParentNumber: a sibling or descendant is not an uncle,
            // and the generation gap is bounded.
            let uncle_hash = uncle.hash();
            if uncle.number >= number {
                return Err(ValidationError::UncleIsSiblingOrDescendant { hash: uncle_hash });
            }
            if uncle.number + self.uncle_generation_limit < number + 1 {
                return Err(ValidationError::UncleTooOld {
                    hash: uncle_hash,
                    limit: self.uncle_generation_limit,
                });
            }

            if !seen.insert(uncle_hash) {
                return Err(ValidationError::UncleRepeated { hash: uncle_hash });
            }

            if ancestors.contains(&uncle_hash) {
                return Err(ValidationError::UncleIsAncestor { hash: uncle_hash });
            }
            if used.contains(&uncle_hash) {
                return Err(ValidationError::UncleAlreadyUsed { hash: uncle_hash });
            }

            // validateUncleParent: the uncle's parent must itself be one of the
            // block's ancestors -- that is what makes it an uncle rather than a
            // block from an unrelated fork -- and the uncle must satisfy the
            // parent-dependent rules against it.
            let parent = self
                .store
                .header(uncle.parent_hash)
                .filter(|_| ancestors.contains(&uncle.parent_hash))
                .ok_or(ValidationError::UncleHasNoCommonParent { hash: uncle_hash })?;
            for rule in self.parent_rules {
                rule.validate_with_parent(uncle, &parent)?;
            }
        }

        Ok(())
    }
}
