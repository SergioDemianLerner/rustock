/// RSK account state RLP encoding.
///
/// Accounts are stored in the Unitrie as RLP lists:
/// - `[nonce, balance]` when stateFlags == 0
/// - `[nonce, balance, stateFlags]` otherwise
///
/// Unlike Ethereum, RSK does not store storageRoot or codeHash in the account.
/// These are derived from the Unitrie key layout instead.
use alloy_primitives::U256;
use alloy_rlp::{Decodable, Encodable};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AccountState {
    pub nonce: U256,
    pub balance: U256,
    pub state_flags: u32,
    /// RSKIP-TBD: the block in which this account's `(nonce, balance,
    /// state_flags)` last changed, or 0 for "not modified since the rule
    /// activated" — which is *not* the same as "never modified".
    pub last_modified_block: u64,
    /// RSKIP-TBD: how many further modifications happened in
    /// `last_modified_block`, beyond the first. Zero for the overwhelmingly
    /// common case of one, which is why it counts from zero rather than one:
    /// a zero is a trailing element and is dropped from the encoding.
    pub intra_block_count: u64,
}

impl AccountState {
    pub fn new(nonce: U256, balance: U256) -> Self {
        Self { nonce, balance, ..Default::default() }
    }

    /// The part of the record the witness fields describe.
    ///
    /// Excluding them is not an optimisation: a witness that counted its own
    /// updates would be circular, and every modification would look like two.
    pub fn subject(&self) -> (U256, U256, u32) {
        (self.nonce, self.balance, self.state_flags)
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        // nonce: rskj `RLP.encodeBigInteger` — zero is RLP-empty (0x80),
        // non-zero is the unsigned minimal big-endian bytes (standard RLP).
        let nonce_enc = encode_u256(&self.nonce);
        // balance: rskj `RLP.encodeSignedCoinNonNullZero` — zero is the single
        // byte 0x00 (NOT RLP-empty), non-zero is the SIGNED big-endian bytes
        // (`Coin.getBytes` = `BigInteger.toByteArray`, leading 0x00 when the top
        // bit is set). This differs from `nonce`/standard RLP and is required
        // for the unitrie account record to match rskj byte-for-byte.
        let balance_enc = encode_coin_nonnull_zero(&self.balance);

        // Everything after `balance` is optional and is dropped while the
        // last element is zero. `state_flags` already behaved this way; the
        // two witness fields extend the same rule rather than adding a
        // special case, which is also what makes this encoder produce
        // byte-identical records to the pre-activation one whenever the
        // witness fields are unset.
        let tail: [(bool, Vec<u8>); 3] = [
            (self.state_flags != 0, encode_u32(self.state_flags)),
            (self.last_modified_block != 0, encode_u64(self.last_modified_block)),
            (self.intra_block_count != 0, encode_u64(self.intra_block_count)),
        ];
        let mut keep = tail.len();
        while keep > 0 && !tail[keep - 1].0 {
            keep -= 1;
        }

        let total = nonce_enc.len()
            + balance_enc.len()
            + tail[..keep].iter().map(|(_, e)| e.len()).sum::<usize>();
        alloy_rlp::Header { list: true, payload_length: total }.encode(&mut buf);
        buf.extend_from_slice(&nonce_enc);
        buf.extend_from_slice(&balance_enc);
        for (_, e) in &tail[..keep] {
            buf.extend_from_slice(e);
        }
        buf
    }

    pub fn decode(data: &[u8]) -> anyhow::Result<Self> {
        let mut cursor = data;
        let header = alloy_rlp::Header::decode(&mut cursor)?;
        anyhow::ensure!(header.list, "AccountState must be an RLP list");

        let nonce = decode_u256(&mut cursor)?;
        // balance uses the NonNullZero/signed encoding; read it leniently
        // (accepts 0x00, signed leading-zero bytes, and the legacy 0x80).
        let balance = decode_coin(&mut cursor)?;

        let flags_present = !cursor.is_empty();
        let state_flags = if flags_present { decode_u32(&mut cursor)? } else { 0 };

        let lmb_present = !cursor.is_empty();
        let last_modified_block = if lmb_present { decode_u64(&mut cursor)? } else { 0 };

        let ibc_present = !cursor.is_empty();
        let intra_block_count = if ibc_present { decode_u64(&mut cursor)? } else { 0 };

        // Canonicality: one account state must have exactly one encoding, or
        // it has two node hashes and two state roots.
        //
        // This is enforced only for the two elements this rule adds. The
        // legacy `[nonce, balance, 0]` form stays acceptable even though
        // nothing produces it, because tightening the decoder on a shape the
        // chain may already contain would reject history -- a rule stricter
        // than the chain it must accept is a fork, and this one would be
        // discovered only by a node resyncing from genesis.
        if ibc_present {
            anyhow::ensure!(
                intra_block_count != 0,
                "non-canonical account record: trailing zero intraBlockCount"
            );
        } else if lmb_present {
            anyhow::ensure!(
                last_modified_block != 0,
                "non-canonical account record: trailing zero lastModifiedBlock"
            );
        }

        Ok(Self { nonce, balance, state_flags, last_modified_block, intra_block_count })
    }
}

fn encode_u256(v: &U256) -> Vec<u8> {
    let mut buf = Vec::new();
    v.encode(&mut buf);
    buf
}

/// rskj `RLP.encodeSignedCoinNonNullZero`: zero balance is the single raw byte
/// `0x00`; a non-zero balance is `RLP.encodeElement(Coin.getBytes())` where
/// `Coin.getBytes()` is `BigInteger.toByteArray()` (signed two's-complement, so
/// a leading `0x00` is prepended when the most-significant bit is set).
fn encode_coin_nonnull_zero(balance: &U256) -> Vec<u8> {
    if balance.is_zero() {
        return vec![0x00];
    }
    let be = balance.to_be_bytes::<32>();
    let start = be.iter().position(|b| *b != 0).unwrap();
    let mut signed = Vec::with_capacity(33 - start);
    if be[start] & 0x80 != 0 {
        signed.push(0);
    }
    signed.extend_from_slice(&be[start..]);
    let mut out = Vec::new();
    signed.as_slice().encode(&mut out);
    out
}

/// Read one RLP byte-string item and interpret it as an unsigned big-endian
/// integer (leading zeros, signed-sign bytes, and the legacy RLP-empty form all
/// decode to the same value). Inverse of `encode_coin_nonnull_zero`.
fn decode_coin(cursor: &mut &[u8]) -> anyhow::Result<U256> {
    let first = *cursor.first().ok_or_else(|| anyhow::anyhow!("coin: empty"))?;
    let bytes: &[u8] = if first < 0x80 {
        let b = &cursor[..1];
        *cursor = &cursor[1..];
        b
    } else if first < 0xb8 {
        let len = (first - 0x80) as usize;
        anyhow::ensure!(cursor.len() > len, "coin: short string truncated");
        let b = &cursor[1..1 + len];
        *cursor = &cursor[1 + len..];
        b
    } else {
        let ll = (first - 0xb7) as usize;
        anyhow::ensure!(cursor.len() > ll, "coin: long header truncated");
        let len = cursor[1..1 + ll].iter().fold(0usize, |a, &x| (a << 8) | x as usize);
        anyhow::ensure!(cursor.len() >= 1 + ll + len, "coin: long string truncated");
        let b = &cursor[1 + ll..1 + ll + len];
        *cursor = &cursor[1 + ll + len..];
        b
    };
    let start = bytes.iter().position(|&b| b != 0).unwrap_or(bytes.len());
    anyhow::ensure!(bytes.len() - start <= 32, "coin: value exceeds 32 bytes");
    Ok(U256::from_be_slice(&bytes[start..]))
}

fn encode_u32(v: u32) -> Vec<u8> {
    let mut buf = Vec::new();
    let u = U256::from(v);
    u.encode(&mut buf);
    buf
}

fn encode_u64(v: u64) -> Vec<u8> {
    let mut buf = Vec::new();
    U256::from(v).encode(&mut buf);
    buf
}

fn decode_u256(cursor: &mut &[u8]) -> anyhow::Result<U256> {
    Ok(U256::decode(cursor)?)
}

fn decode_u64(cursor: &mut &[u8]) -> anyhow::Result<u64> {
    let v = U256::decode(cursor)?;
    anyhow::ensure!(v <= U256::from(u64::MAX), "account field exceeds u64");
    Ok(v.to::<u64>())
}

fn decode_u32(cursor: &mut &[u8]) -> anyhow::Result<u32> {
    let v = U256::decode(cursor)?;
    Ok(v.to::<u32>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_decode_basic() {
        let acct = AccountState::new(U256::from(42), U256::from(1_000_000));
        let encoded = acct.encode();
        let decoded = AccountState::decode(&encoded).unwrap();
        assert_eq!(decoded, acct);
    }

    #[test]
    fn test_encode_decode_with_flags() {
        let acct = AccountState {
            nonce: U256::from(1),
            balance: U256::from(2),
            state_flags: 3,
            ..Default::default()
        };
        let encoded = acct.encode();
        let decoded = AccountState::decode(&encoded).unwrap();
        assert_eq!(decoded, acct);
    }

    #[test]
    fn test_encode_decode_zero() {
        let acct = AccountState::default();
        let encoded = acct.encode();
        let decoded = AccountState::decode(&encoded).unwrap();
        assert_eq!(decoded, acct);
    }

    #[test]
    fn test_no_flags_when_zero() {
        let acct = AccountState::new(U256::from(1), U256::from(2));
        let encoded = acct.encode();
        // With flags=0, the list should have 2 items, not 3
        let acct_with_flags = AccountState {
            nonce: U256::from(1),
            balance: U256::from(2),
            state_flags: 1,
            ..Default::default()
        };
        let encoded_with_flags = acct_with_flags.encode();
        assert!(encoded.len() < encoded_with_flags.len());
    }

    /// Ground truth from rskj's unitrie (mainnet #3397, dumped via
    /// `co.rsk.cli.tools.ExportState`): a zero-balance account encodes the
    /// balance as the single byte `0x00` (`RLP.encodeSignedCoinNonNullZero`),
    /// NOT the RLP-empty `0x80`. The REMASC contract (nonce 0, balance 0) is
    /// `c28000`; a fresh EOA (nonce 1, balance 0) is `c20100`.
    #[test]
    fn rskj_nonnull_zero_balance_encoding() {
        assert_eq!(
            alloy_primitives::hex::encode(AccountState::new(U256::ZERO, U256::ZERO).encode()),
            "c28000",
            "nonce 0 / balance 0 (REMASC contract record)"
        );
        assert_eq!(
            alloy_primitives::hex::encode(AccountState::new(U256::from(1), U256::ZERO).encode()),
            "c20100",
            "nonce 1 / balance 0 (fresh EOA)"
        );
    }

    /// rskj `Coin.getBytes()` is `BigInteger.toByteArray()` (signed), so a
    /// balance whose minimal big-endian top byte has its high bit set gets a
    /// leading `0x00`. Balance 128 (0x80) → element `82 00 80`.
    #[test]
    fn signed_balance_high_bit_gets_zero_prefix() {
        assert_eq!(
            alloy_primitives::hex::encode(AccountState::new(U256::ZERO, U256::from(128)).encode()),
            "c480820080"
        );
        // round-trips back to the same value
        let enc = AccountState::new(U256::from(7), U256::from(128)).encode();
        assert_eq!(AccountState::decode(&enc).unwrap(), AccountState::new(U256::from(7), U256::from(128)));
    }

    /// Decoding tolerates the NonNullZero form (0x00), signed leading zeros,
    /// and the legacy RLP-empty form (0x80) — all map to the same value.
    #[test]
    fn decode_coin_tolerates_encodings() {
        // c28000 (NonNullZero zero) and the legacy c28080 both decode to (0,0).
        assert_eq!(AccountState::decode(&alloy_primitives::hex::decode("c28000").unwrap()).unwrap(), AccountState::new(U256::ZERO, U256::ZERO));
        assert_eq!(AccountState::decode(&alloy_primitives::hex::decode("c28080").unwrap()).unwrap(), AccountState::new(U256::ZERO, U256::ZERO));
    }

    #[test]
    fn test_large_balance() {
        let balance = U256::from(10u64).pow(U256::from(18)); // 1 ETH/RBTC in wei
        let acct = AccountState::new(U256::ZERO, balance);
        let encoded = acct.encode();
        let decoded = AccountState::decode(&encoded).unwrap();
        assert_eq!(decoded.balance, balance);
    }
}

#[cfg(test)]
mod witness_tests {
    use super::*;

    /// The property the whole lazy activation rests on: an account whose
    /// witness fields are unset must encode to the bytes it encoded to
    /// before this rule existed. If this fails, activation rewrites the
    /// world instead of nothing.
    #[test]
    fn unset_witness_is_byte_identical_to_the_legacy_form() {
        // rskj ground truth, same vectors as the test above.
        let remasc = AccountState::default();
        assert_eq!(remasc.encode(), vec![0xc2, 0x80, 0x00]);

        let eoa = AccountState::new(U256::from(1), U256::ZERO);
        assert_eq!(eoa.encode(), vec![0xc2, 0x01, 0x00]);
    }

    #[test]
    fn witness_round_trips() {
        let acct = AccountState {
            nonce: U256::from(88),
            balance: U256::from(1_349_000_000_000_000u64),
            state_flags: 0,
            last_modified_block: 9_407_088,
            intra_block_count: 2,
        };
        assert_eq!(AccountState::decode(&acct.encode()).unwrap(), acct);
    }

    /// A zero `state_flags` must be emitted explicitly once a later element
    /// is present: RLP lists are positional, so dropping it would shift
    /// `lastModifiedBlock` into the flags slot.
    #[test]
    fn a_zero_state_flags_is_emitted_when_a_later_field_is_set() {
        let acct = AccountState {
            nonce: U256::from(1),
            balance: U256::from(2),
            state_flags: 0,
            last_modified_block: 500,
            intra_block_count: 0,
        };
        let enc = acct.encode();
        let decoded = AccountState::decode(&enc).unwrap();
        assert_eq!(decoded, acct);
        assert_eq!(decoded.state_flags, 0);
        assert_eq!(decoded.last_modified_block, 500);
    }

    /// The common case: one modification in the last block, so
    /// `intra_block_count` is zero and disappears.
    #[test]
    fn a_single_modification_drops_the_count_element() {
        let one = AccountState {
            last_modified_block: 500,
            ..AccountState::new(U256::from(1), U256::from(2))
        };
        let many = AccountState { intra_block_count: 1, ..one.clone() };
        assert!(
            one.encode().len() < many.encode().len(),
            "a zero count must be omitted, not emitted"
        );
    }

    #[test]
    fn a_trailing_zero_witness_field_is_rejected() {
        // [nonce, balance, flags, lastModified, 0] -- two encodings of one
        // state is two state roots.
        let mut buf = Vec::new();
        let body = [
            encode_u256(&U256::from(1)),
            encode_coin_nonnull_zero(&U256::from(2)),
            encode_u32(0),
            encode_u64(500),
            encode_u64(0),
        ]
        .concat();
        alloy_rlp::Header { list: true, payload_length: body.len() }.encode(&mut buf);
        buf.extend_from_slice(&body);
        assert!(AccountState::decode(&buf).is_err(), "trailing zero must not decode");

        // Same for a trailing zero lastModifiedBlock.
        let mut buf = Vec::new();
        let body = [
            encode_u256(&U256::from(1)),
            encode_coin_nonnull_zero(&U256::from(2)),
            encode_u32(3),
            encode_u64(0),
        ]
        .concat();
        alloy_rlp::Header { list: true, payload_length: body.len() }.encode(&mut buf);
        buf.extend_from_slice(&body);
        assert!(AccountState::decode(&buf).is_err());
    }

    /// Leniency the chain may rely on is preserved: a legacy three-element
    /// record with a zero `state_flags` still decodes. Our encoder never
    /// produces one, but refusing it would be a rule stricter than the
    /// history it must accept.
    #[test]
    fn a_legacy_zero_state_flags_record_still_decodes() {
        let mut buf = Vec::new();
        let body = [
            encode_u256(&U256::from(1)),
            encode_coin_nonnull_zero(&U256::from(2)),
            encode_u32(0),
        ]
        .concat();
        alloy_rlp::Header { list: true, payload_length: body.len() }.encode(&mut buf);
        buf.extend_from_slice(&body);
        let decoded = AccountState::decode(&buf).expect("legacy form must stay acceptable");
        assert_eq!(decoded.state_flags, 0);
        assert_eq!(decoded.last_modified_block, 0);
    }
}
