//! The one piece of metadata FlatDB persists.
//!
//! Everything needed to interpret the heap and the index after a restart
//! lives in a single 48-byte record, written into a fixed two-slot file and
//! alternated between the slots on each write. The previous checkpoint is
//! therefore always intact while the next one is being written, and each
//! slot carries a CRC so a slot torn by a power failure is discarded rather
//! than believed.
//!
//! One record rather than two is the point. The original FlatDB kept a heap
//! descriptor and an index header separately, which meant two things had to
//! become durable in a particular order; one record has no order to get
//! wrong. Recovery needs no log, no undo and no scan — the newest slot that
//! verifies wins.

use std::io;
use std::path::Path;

/// Each slot gets a whole 4 KiB page, so the two never share one.
pub const SLOT_SIZE: u64 = 4096;
pub const FILE_SIZE: u64 = 2 * SLOT_SIZE;
pub const FILE_NAME: &str = "checkpoint.dat";

const MAGIC: u32 = 0x464C_4154; // "FLAT"
const VERSION: u32 = 1;
/// magic, version, sequence, heap_committed_size, index_capacity,
/// index_size, segment_size.
const BODY_LENGTH: usize = 4 + 4 + 8 + 8 + 8 + 8 + 8;
const RECORD_LENGTH: usize = BODY_LENGTH + 8;

/// What the store looked like at the last completed flush.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Checkpoint {
    /// Higher wins. Also selects the slot: `sequence & 1`.
    pub sequence: u64,
    /// Heap bytes known to be committed. Anything at or beyond this offset
    /// was written by a flush that did not finish, and is invisible.
    pub heap_committed_size: u64,
    /// Slots in the index. Always a power of two, and part of the index
    /// file's name, so a grow that is interrupted cannot be half-adopted.
    pub index_capacity: u64,
    /// Live entries in the index.
    pub index_size: u64,
    /// Heap segment size this database was written with.
    ///
    /// Every offset is `segment * segment_size + position`, so opening a
    /// database with a different one would resolve every record to the
    /// wrong bytes. Recording it turns that from silent corruption into a
    /// refusal to open. The Java version hardcodes the constant and has no
    /// way to notice.
    pub segment_size: u64,
}

impl Checkpoint {
    pub fn initial() -> Self {
        Self {
            sequence: 0,
            heap_committed_size: 0,
            index_capacity: 0,
            index_size: 0,
            segment_size: 0,
        }
    }

    pub fn next(
        &self,
        heap_committed_size: u64,
        index_capacity: u64,
        index_size: u64,
        segment_size: u64,
    ) -> Self {
        Self {
            sequence: self.sequence + 1,
            heap_committed_size,
            index_capacity,
            index_size,
            segment_size,
        }
    }

    fn encode(&self) -> [u8; RECORD_LENGTH] {
        let mut out = [0u8; RECORD_LENGTH];
        out[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        out[4..8].copy_from_slice(&VERSION.to_le_bytes());
        out[8..16].copy_from_slice(&self.sequence.to_le_bytes());
        out[16..24].copy_from_slice(&self.heap_committed_size.to_le_bytes());
        out[24..32].copy_from_slice(&self.index_capacity.to_le_bytes());
        out[32..40].copy_from_slice(&self.index_size.to_le_bytes());
        out[40..48].copy_from_slice(&self.segment_size.to_le_bytes());
        let crc = crc32fast::hash(&out[..BODY_LENGTH]) as u64;
        out[48..56].copy_from_slice(&crc.to_le_bytes());
        out
    }

    fn decode(slot: &[u8]) -> Option<Self> {
        if slot.len() < RECORD_LENGTH {
            return None;
        }
        let u32_at = |i: usize| u32::from_le_bytes(slot[i..i + 4].try_into().expect("4 bytes"));
        let u64_at = |i: usize| u64::from_le_bytes(slot[i..i + 8].try_into().expect("8 bytes"));
        if u32_at(0) != MAGIC || u32_at(4) != VERSION {
            return None;
        }
        if u64_at(48) != crc32fast::hash(&slot[..BODY_LENGTH]) as u64 {
            return None;
        }
        Some(Self {
            sequence: u64_at(8),
            heap_committed_size: u64_at(16),
            index_capacity: u64_at(24),
            index_size: u64_at(32),
            segment_size: u64_at(40),
        })
    }

    /// Writes into the slot the sequence number selects and fsyncs. The
    /// other slot, holding the previous checkpoint, is left untouched —
    /// which is the whole mechanism.
    pub fn write(&self, dir: &Path) -> io::Result<()> {
        use std::io::{Seek, SeekFrom, Write};
        let path = dir.join(FILE_NAME);
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)?;
        if file.metadata()?.len() < FILE_SIZE {
            file.set_len(FILE_SIZE)?;
        }
        file.seek(SeekFrom::Start((self.sequence & 1) * SLOT_SIZE))?;
        file.write_all(&self.encode())?;
        file.sync_all()
    }

    /// The newest checkpoint that still verifies, or the initial one when
    /// the file is absent or both slots are unreadable.
    ///
    /// Opening empty rather than failing is deliberate: a store whose
    /// metadata is gone holds nodes nobody can address, and for a trie that
    /// is recoverable from peers. Refusing to open would turn a resyncable
    /// state into an operator incident.
    pub fn read(dir: &Path) -> io::Result<Self> {
        let path = dir.join(FILE_NAME);
        if !path.exists() {
            return Ok(Self::initial());
        }
        let bytes = std::fs::read(&path)?;
        let mut best: Option<Self> = None;
        for i in 0..2usize {
            let from = i * SLOT_SIZE as usize;
            if from + RECORD_LENGTH > bytes.len() {
                continue;
            }
            if let Some(candidate) = Self::decode(&bytes[from..from + RECORD_LENGTH]) {
                if best.is_none_or(|b| candidate.sequence > b.sequence) {
                    best = Some(candidate);
                }
            }
        }
        Ok(best.unwrap_or_else(Self::initial))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(sequence: u64) -> Checkpoint {
        Checkpoint {
            sequence,
            heap_committed_size: 1234 + sequence,
            index_capacity: 1024,
            index_size: 7 + sequence,
            segment_size: 1 << 29,
        }
    }

    #[test]
    fn a_checkpoint_round_trips_through_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let written = sample(3);
        written.write(dir.path()).unwrap();
        assert_eq!(Checkpoint::read(dir.path()).unwrap(), written);
    }

    #[test]
    fn an_absent_file_reads_as_the_initial_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(Checkpoint::read(dir.path()).unwrap(), Checkpoint::initial());
    }

    /// The alternation is the whole mechanism: consecutive writes must land
    /// in different slots, or a torn write destroys the only copy.
    #[test]
    fn consecutive_writes_alternate_slots() {
        let dir = tempfile::tempdir().unwrap();
        sample(4).write(dir.path()).unwrap();
        sample(5).write(dir.path()).unwrap();
        let bytes = std::fs::read(dir.path().join(FILE_NAME)).unwrap();

        let first = Checkpoint::decode(&bytes[..RECORD_LENGTH]).expect("slot 0 is intact");
        let second = Checkpoint::decode(&bytes[SLOT_SIZE as usize..][..RECORD_LENGTH])
            .expect("slot 1 is intact");
        assert_eq!(first.sequence, 4, "an even sequence lives in slot 0");
        assert_eq!(second.sequence, 5, "an odd sequence lives in slot 1");
        assert_eq!(Checkpoint::read(dir.path()).unwrap().sequence, 5, "the newer one wins");
    }

    /// A torn newest slot must fall back to the previous one rather than
    /// taking the damaged bytes at face value.
    #[test]
    fn a_torn_slot_falls_back_to_the_previous_one() {
        let dir = tempfile::tempdir().unwrap();
        sample(4).write(dir.path()).unwrap();
        sample(5).write(dir.path()).unwrap();

        let path = dir.path().join(FILE_NAME);
        let mut bytes = std::fs::read(&path).unwrap();
        // Slot 1 holds sequence 5.
        for byte in bytes[SLOT_SIZE as usize + 20..SLOT_SIZE as usize + 40].iter_mut() {
            *byte ^= 0x5A;
        }
        std::fs::write(&path, &bytes).unwrap();

        assert_eq!(Checkpoint::read(dir.path()).unwrap(), sample(4));
    }

    #[test]
    fn both_slots_destroyed_reads_as_empty_rather_than_failing() {
        let dir = tempfile::tempdir().unwrap();
        sample(4).write(dir.path()).unwrap();
        sample(5).write(dir.path()).unwrap();
        std::fs::write(dir.path().join(FILE_NAME), vec![0xFFu8; FILE_SIZE as usize]).unwrap();
        assert_eq!(Checkpoint::read(dir.path()).unwrap(), Checkpoint::initial());
    }

    /// Every field has to survive, not just the ones a smoke test reads.
    #[test]
    fn every_field_survives_encoding() {
        let c = Checkpoint {
            sequence: u64::MAX / 3,
            heap_committed_size: 0x0123_4567_89AB_CDEF,
            index_capacity: 1 << 30,
            index_size: 999_999_999,
            segment_size: 1 << 29,
        };
        assert_eq!(Checkpoint::decode(&c.encode()), Some(c));
    }

    #[test]
    fn a_single_flipped_bit_anywhere_in_the_body_is_caught() {
        let encoded = sample(9).encode();
        for byte in 0..BODY_LENGTH {
            for bit in 0..8 {
                let mut damaged = encoded;
                damaged[byte] ^= 1 << bit;
                assert_eq!(
                    Checkpoint::decode(&damaged),
                    None,
                    "byte {byte} bit {bit} went undetected"
                );
            }
        }
    }
}
