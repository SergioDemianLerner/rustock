//! RSKIP-351 block header extensions.
//!
//! A V1/V2 header does not carry its own 256-byte logs bloom. It carries, in
//! the bloom's slot, a short commitment:
//!
//! ```text
//! extension_data = RLP([ version, extension.hash() ])
//! ```
//!
//! and the extension body travels separately. That is what makes a header
//! small enough to be worth serving in bulk, and it is why the *hash* and the
//! *encoding* of an extension are different functions: the hash commits to
//! `keccak256(logs_bloom)` rather than to the bloom itself, so a peer can
//! verify the commitment without being sent 256 bytes it may already have.
//!
//! Translated from rskj `org.ethereum.core.BlockHeaderExtension`,
//! `BlockHeaderExtensionV1` and `BlockHeaderExtensionV2`. Byte-for-byte
//! compatibility is the whole point of this module, so the field order and the
//! two encodings below follow rskj's `addElementsEncoded` override order
//! exactly — V2 inserts `base_event` *before* V1's `edges`, because
//! `BlockHeaderExtensionV2.addElementsEncoded` adds its own field first and
//! then calls `super`.

use alloy_primitives::{keccak256, B256};
use alloy_rlp::{Encodable, Header as RlpHeader};

/// Maximum size of the V2 `base_event` field.
///
/// rskj `BlockHeaderV2.BASE_EVENT_MAX_SIZE`; exceeding it is a
/// `FieldMaxSizeBlockHeaderException` there.
pub const BASE_EVENT_MAX_SIZE: usize = 128;

/// A parsed header extension body.
///
/// `edges` are the transaction-sublist boundaries of RSKIP-144; `None` and
/// `Some(vec![])` are **different** encodings in rskj — `None` omits the
/// element entirely, an empty slice emits an empty one — so the distinction is
/// preserved rather than normalised.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlockHeaderExtension {
    V1 {
        logs_bloom: Vec<u8>,
        edges: Option<Vec<i16>>,
    },
    V2 {
        logs_bloom: Vec<u8>,
        edges: Option<Vec<i16>>,
        base_event: Option<Vec<u8>>,
    },
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ExtensionError {
    #[error("invalid extension encoding: expected a 2-item list")]
    NotAPair,
    #[error("unknown extension version: {0}")]
    UnknownVersion(u8),
    #[error("malformed extension body")]
    Malformed,
    #[error("baseEvent length cannot exceed {BASE_EVENT_MAX_SIZE} bytes, got {0}")]
    BaseEventTooLong(usize),
}

/// rskj `ByteUtil.shortsToRLP`: pack as **little-endian** shorts, then encode
/// the packed bytes as one RLP element.
///
/// The endianness is not a detail. RLP is big-endian everywhere else, so a
/// reader who assumes the usual convention produces a different commitment and
/// a different block hash.
fn shorts_to_rlp(shorts: &[i16]) -> Vec<u8> {
    let mut packed = Vec::with_capacity(shorts.len() * 2);
    for s in shorts {
        packed.extend_from_slice(&s.to_le_bytes());
    }
    let mut out = Vec::new();
    packed.as_slice().encode(&mut out);
    out
}

/// rskj `ByteUtil.rlpToShorts`, applied to an already-decoded payload.
///
/// A trailing odd byte is dropped, which is what Java's `ShortBuffer.get`
/// does with a buffer that is not a whole number of shorts.
fn rlp_to_shorts(payload: &[u8]) -> Vec<i16> {
    payload
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect()
}

/// Encodes `items` (each already RLP-encoded) as one RLP list.
fn encode_list(items: &[Vec<u8>]) -> Vec<u8> {
    let payload_length = items.iter().map(|i| i.len()).sum();
    let mut out = Vec::new();
    RlpHeader { list: true, payload_length }.encode(&mut out);
    for i in items {
        out.extend_from_slice(i);
    }
    out
}

/// Encodes one byte the way rskj's `RLP.encodeByte` does.
fn encode_byte(b: u8) -> Vec<u8> {
    let mut out = Vec::new();
    if b == 0 {
        out.push(0x80);
    } else if b < 0x80 {
        out.push(b);
    } else {
        [b].as_slice().encode(&mut out);
    }
    out
}

fn encode_element(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    bytes.encode(&mut out);
    out
}

/// Splits an RLP list into its items' payloads.
///
/// Returns the decoded payload of each item, so an empty element yields an
/// empty slice rather than nothing — matching rskj's `getRLPRawData()`, whose
/// contract `BlockHeaderExtensionV2.fromEncoded` documents as "an empty
/// element decodes to byte[0] rather than null".
fn decode_list_items(encoded: &[u8]) -> Result<Vec<Vec<u8>>, ExtensionError> {
    let mut buf = encoded;
    let header = RlpHeader::decode(&mut buf).map_err(|_| ExtensionError::Malformed)?;
    if !header.list {
        return Err(ExtensionError::Malformed);
    }
    let mut body = &buf[..header.payload_length.min(buf.len())];
    let mut items = Vec::new();
    while !body.is_empty() {
        let h = RlpHeader::decode(&mut body).map_err(|_| ExtensionError::Malformed)?;
        if h.list {
            return Err(ExtensionError::Malformed);
        }
        if h.payload_length > body.len() {
            return Err(ExtensionError::Malformed);
        }
        items.push(body[..h.payload_length].to_vec());
        body = &body[h.payload_length..];
    }
    Ok(items)
}

impl BlockHeaderExtension {
    pub fn version(&self) -> u8 {
        match self {
            BlockHeaderExtension::V1 { .. } => 0x1,
            BlockHeaderExtension::V2 { .. } => 0x2,
        }
    }

    pub fn logs_bloom(&self) -> &[u8] {
        match self {
            BlockHeaderExtension::V1 { logs_bloom, .. }
            | BlockHeaderExtension::V2 { logs_bloom, .. } => logs_bloom,
        }
    }

    pub fn edges(&self) -> Option<&[i16]> {
        match self {
            BlockHeaderExtension::V1 { edges, .. } | BlockHeaderExtension::V2 { edges, .. } => {
                edges.as_deref()
            }
        }
    }

    pub fn base_event(&self) -> Option<&[u8]> {
        match self {
            BlockHeaderExtension::V1 { .. } => None,
            BlockHeaderExtension::V2 { base_event, .. } => base_event.as_deref(),
        }
    }

    /// The fields after the bloom, in rskj's `addElementsEncoded` order.
    ///
    /// V2 adds `base_event` and *then* delegates to V1 for `edges`, so a V2
    /// body is `[bloom, base_event, edges?]` — base_event is always present
    /// (empty when unset), edges only when `Some`.
    fn trailing_elements(&self) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        if let BlockHeaderExtension::V2 { base_event, .. } = self {
            out.push(encode_element(base_event.as_deref().unwrap_or(&[])));
        }
        if let Some(edges) = self.edges() {
            out.push(shorts_to_rlp(edges));
        }
        out
    }

    /// The extension body, as transmitted.
    pub fn encoded(&self) -> Vec<u8> {
        let mut items = vec![encode_element(self.logs_bloom())];
        items.extend(self.trailing_elements());
        encode_list(&items)
    }

    /// The body with the bloom replaced by `keccak256(bloom)`.
    ///
    /// This is what the header commits to. Hashing the bloom first is what
    /// lets a header be verified against an extension without the 256-byte
    /// bloom being sent alongside it.
    fn encoded_for_hash(&self) -> Vec<u8> {
        let mut items = vec![encode_element(keccak256(self.logs_bloom()).as_slice())];
        items.extend(self.trailing_elements());
        encode_list(&items)
    }

    pub fn hash(&self) -> B256 {
        keccak256(self.encoded_for_hash())
    }

    /// `RLP([version, body])` — the self-describing form, rskj
    /// `BlockHeaderExtension.toEncoded`.
    pub fn to_encoded(&self) -> Vec<u8> {
        encode_list(&[encode_byte(self.version()), encode_element(&self.encoded())])
    }

    /// What the header carries in the logs-bloom slot: `RLP([version, hash])`.
    ///
    /// rskj `BlockHeaderV1.createExtensionData`.
    pub fn create_extension_data(&self) -> Vec<u8> {
        encode_list(&[
            encode_byte(self.version()),
            encode_element(self.hash().as_slice()),
        ])
    }

    /// Inverse of [`to_encoded`](Self::to_encoded).
    pub fn from_encoded(encoded: &[u8]) -> Result<Self, ExtensionError> {
        let items = decode_list_items(encoded)?;
        if items.len() != 2 {
            return Err(ExtensionError::NotAPair);
        }
        // rskj treats an absent or empty version element as 0, then rejects it
        // for not being 1 or 2 -- so an empty version is an unknown version,
        // not a default.
        let version = items[0].first().copied().unwrap_or(0);
        match version {
            0x1 => Self::v1_from_body(&items[1]),
            0x2 => Self::v2_from_body(&items[1]),
            v => Err(ExtensionError::UnknownVersion(v)),
        }
    }

    fn v1_from_body(body: &[u8]) -> Result<Self, ExtensionError> {
        let items = decode_list_items(body)?;
        if items.is_empty() {
            return Err(ExtensionError::Malformed);
        }
        Ok(BlockHeaderExtension::V1 {
            logs_bloom: items[0].clone(),
            edges: (items.len() == 2).then(|| rlp_to_shorts(&items[1])),
        })
    }

    fn v2_from_body(body: &[u8]) -> Result<Self, ExtensionError> {
        let items = decode_list_items(body)?;
        if items.len() < 2 {
            return Err(ExtensionError::Malformed);
        }
        Ok(BlockHeaderExtension::V2 {
            logs_bloom: items[0].clone(),
            base_event: Some(items[1].clone()),
            edges: (items.len() == 3).then(|| rlp_to_shorts(&items[2])),
        })
    }

    /// rskj `BlockHeaderExtensionV2.setBaseEvent`, which refuses an
    /// over-long value rather than truncating it.
    pub fn set_base_event(&mut self, value: Option<Vec<u8>>) -> Result<(), ExtensionError> {
        if let Some(v) = &value {
            if v.len() > BASE_EVENT_MAX_SIZE {
                return Err(ExtensionError::BaseEventTooLong(v.len()));
            }
        }
        match self {
            BlockHeaderExtension::V2 { base_event, .. } => {
                *base_event = value;
                Ok(())
            }
            BlockHeaderExtension::V1 { .. } => Err(ExtensionError::UnknownVersion(0x1)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ported from rskj. Each test below names the case it came from, so a
    /// divergence can be taken back to the Java it was checked against:
    ///   co/rsk/core/BlockHeaderExtensionV1Test.java
    ///   org/ethereum/core/BlockHeaderExtensionV2Test.java
    ///   co/rsk/core/BlockHeaderExtensionTest.java
    const EDGES: [i16; 4] = [1, 2, 3, 4];

    fn bloom(tag: &[u8]) -> Vec<u8> {
        let mut b = vec![0u8; 256];
        b[..tag.len()].copy_from_slice(tag);
        b
    }

    fn v1(logs_bloom: Vec<u8>, edges: Option<Vec<i16>>) -> BlockHeaderExtension {
        BlockHeaderExtension::V1 { logs_bloom, edges }
    }

    fn v2(logs_bloom: Vec<u8>, edges: Option<Vec<i16>>, base_event: Option<Vec<u8>>) -> BlockHeaderExtension {
        BlockHeaderExtension::V2 { logs_bloom, edges, base_event }
    }

    // ---- V1 ---------------------------------------------------------------

    /// rskj `createWithLogsBloomAndEdges`
    #[test]
    fn v1_keeps_the_bloom_and_edges_it_was_given() {
        let b = bloom(&[1, 2, 3, 4]);
        let e = v1(b.clone(), Some(EDGES.to_vec()));
        assert_eq!(e.logs_bloom(), &b[..]);
        assert_eq!(e.edges(), Some(&EDGES[..]));
        assert_eq!(e.version(), 0x1);
    }

    /// rskj `hashIncludesLogsBloom` -- the hash commits to the bloom even
    /// though the encoding the header carries does not contain it.
    #[test]
    fn v1_hash_changes_with_the_bloom() {
        let a = v1(bloom(&[1, 2, 3, 4]), Some(EDGES.to_vec()));
        let b = v1(bloom(&[1, 2, 3, 5]), Some(EDGES.to_vec()));
        assert_ne!(a.hash(), b.hash());
    }

    /// rskj `hashIncludesEdges`
    #[test]
    fn v1_hash_changes_with_the_edges() {
        let b = bloom(&[1, 2, 3, 4]);
        let a = v1(b.clone(), Some(EDGES.to_vec()));
        let c = v1(b, Some(vec![5, 6, 7, 8]));
        assert_ne!(a.hash(), c.hash());
    }

    /// rskj `encodeDecode`
    #[test]
    fn v1_round_trips() {
        let original = v1(bloom(&[1, 2, 3, 4]), Some(EDGES.to_vec()));
        let decoded = BlockHeaderExtension::from_encoded(&original.to_encoded()).unwrap();
        assert_eq!(decoded, original);
    }

    /// The bloom is hashed before being committed to, so the two encodings are
    /// different lengths -- the point of RSKIP-351.
    #[test]
    fn the_committed_form_is_smaller_than_the_body() {
        let e = v1(bloom(&[1, 2, 3, 4]), Some(EDGES.to_vec()));
        assert!(
            e.create_extension_data().len() < e.encoded().len(),
            "the header's commitment must be smaller than the body it commits to"
        );
    }

    // ---- V2 ---------------------------------------------------------------

    /// rskj `constructorAndGetters`
    #[test]
    fn v2_keeps_all_three_fields() {
        let e = v2(bloom(&[9]), Some(vec![1, 2]), Some(vec![0xAB; 10]));
        assert_eq!(e.version(), 0x2);
        assert_eq!(e.edges(), Some(&[1i16, 2][..]));
        assert_eq!(e.base_event(), Some(&[0xABu8; 10][..]));
    }

    /// rskj `decodeOnlyLogsBloom`: a null baseEvent still encodes as an empty
    /// element, so it decodes back as empty rather than absent.
    #[test]
    fn v2_a_null_base_event_decodes_as_empty_not_absent() {
        let e = v2(vec![0u8; 256], None, None);
        let decoded = BlockHeaderExtension::from_encoded(&e.to_encoded()).unwrap();
        assert_eq!(decoded.base_event(), Some(&[][..]), "must not be lost as absent");
        assert_eq!(decoded.edges(), None);
    }

    /// rskj `decodeLogsBloomAndEdges`
    #[test]
    fn v2_round_trips_with_edges_and_no_base_event() {
        let e = v2(vec![0u8; 256], Some(vec![1, 2]), None);
        let decoded = BlockHeaderExtension::from_encoded(&e.to_encoded()).unwrap();
        assert_eq!(decoded.edges(), Some(&[1i16, 2][..]));
        assert_eq!(decoded.base_event(), Some(&[][..]));
    }

    /// rskj `decodeWithEmptyEdges`
    #[test]
    fn v2_round_trips_with_a_base_event_and_no_edges() {
        let be = vec![0u8; 128];
        let e = v2(vec![0u8; 256], None, Some(be.clone()));
        let decoded = BlockHeaderExtension::from_encoded(&e.to_encoded()).unwrap();
        assert_eq!(decoded.base_event(), Some(&be[..]));
        assert_eq!(decoded.edges(), None, "absent edges must stay absent");
    }

    /// rskj `decodeWithEmptyBaseEvent`
    #[test]
    fn v2_round_trips_with_an_empty_base_event() {
        let e = v2(vec![0u8; 256], Some(vec![1]), Some(vec![]));
        let decoded = BlockHeaderExtension::from_encoded(&e.to_encoded()).unwrap();
        assert_eq!(decoded.edges(), Some(&[1i16][..]));
        assert_eq!(decoded.base_event(), Some(&[][..]));
    }

    /// rskj `testSetBaseEventWithMaxSize` / `testSetBaseEventExceedingMaxSizeThrowsException`
    #[test]
    fn v2_refuses_a_base_event_over_the_limit_and_accepts_one_at_it() {
        let mut e = v2(vec![0u8; 256], None, None);
        assert!(e.set_base_event(Some(vec![0xFF; BASE_EVENT_MAX_SIZE])).is_ok());
        assert_eq!(
            e.set_base_event(Some(vec![0xFF; BASE_EVENT_MAX_SIZE + 1])),
            Err(ExtensionError::BaseEventTooLong(BASE_EVENT_MAX_SIZE + 1)),
            "over-long values are refused, not truncated"
        );
    }

    /// rskj `testBaseEventPersistenceAfterEncoding`
    #[test]
    fn v2_base_event_survives_a_round_trip_unchanged() {
        let be: Vec<u8> = (0..=255u8).take(128).collect();
        let e = v2(bloom(&[7]), Some(vec![3]), Some(be.clone()));
        let decoded = BlockHeaderExtension::from_encoded(&e.to_encoded()).unwrap();
        assert_eq!(decoded.base_event(), Some(&be[..]));
        assert_eq!(decoded.hash(), e.hash(), "the commitment must survive too");
    }

    /// The baseEvent is committed to: two blocks differing only there must not
    /// share a header commitment, or the field would be unauthenticated.
    #[test]
    fn v2_hash_changes_with_the_base_event() {
        let a = v2(vec![0u8; 256], None, Some(vec![1]));
        let b = v2(vec![0u8; 256], None, Some(vec![2]));
        assert_ne!(a.hash(), b.hash());
    }

    /// V1 and V2 over the same bloom and edges are different commitments --
    /// the version is not cosmetic.
    #[test]
    fn v1_and_v2_do_not_collide() {
        let b = bloom(&[4, 5, 6]);
        let one = v1(b.clone(), Some(vec![2]));
        let two = v2(b, Some(vec![2]), None);
        assert_ne!(one.hash(), two.hash());
        assert_ne!(one.create_extension_data(), two.create_extension_data());
    }

    // ---- the self-describing wrapper --------------------------------------

    /// rskj `BlockHeaderExtensionTest.decodeV1`
    #[test]
    fn to_encoded_names_its_version() {
        for e in [v1(bloom(&[1]), None), v2(bloom(&[1]), None, None)] {
            let decoded = BlockHeaderExtension::from_encoded(&e.to_encoded()).unwrap();
            assert_eq!(decoded.version(), e.version());
        }
    }

    /// rskj `BlockHeaderExtensionTest.invalidDecode` -- an unknown version is
    /// refused rather than guessed at.
    #[test]
    fn an_unknown_version_is_refused() {
        let body = encode_element(&v1(vec![0u8; 256], None).encoded());
        let bad = encode_list(&[encode_byte(0x3), body]);
        assert_eq!(
            BlockHeaderExtension::from_encoded(&bad),
            Err(ExtensionError::UnknownVersion(0x3))
        );
    }

    #[test]
    fn a_non_pair_is_refused() {
        let only_one = encode_list(&[encode_byte(0x1)]);
        assert_eq!(BlockHeaderExtension::from_encoded(&only_one), Err(ExtensionError::NotAPair));
    }

    /// rskj `decodeMalformedRLPThrows`
    #[test]
    fn malformed_bytes_are_refused_rather_than_panicking() {
        for junk in [&b""[..], &b"\xff\xff\xff"[..], &b"not rlp at all"[..]] {
            assert!(BlockHeaderExtension::from_encoded(junk).is_err(), "junk: {junk:?}");
        }
    }

    // ---- the encoding detail most likely to be got wrong -------------------

    /// rskj packs edges as **little-endian** shorts before RLP-encoding them.
    /// Everything else in RLP is big-endian, so a reader following the usual
    /// convention computes a different commitment and therefore a different
    /// block hash. Pinned explicitly.
    #[test]
    fn edges_are_packed_little_endian() {
        let packed = shorts_to_rlp(&[0x0102, 0x0304]);
        // RLP element header for 4 bytes, then the shorts, low byte first.
        assert_eq!(packed, vec![0x84, 0x02, 0x01, 0x04, 0x03]);
        assert_eq!(rlp_to_shorts(&packed[1..]), vec![0x0102, 0x0304]);
    }

    /// An empty edge list is not the same as no edge list: rskj omits the
    /// element for `null` and emits an empty one for `short[0]`, which are
    /// different bytes and so different commitments.
    #[test]
    fn empty_edges_and_absent_edges_are_different_commitments() {
        let absent = v1(vec![0u8; 256], None);
        let empty = v1(vec![0u8; 256], Some(vec![]));
        assert_ne!(absent.encoded(), empty.encoded());
        assert_ne!(absent.hash(), empty.hash());
    }
}
