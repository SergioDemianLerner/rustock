use alloy_primitives::{U256, B256};
use rustock_core::types::header::Header;
use serde::Serialize;

/// Formats a u64 as a hex string with 0x prefix (no leading zeros except for 0x0).
pub fn to_hex_u64(v: u64) -> String {
    format!("{:#x}", v)
}

/// Formats a U256 as a hex string with 0x prefix.
pub fn to_hex_u256(v: &U256) -> String {
    if v.is_zero() {
        "0x0".to_string()
    } else {
        format!("{:#x}", v)
    }
}

/// Formats a B256 as a 0x-prefixed lowercase hex string.
pub fn to_hex_b256(v: &B256) -> String {
    format!("{:#x}", v)
}

/// Formats raw bytes as 0x-prefixed hex.
pub fn to_hex_bytes(v: &[u8]) -> String {
    format!("0x{}", hex::encode(v))
}

/// Parses a 0x-prefixed hex string to a B256. Returns None on failure.
pub fn parse_b256(s: &str) -> Option<B256> {
    s.parse::<B256>().ok()
}

/// Parses a block number from a hex string or special values ("latest", "earliest", "pending").
/// Returns the resolved block number or None.
pub fn parse_block_number(s: &str, head_number: u64) -> Option<u64> {
    match s {
        "latest" | "pending" => Some(head_number),
        "earliest" => Some(0),
        hex_str => {
            let stripped = hex_str.strip_prefix("0x").unwrap_or(hex_str);
            u64::from_str_radix(stripped, 16).ok()
        }
    }
}

/// Parses a 0x-prefixed hex index. Returns None on anything malformed, which
/// callers report as an invalid parameter -- rskj's `HexIndexParam` throws
/// `invalidParamError` for the same inputs, before the method body runs.
pub fn parse_hex_u32(s: &str) -> Option<u32> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    u32::from_str_radix(s, 16).ok()
}

/// Block result DTO matching rskj's `BlockResultDTO` JSON format.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockResultDto {
    pub number: String,
    pub hash: String,
    pub parent_hash: String,
    #[serde(rename = "sha3Uncles")]
    pub sha3_uncles: String,
    pub miner: String,
    pub state_root: String,
    pub transactions_root: String,
    pub receipts_root: String,
    pub logs_bloom: String,
    pub difficulty: String,
    pub total_difficulty: String,
    pub gas_limit: String,
    pub gas_used: String,
    pub timestamp: String,
    pub extra_data: String,
    pub minimum_gas_price: String,
    pub transactions: Vec<serde_json::Value>,
    pub uncles: Vec<serde_json::Value>,
    pub size: String,
}

impl BlockResultDto {
    pub fn from_header(header: &Header, hash: B256, total_difficulty: U256) -> Self {
        Self::from_header_with_body(header, hash, total_difficulty, None, false)
    }

    pub fn from_header_with_body(
        header: &Header,
        hash: B256,
        total_difficulty: U256,
        body: Option<&(Vec<rustock_core::Transaction>, Vec<Header>)>,
        full_txs: bool,
    ) -> Self {
        let size = encoded_block_len(header, body);

        let transactions = if let Some((txs, _)) = body {
            txs.iter()
                .enumerate()
                .map(|(i, tx)| {
                    if full_txs {
                        tx_to_json(tx, &hash, header.number, i)
                    } else {
                        let h = tx_hash(tx);
                        serde_json::Value::String(to_hex_b256(&h))
                    }
                })
                .collect()
        } else {
            vec![]
        };

        let uncles = if let Some((_, ommers)) = body {
            ommers.iter().map(|o| {
                serde_json::Value::String(to_hex_b256(&o.hash()))
            }).collect()
        } else {
            vec![]
        };

        Self {
            number: to_hex_u64(header.number),
            hash: to_hex_b256(&hash),
            parent_hash: to_hex_b256(&header.parent_hash),
            sha3_uncles: to_hex_b256(&header.ommers_hash),
            miner: format!("{:#x}", header.beneficiary),
            state_root: to_hex_b256(&header.state_root),
            transactions_root: to_hex_b256(&header.transactions_root),
            receipts_root: to_hex_b256(&header.receipts_root),
            logs_bloom: to_hex_bytes(header.logs_bloom.as_ref()),
            difficulty: to_hex_u256(&header.difficulty),
            total_difficulty: to_hex_u256(&total_difficulty),
            gas_limit: to_hex_u256(&header.gas_limit),
            gas_used: to_hex_u64(header.gas_used),
            timestamp: to_hex_u64(header.timestamp),
            extra_data: to_hex_bytes(&header.extra_data),
            minimum_gas_price: to_hex_u256(&header.minimum_gas_price),
            transactions,
            uncles,
            size: to_hex_u64(size),
        }
    }
}

/// Length of the block's RLP encoding, which is what `size` reports.
///
/// rskj answers with `block.getEncoded().length` -- the whole block, i.e. the
/// RLP list `[header, transactions, uncles]`, not the header alone. Measuring
/// only the header made `size` short by the entire transaction list, so a
/// block carrying 40 KB of transactions reported ~600 bytes. Callers use this
/// to budget bandwidth and to sanity-check a block they have fetched, and both
/// uses are defeated by a number that ignores the payload.
///
/// A block whose body is absent (header-only, or an uncle the node never
/// stored) encodes as `[header, [], []]`, which is what rskj also produces for
/// that case: it synthesises a body-less block from the header.
fn encoded_block_len(
    header: &Header,
    body: Option<&(Vec<rustock_core::Transaction>, Vec<Header>)>,
) -> u64 {
    use alloy_rlp::{Encodable, Header as RlpHeader};

    let mut header_rlp = Vec::new();
    header.encode(&mut header_rlp);

    let (txs_payload, ommers_payload) = match body {
        Some((txs, ommers)) => {
            let txs_len: usize = txs.iter().map(|tx| tx.rlp_for_trie().len()).sum();
            let mut ommers_len = 0usize;
            for o in ommers {
                let mut buf = Vec::new();
                o.encode(&mut buf);
                ommers_len += buf.len();
            }
            (txs_len, ommers_len)
        }
        None => (0, 0),
    };

    let payload = header_rlp.len()
        + RlpHeader { list: true, payload_length: txs_payload }.length()
        + txs_payload
        + RlpHeader { list: true, payload_length: ommers_payload }.length()
        + ommers_payload;

    (RlpHeader { list: true, payload_length: payload }.length() + payload) as u64
}

/// Compute the keccak256 hash of a transaction's RLP encoding.
/// The canonical transaction hash.
///
/// Must be `Transaction::tx_hash()`, which hashes `rlp_for_trie()` -- the
/// ORIGINAL bytes when they were cached at decode time. Re-encoding here
/// instead produced a different hash for every transaction whose RLP does not
/// round-trip byte-for-byte (~42% of mainnet transactions when measured), so
/// `eth_getBlockBy*` reported hashes that `eth_getTransactionByHash` and
/// `eth_getTransactionReceipt` could not then find, because the index is keyed
/// by the canonical hash.
fn tx_hash(tx: &rustock_core::Transaction) -> B256 {
    tx.tx_hash()
}

/// Format a transaction as a JSON object for `eth_getBlockBy*` (full tx mode).
fn tx_to_json(tx: &rustock_core::Transaction, block_hash: &B256, block_number: u64, index: usize) -> serde_json::Value {
    let hash = tx_hash(tx);
    serde_json::json!({
        "hash": to_hex_b256(&hash),
        "nonce": to_hex_u64(tx.nonce),
        "blockHash": to_hex_b256(block_hash),
        "blockNumber": to_hex_u64(block_number),
        "transactionIndex": to_hex_u64(index as u64),
        "from": "0x0000000000000000000000000000000000000000",
        "to": to_hex_bytes(&tx.to),
        "value": to_hex_u256(&tx.value),
        "gasPrice": to_hex_u256(&tx.gas_price),
        "gas": to_hex_u256(&tx.gas_limit),
        "input": to_hex_bytes(&tx.input),
        "v": to_hex_u64(tx.v),
        "r": to_hex_u256(&tx.r),
        "s": to_hex_u256(&tx.s),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_to_hex_u64() {
        assert_eq!(to_hex_u64(0), "0x0");
        assert_eq!(to_hex_u64(255), "0xff");
        assert_eq!(to_hex_u64(4444), "0x115c");
    }

    #[test]
    fn test_to_hex_u256() {
        assert_eq!(to_hex_u256(&U256::ZERO), "0x0");
        assert_eq!(to_hex_u256(&U256::from(0xff)), "0xff");
    }

    #[test]
    fn test_parse_block_number() {
        assert_eq!(parse_block_number("latest", 100), Some(100));
        assert_eq!(parse_block_number("earliest", 100), Some(0));
        assert_eq!(parse_block_number("pending", 100), Some(100));
        assert_eq!(parse_block_number("0x0", 100), Some(0));
        assert_eq!(parse_block_number("0xff", 100), Some(255));
    }

    #[test]
    fn test_parse_b256() {
        let hash = B256::repeat_byte(0xaa);
        let parsed = parse_b256(&format!("{:#x}", hash));
        assert_eq!(parsed, Some(hash));
        assert_eq!(parse_b256("not_a_hash"), None);
    }
}
