use alloy_rlp::{Decodable, Encodable, RlpDecodable, RlpEncodable, Header as RlpHeader};
use alloy_primitives::{B256, Bytes, U256};
use rustock_core::{Header, Transaction};
use rustock_core::rlp_compat::{decode_u8_lenient, decode_u64_lenient, decode_u256_lenient, decode_u32_lenient};
use super::snap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RskStatus {
    pub best_block_number: u64,
    pub best_block_hash: B256,
    pub best_block_parent_hash: Option<B256>,
    pub total_difficulty: Option<U256>,
    /// Lowest block this node can serve, as a fifth trailing element.
    ///
    /// Appended rather than replacing anything: rskj's decoder reads indices
    /// 0..3 and never consults the list length again, so a fifth element is
    /// accepted and ignored. Verified against rskj 9.1.0 by decoding 2-, 4-,
    /// 5- and 6-element messages through its own `MessageType.createMessage`;
    /// all four were accepted with the known fields intact.
    ///
    /// Total difficulty stays where it is. `eth/69` dropped it because it is
    /// meaningless after the merge, which is not true here.
    ///
    /// `None` means "did not say", and a reader must treat that as "serves
    /// everything" rather than "serves nothing" -- that is what every rskj
    /// peer will look like.
    pub earliest_block: Option<u64>,
}

impl Encodable for RskStatus {
    fn encode(&self, out: &mut dyn alloy_rlp::BufMut) {
        let mut list = Vec::new();
        self.best_block_number.encode(&mut list);
        self.best_block_hash.encode(&mut list);
        
        if let (Some(parent), Some(td)) = (self.best_block_parent_hash, self.total_difficulty) {
            parent.encode(&mut list);
            td.encode(&mut list);
            // Only after the four rskj knows about, and only when there is
            // something to say. A node serving the whole chain adds nothing.
            if let Some(earliest) = self.earliest_block {
                earliest.encode(&mut list);
            }
        }
        
        RlpHeader { list: true, payload_length: list.len() }.encode(out);
        out.put_slice(&list);
    }

    fn length(&self) -> usize {
        let mut len = self.best_block_number.length() + self.best_block_hash.length();
        if let (Some(parent), Some(td)) = (self.best_block_parent_hash, self.total_difficulty) {
            len += parent.length() + td.length();
        }
        RlpHeader { list: true, payload_length: len }.length() + len
    }
}

impl Decodable for RskStatus {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let header = RlpHeader::decode(buf)?;
        let mut body = &buf[..header.payload_length];
        *buf = &buf[header.payload_length..];

        let best_block_number = decode_u64_lenient(&mut body)?;
        let best_block_hash = B256::decode(&mut body)?;

        let mut status = Self {
            best_block_number,
            best_block_hash,
            best_block_parent_hash: None,
            total_difficulty: None,
            earliest_block: None,
        };

        if !body.is_empty() {
            status.best_block_parent_hash = Some(B256::decode(&mut body)?);
            status.total_difficulty = Some(decode_u256_lenient(&mut body)?);
        }

        // A fifth element, if the peer speaks the extension. Anything beyond
        // it is left alone on purpose: this is the tolerance a later field
        // will rely on, and it is the same courtesy rskj extends to us.
        if !body.is_empty() {
            status.earliest_block = Some(decode_u64_lenient(&mut body)?);
        }

        Ok(status)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, RlpEncodable, RlpDecodable)]
pub struct BlockHeadersRequest {
    pub id: u64,
    pub query: BlockHeadersQuery,
}

#[derive(Debug, Clone, PartialEq, Eq, RlpEncodable, RlpDecodable)]
pub struct BlockHeadersQuery {
    pub hash: B256,
    pub count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, RlpEncodable, RlpDecodable)]
pub struct BlockHeadersResponse {
    pub id: u64,
    pub headers: Vec<Header>,
}

/// A block identifier used in skeleton responses (hash + number).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockIdentifier {
    pub hash: B256,
    pub number: u64,
}

/// Request a block body by hash (type 14).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BodyRequest {
    pub id: u64,
    pub hash: B256,
}

/// Response containing a block body (type 15).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BodyResponse {
    pub id: u64,
    pub transactions: Vec<Transaction>,
    pub uncles: Vec<Header>,
}

/// Request the hash of the block at a given height (type 8).
/// Used during connection-point binary search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockHashRequest {
    pub id: u64,
    pub height: u64,
}

/// Response with the block hash at the requested height (type 18).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockHashResponse {
    pub id: u64,
    pub hash: B256,
}

/// Request the skeleton (evenly-spaced block identifiers) from a starting height (type 16).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkeletonRequest {
    pub id: u64,
    pub start_number: u64,
}

/// Response with a list of block identifiers forming the skeleton (type 13).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkeletonResponse {
    pub id: u64,
    pub block_identifiers: Vec<BlockIdentifier>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum RskMessageType {
    Status = 1,
    NewBlockHashes = 6,
    Transactions = 7,
    BlockHashRequest = 8,
    BlockHeadersRequest = 9,
    BlockHeadersResponse = 10,
    SkeletonResponse = 13,
    BodyRequest = 14,
    BodyResponse = 15,
    SkeletonRequest = 16,
    BlockHashResponse = 18,
    SnapStateChunkRequest = 20,
    SnapStateChunkResponse = 21,
    SnapStatusRequest = 22,
    SnapStatusResponse = 23,
    SnapBlocksRequest = 24,
    SnapBlocksResponse = 25,
    /// Announces a change to the range of blocks this node serves.
    ///
    /// 26 is the first type rskj does not define; its highest is 25. An rskj
    /// node that received this would throw `IllegalArgumentException` out of
    /// `MessageType.valueOfType` and close the connection, so it is sent only
    /// to peers that negotiated `rsk/63`.
    BlockRangeUpdate = 26,
}

/// The `rsk` subprotocol version that carries the served block range.
///
/// `62` is what rskj speaks today. A node negotiating `63` has agreed to read
/// a fifth element in the status message and to accept
/// [`RskSubMessage::BlockRangeUpdate`]; one at `62` has agreed to neither, and
/// sending it the update would close the connection -- rskj throws out of
/// `MessageType.valueOfType` for any type it does not know, and its netty
/// handler answers that with `ctx.close()`.
pub const RSK_RANGE_VERSION: u64 = 63;

/// The range of blocks a node can serve.
///
/// Shaped after `eth/69`'s `BlockRangeUpdatePacket`, so that an RSKIP
/// proposing it to rskj maps onto something already argued through upstream.
///
/// `earliest` is exact -- a node never holds blocks below it. `latest` trails
/// the real head, because an announcement is a snapshot taken at a moment and
/// the head moves on; a reader should treat the upper bound as loose and the
/// lower bound as firm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockRange {
    /// Lowest block this node can serve. Zero on a node holding the whole
    /// chain, and the prune floor on one that does not.
    pub earliest_block: u64,
    pub latest_block: u64,
    pub latest_block_hash: B256,
}

impl Encodable for BlockRange {
    fn encode(&self, out: &mut dyn alloy_rlp::BufMut) {
        let mut list = Vec::new();
        self.earliest_block.encode(&mut list);
        self.latest_block.encode(&mut list);
        self.latest_block_hash.encode(&mut list);
        RlpHeader { list: true, payload_length: list.len() }.encode(out);
        out.put_slice(&list);
    }
}

impl Decodable for BlockRange {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let header = RlpHeader::decode(buf)?;
        let mut body = &buf[..header.payload_length];
        *buf = &buf[header.payload_length..];
        Ok(Self {
            earliest_block: decode_u64_lenient(&mut body)?,
            latest_block: decode_u64_lenient(&mut body)?,
            latest_block_hash: B256::decode(&mut body)?,
        })
    }
}

#[derive(Debug, Clone)]
pub enum RskSubMessage {
    Status(RskStatus),
    BlockHashRequest(BlockHashRequest),
    BlockHeadersRequest(BlockHeadersRequest),
    BlockHeadersResponse(BlockHeadersResponse),
    SkeletonRequest(SkeletonRequest),
    SkeletonResponse(SkeletonResponse),
    BlockHashResponse(BlockHashResponse),
    BodyRequest(BodyRequest),
    BodyResponse(BodyResponse),
    NewBlockHashes(Vec<BlockIdentifier>),
    Transactions(Vec<Bytes>),
    SnapStatusRequest(snap::SnapStatusRequest),
    SnapStatusResponse(Box<snap::SnapStatusResponse>),
    SnapChunkRequest(snap::SnapChunkRequest),
    SnapChunkResponse(Box<snap::SnapChunkResponse>),
    SnapBlocksRequest(snap::SnapBlocksRequest),
    SnapBlocksResponse(Box<snap::SnapBlocksResponse>),
    BlockRangeUpdate(BlockRange),
    Unknown(u8),
}

impl RskSubMessage {
    pub fn message_type(&self) -> RskMessageType {
        match self {
            RskSubMessage::Status(_) => RskMessageType::Status,
            RskSubMessage::BlockHashRequest(_) => RskMessageType::BlockHashRequest,
            RskSubMessage::BlockHeadersRequest(_) => RskMessageType::BlockHeadersRequest,
            RskSubMessage::BlockHeadersResponse(_) => RskMessageType::BlockHeadersResponse,
            RskSubMessage::SkeletonRequest(_) => RskMessageType::SkeletonRequest,
            RskSubMessage::SkeletonResponse(_) => RskMessageType::SkeletonResponse,
            RskSubMessage::BlockHashResponse(_) => RskMessageType::BlockHashResponse,
            RskSubMessage::BodyRequest(_) => RskMessageType::BodyRequest,
            RskSubMessage::BodyResponse(_) => RskMessageType::BodyResponse,
            RskSubMessage::NewBlockHashes(_) => RskMessageType::NewBlockHashes,
            RskSubMessage::Transactions(_) => RskMessageType::Transactions,
            RskSubMessage::SnapStatusRequest(_) => RskMessageType::SnapStatusRequest,
            RskSubMessage::SnapStatusResponse(_) => RskMessageType::SnapStatusResponse,
            RskSubMessage::SnapChunkRequest(_) => RskMessageType::SnapStateChunkRequest,
            RskSubMessage::SnapChunkResponse(_) => RskMessageType::SnapStateChunkResponse,
            RskSubMessage::SnapBlocksRequest(_) => RskMessageType::SnapBlocksRequest,
            RskSubMessage::SnapBlocksResponse(_) => RskMessageType::SnapBlocksResponse,
            RskSubMessage::BlockRangeUpdate(_) => RskMessageType::BlockRangeUpdate,
            RskSubMessage::Unknown(_) => RskMessageType::Status, // Not used for encoding
        }
    }

    /// Encodes parameters as a List. 
    /// Corresponds to Java's getEncodedMessage() [id + params] or [params for status]
    fn encode_params(&self, out: &mut Vec<u8>) {
        match self {
            RskSubMessage::Status(s) => {
                let mut list = Vec::new();
                s.best_block_number.encode(&mut list);
                s.best_block_hash.encode(&mut list);
                if let (Some(parent), Some(td)) = (s.best_block_parent_hash, s.total_difficulty) {
                    parent.encode(&mut list);
                    td.encode(&mut list);
                    if let Some(earliest) = s.earliest_block {
                        earliest.encode(&mut list);
                    }
                }
                RlpHeader { list: true, payload_length: list.len() }.encode(out);
                out.extend_from_slice(&list);
            }
            RskSubMessage::BlockRangeUpdate(range) => {
                range.encode(out);
            }
            RskSubMessage::BlockHeadersRequest(r) => {
                // RLP([id, RLP([hash, count])])
                let mut query_params = Vec::new();
                r.query.hash.encode(&mut query_params);
                r.query.count.encode(&mut query_params);
                
                let mut inner_list = Vec::new();
                RlpHeader { list: true, payload_length: query_params.len() }.encode(&mut inner_list);
                inner_list.extend_from_slice(&query_params);

                let mut params = Vec::new();
                r.id.encode(&mut params);
                // The inner list is pre-encoded RLP, so it's just appended to the outer list
                params.extend_from_slice(&inner_list);

                RlpHeader { list: true, payload_length: params.len() }.encode(out);
                out.extend_from_slice(&params);
            }
            RskSubMessage::BlockHeadersResponse(r) => {
                // RLP([id, RLP([RLP([headers])])])
                let mut headers_payload = Vec::new();
                for h in &r.headers {
                    h.encode(&mut headers_payload);
                }
                
                let mut headers_list = Vec::new();
                RlpHeader { list: true, payload_length: headers_payload.len() }.encode(&mut headers_list);
                headers_list.extend_from_slice(&headers_payload);

                let mut wrapped_headers = Vec::new();
                RlpHeader { list: true, payload_length: headers_list.len() }.encode(&mut wrapped_headers);
                wrapped_headers.extend_from_slice(&headers_list);

                let mut params = Vec::new();
                r.id.encode(&mut params);
                params.extend_from_slice(&wrapped_headers);

                RlpHeader { list: true, payload_length: params.len() }.encode(out);
                out.extend_from_slice(&params);
            }
            RskSubMessage::BlockHashRequest(r) => {
                // RLP([id, RLP([height])])
                let mut inner = Vec::new();
                r.height.encode(&mut inner);

                let mut inner_list = Vec::new();
                RlpHeader { list: true, payload_length: inner.len() }.encode(&mut inner_list);
                inner_list.extend_from_slice(&inner);

                let mut params = Vec::new();
                r.id.encode(&mut params);
                params.extend_from_slice(&inner_list);

                RlpHeader { list: true, payload_length: params.len() }.encode(out);
                out.extend_from_slice(&params);
            }
            RskSubMessage::BlockHashResponse(r) => {
                // RLP([id, RLP([hash])])
                let mut inner = Vec::new();
                r.hash.encode(&mut inner);

                let mut inner_list = Vec::new();
                RlpHeader { list: true, payload_length: inner.len() }.encode(&mut inner_list);
                inner_list.extend_from_slice(&inner);

                let mut params = Vec::new();
                r.id.encode(&mut params);
                params.extend_from_slice(&inner_list);

                RlpHeader { list: true, payload_length: params.len() }.encode(out);
                out.extend_from_slice(&params);
            }
            RskSubMessage::SkeletonRequest(r) => {
                // RLP([id, RLP([startNumber])])
                let mut inner = Vec::new();
                r.start_number.encode(&mut inner);

                let mut inner_list = Vec::new();
                RlpHeader { list: true, payload_length: inner.len() }.encode(&mut inner_list);
                inner_list.extend_from_slice(&inner);

                let mut params = Vec::new();
                r.id.encode(&mut params);
                params.extend_from_slice(&inner_list);

                RlpHeader { list: true, payload_length: params.len() }.encode(out);
                out.extend_from_slice(&params);
            }
            RskSubMessage::SkeletonResponse(r) => {
                // RLP([id, RLP([RLP([bid_0, bid_1, ...])])])
                // Each bid = RLP([hash, number])
                let mut bids_payload = Vec::new();
                for bid in &r.block_identifiers {
                    let mut bid_elems = Vec::new();
                    bid.hash.encode(&mut bid_elems);
                    bid.number.encode(&mut bid_elems);
                    RlpHeader { list: true, payload_length: bid_elems.len() }.encode(&mut bids_payload);
                    bids_payload.extend_from_slice(&bid_elems);
                }

                let mut inner_list = Vec::new();
                RlpHeader { list: true, payload_length: bids_payload.len() }.encode(&mut inner_list);
                inner_list.extend_from_slice(&bids_payload);

                let mut outer = Vec::new();
                RlpHeader { list: true, payload_length: inner_list.len() }.encode(&mut outer);
                outer.extend_from_slice(&inner_list);

                let mut params = Vec::new();
                r.id.encode(&mut params);
                params.extend_from_slice(&outer);

                RlpHeader { list: true, payload_length: params.len() }.encode(out);
                out.extend_from_slice(&params);
            }
            RskSubMessage::BodyRequest(r) => {
                // RLP([id, RLP([hash])])
                let mut inner = Vec::new();
                r.hash.encode(&mut inner);

                let mut inner_list = Vec::new();
                RlpHeader { list: true, payload_length: inner.len() }.encode(&mut inner_list);
                inner_list.extend_from_slice(&inner);

                let mut params = Vec::new();
                r.id.encode(&mut params);
                params.extend_from_slice(&inner_list);

                RlpHeader { list: true, payload_length: params.len() }.encode(out);
                out.extend_from_slice(&params);
            }
            RskSubMessage::BodyResponse(r) => {
                // RLP([id, RLP([txs_list, uncles_list])])
                let mut txs_payload = Vec::new();
                for tx in &r.transactions {
                    // As in `encode_block`: serve the bytes the header commits
                    // to, not a canonical re-encoding of the parsed fields.
                    txs_payload.extend_from_slice(&tx.rlp_for_trie());
                }
                let mut txs_list = Vec::new();
                RlpHeader { list: true, payload_length: txs_payload.len() }.encode(&mut txs_list);
                txs_list.extend_from_slice(&txs_payload);

                let mut uncles_payload = Vec::new();
                for uncle in &r.uncles {
                    uncle.encode(&mut uncles_payload);
                }
                let mut uncles_list = Vec::new();
                RlpHeader { list: true, payload_length: uncles_payload.len() }.encode(&mut uncles_list);
                uncles_list.extend_from_slice(&uncles_payload);

                let body_len = txs_list.len() + uncles_list.len();
                let mut body = Vec::new();
                RlpHeader { list: true, payload_length: body_len }.encode(&mut body);
                body.extend_from_slice(&txs_list);
                body.extend_from_slice(&uncles_list);

                let mut params = Vec::new();
                r.id.encode(&mut params);
                params.extend_from_slice(&body);

                RlpHeader { list: true, payload_length: params.len() }.encode(out);
                out.extend_from_slice(&params);
            }
            RskSubMessage::Transactions(txs) => {
                let mut txs_payload = Vec::new();
                for tx in txs {
                    // A transaction is already a complete RLP list, so it goes
                    // in verbatim. Wrapping it in a string header made every
                    // transaction this node relayed undecodable to its peers,
                    // the mirror image of the decode bug above.
                    txs_payload.extend_from_slice(tx);
                }
                RlpHeader { list: true, payload_length: txs_payload.len() }.encode(out);
                out.extend_from_slice(&txs_payload);
            }
            // Snap bodies are built in `snap`, which owns their shape; here
            // they are already-encoded parameter lists.
            RskSubMessage::SnapStatusRequest(m) => out.extend_from_slice(&m.encode_body()),
            RskSubMessage::SnapStatusResponse(m) => out.extend_from_slice(&m.encode_body()),
            RskSubMessage::SnapChunkRequest(m) => out.extend_from_slice(&m.encode_body()),
            RskSubMessage::SnapChunkResponse(m) => out.extend_from_slice(&m.encode_body()),
            RskSubMessage::SnapBlocksRequest(m) => out.extend_from_slice(&m.encode_body()),
            RskSubMessage::SnapBlocksResponse(m) => out.extend_from_slice(&m.encode_body()),
            RskSubMessage::NewBlockHashes(_) | RskSubMessage::Unknown(_) => {
                // Receive-only messages are not encoded/sent
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct RskMessage {
    pub sub_message: RskSubMessage,
}

impl RskMessage {
    pub const MESSAGE_ID: u8 = 0x08;

    pub fn new(sub_message: RskSubMessage) -> Self {
        Self { sub_message }
    }
}

impl Encodable for RskMessage {
    fn encode(&self, out: &mut dyn alloy_rlp::BufMut) {
        let mut params_rlp = Vec::new();
        self.sub_message.encode_params(&mut params_rlp);
        
        // Java Message.getEncoded(): RLP([type, RLP_String(params_rlp)])
        let mut msg_rlp = Vec::new();
        (self.sub_message.message_type() as u8).encode(&mut msg_rlp);
        // encodeElement wraps in Rlp String (Blob)
        RlpHeader { list: false, payload_length: params_rlp.len() }.encode(&mut msg_rlp);
        msg_rlp.extend_from_slice(&params_rlp);
        
        let mut wrapped_msg = Vec::new();
        RlpHeader { list: true, payload_length: msg_rlp.len() }.encode(&mut wrapped_msg);
        wrapped_msg.extend_from_slice(&msg_rlp);

        // Java RskMessage.encode(): RLP([wrapped_msg])
        let mut final_rlp = Vec::new();
        RlpHeader { list: true, payload_length: wrapped_msg.len() }.encode(&mut final_rlp);
        final_rlp.extend_from_slice(&wrapped_msg);

        out.put_slice(&final_rlp);
    }

    fn length(&self) -> usize {
        let mut buf = Vec::new();
        self.encode(&mut buf);
        buf.len()
    }
}

impl Decodable for RskMessage {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let h1 = RlpHeader::decode(buf)?;
        let mut b1 = &buf[..h1.payload_length];
        *buf = &buf[h1.payload_length..];

        let h2 = RlpHeader::decode(&mut b1)?;
        let mut b2 = &b1[..h2.payload_length];

        let type_byte = decode_u8_lenient(&mut b2)?;
        
        // Next is the body_blob (RLP String)
        let body_h = RlpHeader::decode(&mut b2)?;
        if body_h.list {
            return Err(alloy_rlp::Error::Custom("Expected RLP string for body blob"));
        }
        let mut body_params = &b2[..body_h.payload_length];

        let sub_message = match type_byte {
            1 => {
                let list_h = RlpHeader::decode(&mut body_params)?;
                let mut list_body = &body_params[..list_h.payload_length];
                let best_block_number = decode_u64_lenient(&mut list_body)?;
                let best_block_hash = B256::decode(&mut list_body)?;
                let mut status = RskStatus {
                    best_block_number,
                    best_block_hash,
                    best_block_parent_hash: None,
                    total_difficulty: None,
                    earliest_block: None,
                };
                if !list_body.is_empty() {
                    status.best_block_parent_hash = Some(B256::decode(&mut list_body)?);
                    status.total_difficulty = Some(decode_u256_lenient(&mut list_body)?);
                }
                if !list_body.is_empty() {
                    status.earliest_block = Some(decode_u64_lenient(&mut list_body)?);
                }
                RskSubMessage::Status(status)
            }
            9 => {
                let list_h = RlpHeader::decode(&mut body_params)?;
                let mut list_body = &body_params[..list_h.payload_length];
                let id = decode_u64_lenient(&mut list_body)?;
                
                let query_h = RlpHeader::decode(&mut list_body)?;
                let mut query_body = &list_body[..query_h.payload_length];
                let hash = B256::decode(&mut query_body)?;
                let count = decode_u32_lenient(&mut query_body)?;

                RskSubMessage::BlockHeadersRequest(BlockHeadersRequest {
                    id,
                    query: BlockHeadersQuery { hash, count },
                })
            }
            10 => {
                let list_h = RlpHeader::decode(&mut body_params)?;
                let mut list_body = &body_params[..list_h.payload_length];
                let id = decode_u64_lenient(&mut list_body)?;

                let outer_h = RlpHeader::decode(&mut list_body)?;
                let mut outer_body = &list_body[..outer_h.payload_length];
                
                let inner_h = RlpHeader::decode(&mut outer_body)?;
                let mut inner_body = &outer_body[..inner_h.payload_length];

                let mut headers = Vec::new();
                while !inner_body.is_empty() {
                    headers.push(Header::decode_with_hash(&mut inner_body)?);
                }
                RskSubMessage::BlockHeadersResponse(BlockHeadersResponse { id, headers })
            }
            8 => {
                // BlockHashRequest: RLP([id, RLP([height])])
                let list_h = RlpHeader::decode(&mut body_params)?;
                let mut list_body = &body_params[..list_h.payload_length];
                let id = decode_u64_lenient(&mut list_body)?;

                let inner_h = RlpHeader::decode(&mut list_body)?;
                let mut inner_body = &list_body[..inner_h.payload_length];
                let height = decode_u64_lenient(&mut inner_body)?;

                RskSubMessage::BlockHashRequest(BlockHashRequest { id, height })
            }
            13 => {
                // SkeletonResponse: RLP([id, RLP([RLP([bid_0, bid_1, ...])])])
                let list_h = RlpHeader::decode(&mut body_params)?;
                let mut list_body = &body_params[..list_h.payload_length];
                let id = decode_u64_lenient(&mut list_body)?;

                let outer_h = RlpHeader::decode(&mut list_body)?;
                let mut outer_body = &list_body[..outer_h.payload_length];

                let inner_h = RlpHeader::decode(&mut outer_body)?;
                let mut inner_body = &outer_body[..inner_h.payload_length];

                let mut block_identifiers = Vec::new();
                while !inner_body.is_empty() {
                    let bid_h = RlpHeader::decode(&mut inner_body)?;
                    let mut bid_body = &inner_body[..bid_h.payload_length];
                    inner_body = &inner_body[bid_h.payload_length..];
                    let hash = B256::decode(&mut bid_body)?;
                    let number = decode_u64_lenient(&mut bid_body)?;
                    block_identifiers.push(BlockIdentifier { hash, number });
                }

                RskSubMessage::SkeletonResponse(SkeletonResponse { id, block_identifiers })
            }
            16 => {
                // SkeletonRequest: RLP([id, RLP([startNumber])])
                let list_h = RlpHeader::decode(&mut body_params)?;
                let mut list_body = &body_params[..list_h.payload_length];
                let id = decode_u64_lenient(&mut list_body)?;

                let inner_h = RlpHeader::decode(&mut list_body)?;
                let mut inner_body = &list_body[..inner_h.payload_length];
                let start_number = decode_u64_lenient(&mut inner_body)?;

                RskSubMessage::SkeletonRequest(SkeletonRequest { id, start_number })
            }
            18 => {
                // BlockHashResponse: RLP([id, RLP([hash])])
                let list_h = RlpHeader::decode(&mut body_params)?;
                let mut list_body = &body_params[..list_h.payload_length];
                let id = decode_u64_lenient(&mut list_body)?;

                let inner_h = RlpHeader::decode(&mut list_body)?;
                let mut inner_body = &list_body[..inner_h.payload_length];
                let hash = B256::decode(&mut inner_body)?;

                RskSubMessage::BlockHashResponse(BlockHashResponse { id, hash })
            }
            14 => {
                // BodyRequest: RLP([id, RLP([hash])])
                let list_h = RlpHeader::decode(&mut body_params)?;
                let mut list_body = &body_params[..list_h.payload_length];
                let id = decode_u64_lenient(&mut list_body)?;

                let inner_h = RlpHeader::decode(&mut list_body)?;
                let mut inner_body = &list_body[..inner_h.payload_length];
                let hash = B256::decode(&mut inner_body)?;

                RskSubMessage::BodyRequest(BodyRequest { id, hash })
            }
            15 => {
                // BodyResponse: RLP([id, RLP([txs_list, uncles_list, extension?])])
                let list_h = RlpHeader::decode(&mut body_params)?;
                let mut list_body = &body_params[..list_h.payload_length];
                let id = decode_u64_lenient(&mut list_body)?;

                let body_h = RlpHeader::decode(&mut list_body)?;
                let mut body_data = &list_body[..body_h.payload_length];

                // Decode transactions list
                let txs_h = RlpHeader::decode(&mut body_data)?;
                let mut txs_body = &body_data[..txs_h.payload_length];
                body_data = &body_data[txs_h.payload_length..];
                let mut transactions = Vec::new();
                while !txs_body.is_empty() {
                    transactions.push(Transaction::decode(&mut txs_body)?);
                }

                // Decode uncles list
                let uncles_h = RlpHeader::decode(&mut body_data)?;
                let mut uncles_body = &body_data[..uncles_h.payload_length];
                let mut uncles = Vec::new();
                while !uncles_body.is_empty() {
                    uncles.push(Header::decode_with_hash(&mut uncles_body)?);
                }

                // Skip optional BlockHeaderExtension (we don't need it for body storage)

                RskSubMessage::BodyResponse(BodyResponse { id, transactions, uncles })
            }
            6 => {
                // NewBlockHashes: RLP list of [hash, number] pairs
                let list_h = RlpHeader::decode(&mut body_params)?;
                let mut list_body = &body_params[..list_h.payload_length];
                let mut identifiers = Vec::new();
                while !list_body.is_empty() {
                    let item_h = RlpHeader::decode(&mut list_body)?;
                    let mut item_body = &list_body[..item_h.payload_length];
                    list_body = &list_body[item_h.payload_length..];
                    let hash = B256::decode(&mut item_body)?;
                    let number = decode_u64_lenient(&mut item_body)?;
                    identifiers.push(BlockIdentifier { hash, number });
                }
                RskSubMessage::NewBlockHashes(identifiers)
            }
            7 => {
                let list_h = RlpHeader::decode(&mut body_params)?;
                let mut list_body = &body_params[..list_h.payload_length];
                let mut txs = Vec::new();
                while !list_body.is_empty() {
                    // Each item is a COMPLETE transaction -- itself an RLP list
                    // -- and must be handed on whole. Decoding the header to
                    // find the item's length and then keeping only the payload
                    // strips the list header, and the pool cannot decode what
                    // is left: `Transaction::decode` needs a complete RLP list.
                    //
                    // Mainnet symptom: 19 transactions offered in five minutes,
                    // all 19 rejected `rlp-decode`.
                    let before = list_body;
                    let tx_h = RlpHeader::decode(&mut list_body)?;
                    let header_len = before.len() - list_body.len();
                    let total = header_len + tx_h.payload_length;
                    txs.push(Bytes::copy_from_slice(&before[..total]));
                    list_body = &before[total..];
                }
                RskSubMessage::Transactions(txs)
            }
            snap::message_type::STATUS_REQUEST => RskSubMessage::SnapStatusRequest(
                snap::SnapStatusRequest::decode_body(&mut body_params)?,
            ),
            snap::message_type::STATUS_RESPONSE => RskSubMessage::SnapStatusResponse(
                Box::new(snap::SnapStatusResponse::decode_body(&mut body_params)?),
            ),
            snap::message_type::STATE_CHUNK_REQUEST => RskSubMessage::SnapChunkRequest(
                snap::SnapChunkRequest::decode_body(&mut body_params)?,
            ),
            snap::message_type::STATE_CHUNK_RESPONSE => RskSubMessage::SnapChunkResponse(
                Box::new(snap::SnapChunkResponse::decode_body(&mut body_params)?),
            ),
            snap::message_type::BLOCKS_REQUEST => RskSubMessage::SnapBlocksRequest(
                snap::SnapBlocksRequest::decode_body(&mut body_params)?,
            ),
            snap::message_type::BLOCKS_RESPONSE => RskSubMessage::SnapBlocksResponse(
                Box::new(snap::SnapBlocksResponse::decode_body(&mut body_params)?),
            ),
            26 => RskSubMessage::BlockRangeUpdate(BlockRange::decode(&mut body_params)?),
            other => {
                RskSubMessage::Unknown(other)
            }
        };

        Ok(RskMessage { sub_message })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_rlp::{Encodable, Decodable};


    /// A REMASC transaction's zero fields are written as literal 0x00 bytes,
    /// not RLP's empty string. Serving a canonical re-encoding changes the
    /// transaction root and the receiving peer rejects the body.
    #[test]
    fn a_remasc_body_keeps_the_bytes_its_header_commits_to() {
        use rustock_core::ordered_tx_trie_root;

        let mut inner: Vec<u8> = vec![0x83, 0x8d, 0x5a, 0xd7, 0x00, 0x00, 0x94];
        inner.extend_from_slice(&[0u8; 19]);
        inner.push(0x08);
        inner.extend_from_slice(&[0x00, 0x80, 0x00, 0x00, 0x00]);
        let mut raw = Vec::new();
        RlpHeader { list: true, payload_length: inner.len() }.encode(&mut raw);
        raw.extend_from_slice(&inner);

        let tx = Transaction::decode(&mut raw.as_slice()).unwrap();
        let before = ordered_tx_trie_root(std::slice::from_ref(&tx), true);

        let mut buf = Vec::new();
        let msg = RskMessage::new(RskSubMessage::BodyResponse(BodyResponse {
            id: 7,
            transactions: vec![tx],
            uncles: Vec::new(),
        }));
        msg.encode(&mut buf);
        let decoded = RskMessage::decode(&mut buf.as_slice()).expect("decodes");

        match decoded.sub_message {
            RskSubMessage::BodyResponse(r) => {
                assert_eq!(
                    before,
                    ordered_tx_trie_root(&r.transactions, true),
                    "the body wire must not change the transaction root"
                );
            }
            other => panic!("decoded as {other:?}"),
        }
    }

    #[test]
    fn test_rsk_status_rlp() {
        let status = RskStatus {
            best_block_number: 1234,
            best_block_hash: B256::repeat_byte(0x11),
            best_block_parent_hash: Some(B256::repeat_byte(0x22)),
            total_difficulty: Some(U256::from(9999)),
        earliest_block: None,
        };

        let mut buf = Vec::new();
        status.encode(&mut buf);

        let mut decode_buf = buf.as_slice();
        let decoded = RskStatus::decode(&mut decode_buf).unwrap();
        assert_eq!(status, decoded);
    }

    #[test]
    fn test_rsk_message_rlp_status() {
        let status = RskStatus {
            best_block_number: 1,
            best_block_hash: B256::repeat_byte(0xaa),
            best_block_parent_hash: None,
            total_difficulty: None,
        earliest_block: None,
        };
        let msg = RskMessage::new(RskSubMessage::Status(status));

        let mut buf = Vec::new();
        msg.encode(&mut buf);

        let mut decode_buf = buf.as_slice();
        let decoded = RskMessage::decode(&mut decode_buf).unwrap();
        
        if let RskSubMessage::Status(s) = decoded.sub_message {
            assert_eq!(s.best_block_number, 1);
            assert_eq!(s.best_block_hash, B256::repeat_byte(0xaa));
        } else {
            panic!("Expected Status message");
        }
    }

    #[test]
    fn test_rsk_message_rlp_headers_request() {
        let req = BlockHeadersRequest {
            id: 42,
            query: BlockHeadersQuery {
                hash: B256::repeat_byte(0xbb),
                count: 10,
            },
        };
        let msg = RskMessage::new(RskSubMessage::BlockHeadersRequest(req));

        let mut buf = Vec::new();
        msg.encode(&mut buf);

        let mut decode_buf = buf.as_slice();
        let decoded = RskMessage::decode(&mut decode_buf).unwrap();
        
        if let RskSubMessage::BlockHeadersRequest(r) = decoded.sub_message {
            assert_eq!(r.id, 42);
            assert_eq!(r.query.count, 10);
            assert_eq!(r.query.hash, B256::repeat_byte(0xbb));
        } else {
            panic!("Expected BlockHeadersRequest message");
        }
    }

    #[test]
    fn test_rsk_message_rlp_block_hash_request() {
        let req = BlockHashRequest { id: 7, height: 12345 };
        let msg = RskMessage::new(RskSubMessage::BlockHashRequest(req));

        let mut buf = Vec::new();
        msg.encode(&mut buf);

        let mut decode_buf = buf.as_slice();
        let decoded = RskMessage::decode(&mut decode_buf).unwrap();

        if let RskSubMessage::BlockHashRequest(r) = decoded.sub_message {
            assert_eq!(r.id, 7);
            assert_eq!(r.height, 12345);
        } else {
            panic!("Expected BlockHashRequest, got {:?}", decoded.sub_message);
        }
    }

    #[test]
    fn test_rsk_message_rlp_block_hash_response() {
        let resp = BlockHashResponse { id: 7, hash: B256::repeat_byte(0xcc) };
        let msg = RskMessage::new(RskSubMessage::BlockHashResponse(resp));

        let mut buf = Vec::new();
        msg.encode(&mut buf);

        let mut decode_buf = buf.as_slice();
        let decoded = RskMessage::decode(&mut decode_buf).unwrap();

        if let RskSubMessage::BlockHashResponse(r) = decoded.sub_message {
            assert_eq!(r.id, 7);
            assert_eq!(r.hash, B256::repeat_byte(0xcc));
        } else {
            panic!("Expected BlockHashResponse, got {:?}", decoded.sub_message);
        }
    }

    #[test]
    fn test_rsk_message_rlp_skeleton_request() {
        let req = SkeletonRequest { id: 99, start_number: 5000 };
        let msg = RskMessage::new(RskSubMessage::SkeletonRequest(req));

        let mut buf = Vec::new();
        msg.encode(&mut buf);

        let mut decode_buf = buf.as_slice();
        let decoded = RskMessage::decode(&mut decode_buf).unwrap();

        if let RskSubMessage::SkeletonRequest(r) = decoded.sub_message {
            assert_eq!(r.id, 99);
            assert_eq!(r.start_number, 5000);
        } else {
            panic!("Expected SkeletonRequest, got {:?}", decoded.sub_message);
        }
    }

    #[test]
    fn test_rsk_message_rlp_skeleton_response() {
        let resp = SkeletonResponse {
            id: 99,
            block_identifiers: vec![
                BlockIdentifier { hash: B256::repeat_byte(0x01), number: 0 },
                BlockIdentifier { hash: B256::repeat_byte(0x02), number: 192 },
                BlockIdentifier { hash: B256::repeat_byte(0x03), number: 384 },
            ],
        };
        let msg = RskMessage::new(RskSubMessage::SkeletonResponse(resp));

        let mut buf = Vec::new();
        msg.encode(&mut buf);

        let mut decode_buf = buf.as_slice();
        let decoded = RskMessage::decode(&mut decode_buf).unwrap();

        if let RskSubMessage::SkeletonResponse(r) = decoded.sub_message {
            assert_eq!(r.id, 99);
            assert_eq!(r.block_identifiers.len(), 3);
            assert_eq!(r.block_identifiers[0].hash, B256::repeat_byte(0x01));
            assert_eq!(r.block_identifiers[0].number, 0);
            assert_eq!(r.block_identifiers[1].number, 192);
            assert_eq!(r.block_identifiers[2].number, 384);
        } else {
            panic!("Expected SkeletonResponse, got {:?}", decoded.sub_message);
        }
    }

    #[test]
    /// A `Transactions` message as it arrives on the wire: an outer RLP list
    /// whose items are each a COMPLETE transaction -- itself an RLP list.
    ///
    /// The round-trip test below cannot catch a mistake here, because it feeds
    /// our own encoder's output to our own decoder; if both are wrong in the
    /// same way it still passes. This one pins the format itself.
    ///
    /// Mainnet symptom: 19 transactions offered in five minutes, all 19
    /// rejected `rlp-decode`, because the decoder handed the pool each
    /// transaction's payload with its list header stripped off.
    #[test]
    fn transactions_decode_keeps_each_transaction_whole() {
        use alloy_primitives::Bytes;

        // A complete, self-contained RLP list: 0xc4 header + four items.
        // This stands for one transaction.
        let tx: Vec<u8> = vec![0xc4, 0x01, 0x02, 0x03, 0x04];

        // The wire message: an outer list containing that item verbatim.
        let mut inner = Vec::new();
        inner.extend_from_slice(&tx);
        let mut wire = Vec::new();
        RlpHeader { list: true, payload_length: inner.len() }.encode(&mut wire);
        wire.extend_from_slice(&inner);

        // Frame it exactly as rskj's Message.getEncoded does:
        //   RLP([ RLP([ type, RLP_String(params) ]) ])
        let mut msg_rlp = Vec::new();
        (RskMessageType::Transactions as u8).encode(&mut msg_rlp);
        RlpHeader { list: false, payload_length: wire.len() }.encode(&mut msg_rlp);
        msg_rlp.extend_from_slice(&wire);

        let mut wrapped = Vec::new();
        RlpHeader { list: true, payload_length: msg_rlp.len() }.encode(&mut wrapped);
        wrapped.extend_from_slice(&msg_rlp);

        let mut framed = Vec::new();
        RlpHeader { list: true, payload_length: wrapped.len() }.encode(&mut framed);
        framed.extend_from_slice(&wrapped);

        let mut slice = framed.as_slice();
        let decoded = RskMessage::decode(&mut slice).expect("decodes");
        let RskSubMessage::Transactions(txs) = decoded.sub_message else {
            panic!("expected Transactions");
        };
        assert_eq!(txs.len(), 1);
        assert_eq!(
            txs[0],
            Bytes::from(tx.clone()),
            "each item must survive whole, header included -- the pool decodes \
             it as a transaction, which is only valid as a complete RLP list"
        );
    }

    fn test_rsk_message_rlp_transactions() {
        use alloy_primitives::Bytes;

        // Complete RLP lists, as real transactions are. The previous values
        // were truncated fragments that only round-tripped because the encoder
        // wrapped them in a string header and the decoder unwrapped it again --
        // symmetric, and wrong in both directions.
        let tx1 = Bytes::from(vec![0xc4, 0x01, 0x02, 0x03, 0x04]);
        let tx2 = Bytes::from(vec![0xc6, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16]);
        let msg = RskMessage::new(RskSubMessage::Transactions(vec![tx1.clone(), tx2.clone()]));

        let mut buf = Vec::new();
        msg.encode(&mut buf);

        let mut decode_buf = buf.as_slice();
        let decoded = RskMessage::decode(&mut decode_buf).unwrap();

        if let RskSubMessage::Transactions(txs) = decoded.sub_message {
            assert_eq!(txs.len(), 2);
            assert_eq!(txs[0], tx1);
            assert_eq!(txs[1], tx2);
        } else {
            panic!("Expected Transactions, got {:?}", decoded.sub_message);
        }
    }

    #[test]
    fn test_rsk_message_rlp_body_request() {
        let req = BodyRequest { id: 55, hash: B256::repeat_byte(0xdd) };
        let msg = RskMessage::new(RskSubMessage::BodyRequest(req));

        let mut buf = Vec::new();
        msg.encode(&mut buf);

        let mut decode_buf = buf.as_slice();
        let decoded = RskMessage::decode(&mut decode_buf).unwrap();

        if let RskSubMessage::BodyRequest(r) = decoded.sub_message {
            assert_eq!(r.id, 55);
            assert_eq!(r.hash, B256::repeat_byte(0xdd));
        } else {
            panic!("Expected BodyRequest, got {:?}", decoded.sub_message);
        }
    }

    #[test]
    fn test_rsk_message_rlp_body_response_empty() {
        let resp = BodyResponse {
            id: 55,
            transactions: vec![],
            uncles: vec![],
        };
        let msg = RskMessage::new(RskSubMessage::BodyResponse(resp));

        let mut buf = Vec::new();
        msg.encode(&mut buf);

        let mut decode_buf = buf.as_slice();
        let decoded = RskMessage::decode(&mut decode_buf).unwrap();

        if let RskSubMessage::BodyResponse(r) = decoded.sub_message {
            assert_eq!(r.id, 55);
            assert!(r.transactions.is_empty());
            assert!(r.uncles.is_empty());
        } else {
            panic!("Expected BodyResponse, got {:?}", decoded.sub_message);
        }
    }

    #[test]
    fn test_rsk_message_rlp_body_response_with_data() {
        use rustock_core::Transaction;

        let tx = Transaction {
            nonce: 1,
            gas_price: U256::from(10),
            gas_limit: U256::from(21000),
            to: Bytes::from(vec![0x12; 20]),
            value: U256::from(1000),
            input: Bytes::default(),
            v: 27,
            r: U256::from(88),
            s: U256::from(99),
            cached_rlp: None,
        };

        let resp = BodyResponse {
            id: 77,
            transactions: vec![tx],
            uncles: vec![],
        };
        let msg = RskMessage::new(RskSubMessage::BodyResponse(resp));

        let mut buf = Vec::new();
        msg.encode(&mut buf);

        let mut decode_buf = buf.as_slice();
        let decoded = RskMessage::decode(&mut decode_buf).unwrap();

        if let RskSubMessage::BodyResponse(r) = decoded.sub_message {
            assert_eq!(r.id, 77);
            assert_eq!(r.transactions.len(), 1);
            assert_eq!(r.transactions[0].nonce, 1);
            assert!(r.uncles.is_empty());
        } else {
            panic!("Expected BodyResponse, got {:?}", decoded.sub_message);
        }
    }

    #[test]
    fn test_rsk_message_rlp_body_response_with_txs_and_uncles() {
        use rustock_core::Transaction;
        use alloy_primitives::Address;

        fn make_tx(nonce: u64) -> Transaction {
            Transaction {
                nonce,
                gas_price: U256::from(20_000_000_000u64),
                gas_limit: U256::from(21000),
                to: Bytes::from(Address::repeat_byte(0xAA).as_slice().to_vec()),
                value: U256::from(nonce * 1000 + 1000),
                input: Bytes::default(),
                v: 27,
                r: U256::from(nonce + 100),
                s: U256::from(nonce + 200),
                cached_rlp: None,
            }
        }

        fn make_uncle(number: u64) -> Header {
            Header {
                number,
                parent_hash: B256::repeat_byte(number as u8),
                ommers_hash: B256::ZERO,
                beneficiary: Address::ZERO,
                state_root: B256::ZERO,
                transactions_root: B256::ZERO,
                receipts_root: B256::ZERO,
                logs_bloom: Default::default(),
                extension_data: None,
                difficulty: U256::from(1000),
                gas_limit: U256::from(8_000_000),
                gas_used: 0,
                timestamp: number * 15,
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

        let transactions: Vec<Transaction> = (1..=10).map(make_tx).collect();
        let uncles: Vec<Header> = (1..=9).map(make_uncle).collect();

        let resp = BodyResponse {
            id: 100,
            transactions: transactions.clone(),
            uncles: uncles.clone(),
        };
        let msg = RskMessage::new(RskSubMessage::BodyResponse(resp));

        let mut buf = Vec::new();
        msg.encode(&mut buf);

        let mut decode_buf = buf.as_slice();
        let decoded = RskMessage::decode(&mut decode_buf).unwrap();

        if let RskSubMessage::BodyResponse(r) = decoded.sub_message {
            assert_eq!(r.id, 100);
            assert_eq!(r.transactions.len(), 10);
            assert_eq!(r.uncles.len(), 9);

            for (i, tx) in r.transactions.iter().enumerate() {
                let n = (i + 1) as u64;
                assert_eq!(tx.nonce, n, "tx {i} nonce mismatch");
                assert_eq!(tx.value, U256::from(n * 1000 + 1000));
            }

            for (i, uncle) in r.uncles.iter().enumerate() {
                let n = (i + 1) as u64;
                assert_eq!(uncle.number, n, "uncle {i} number mismatch");
                assert_eq!(uncle.parent_hash, B256::repeat_byte(n as u8));
            }
        } else {
            panic!("Expected BodyResponse, got {:?}", decoded.sub_message);
        }
    }

    #[test]
    fn test_rsk_message_rlp_body_response_uncles_only() {
        use alloy_primitives::Address;

        let uncle = Header {
            number: 42,
            parent_hash: B256::repeat_byte(0x11),
            ommers_hash: B256::ZERO,
            beneficiary: Address::ZERO,
            state_root: B256::ZERO,
            transactions_root: B256::ZERO,
            receipts_root: B256::ZERO,
            logs_bloom: Default::default(),
            extension_data: None,
            difficulty: U256::from(500),
            gas_limit: U256::from(8_000_000),
            gas_used: 0,
            timestamp: 630,
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
        };

        let resp = BodyResponse {
            id: 88,
            transactions: vec![],
            uncles: vec![uncle],
        };
        let msg = RskMessage::new(RskSubMessage::BodyResponse(resp));

        let mut buf = Vec::new();
        msg.encode(&mut buf);

        let mut decode_buf = buf.as_slice();
        let decoded = RskMessage::decode(&mut decode_buf).unwrap();

        if let RskSubMessage::BodyResponse(r) = decoded.sub_message {
            assert_eq!(r.id, 88);
            assert!(r.transactions.is_empty());
            assert_eq!(r.uncles.len(), 1);
            assert_eq!(r.uncles[0].number, 42);
            assert_eq!(r.uncles[0].difficulty, U256::from(500));
        } else {
            panic!("Expected BodyResponse, got {:?}", decoded.sub_message);
        }
    }

    #[test]
    fn test_rsk_message_rlp_transactions_empty() {
        let msg = RskMessage::new(RskSubMessage::Transactions(vec![]));

        let mut buf = Vec::new();
        msg.encode(&mut buf);

        let mut decode_buf = buf.as_slice();
        let decoded = RskMessage::decode(&mut decode_buf).unwrap();

        if let RskSubMessage::Transactions(txs) = decoded.sub_message {
            assert!(txs.is_empty());
        } else {
            panic!("Expected Transactions, got {:?}", decoded.sub_message);
        }
    }
}

#[cfg(test)]
mod block_range_tests {
    use super::*;

    fn status(earliest: Option<u64>) -> RskStatus {
        RskStatus {
            best_block_number: 9_000_000,
            best_block_hash: B256::repeat_byte(0x11),
            best_block_parent_hash: Some(B256::repeat_byte(0x22)),
            total_difficulty: Some(U256::from(123_456_789u64)),
            earliest_block: earliest,
        }
    }

    /// The fifth element goes after the four rskj reads, never instead of one.
    ///
    /// rskj's decoder takes indices 0..3 and never consults the length again,
    /// so an extra element is ignored — but only because it is *extra*.
    /// Replacing total difficulty, as `eth/69` did, would change what index 3
    /// means and make every rskj peer read a block number as a difficulty.
    #[test]
    fn the_extension_is_appended_and_displaces_nothing() {
        let mut plain = Vec::new();
        status(None).encode(&mut plain);
        let mut extended = Vec::new();
        status(Some(8_992_000)).encode(&mut extended);

        // Compare payloads, not whole messages: the list header carries the
        // length, so it differs by construction.
        let payload = |mut buf: &[u8]| -> Vec<u8> {
            let h = RlpHeader::decode(&mut buf).expect("header");
            buf[..h.payload_length].to_vec()
        };
        let plain = payload(&plain);
        let extended = payload(&extended);

        assert!(
            extended.len() > plain.len(),
            "the extension must add to the message, not rewrite it"
        );
        assert_eq!(
            &extended[..plain.len()],
            &plain[..],
            "the four fields rskj reads must be byte-identical and in place; \
             replacing one, as eth/69 did with total difficulty, would make every \
             rskj peer read this node's numbers as something else"
        );
    }

    #[test]
    fn a_status_with_the_extension_round_trips() {
        let original = status(Some(8_992_000));
        let mut buf = Vec::new();
        original.encode(&mut buf);
        let decoded = RskStatus::decode(&mut buf.as_slice()).expect("decodes");
        assert_eq!(decoded.earliest_block, Some(8_992_000));
        assert_eq!(decoded.best_block_number, original.best_block_number);
        assert_eq!(decoded.total_difficulty, original.total_difficulty);
    }

    /// What an rskj peer sends: four elements, no range. It must read as
    /// "did not say", never as "serves nothing from zero".
    #[test]
    fn a_four_element_status_says_nothing_rather_than_zero() {
        let mut buf = Vec::new();
        status(None).encode(&mut buf);
        let decoded = RskStatus::decode(&mut buf.as_slice()).expect("decodes");
        assert_eq!(
            decoded.earliest_block, None,
            "silence must stay distinguishable from a stated floor of zero"
        );
    }

    /// And the two-element form rskj also emits.
    #[test]
    fn a_two_element_status_still_decodes() {
        let bare = RskStatus {
            best_block_number: 42,
            best_block_hash: B256::repeat_byte(0x33),
            best_block_parent_hash: None,
            total_difficulty: None,
            earliest_block: None,
        };
        let mut buf = Vec::new();
        bare.encode(&mut buf);
        let decoded = RskStatus::decode(&mut buf.as_slice()).expect("decodes");
        assert_eq!(decoded.best_block_number, 42);
        assert_eq!(decoded.earliest_block, None);
    }

    /// A sixth element, as some later extension would add, must not break
    /// this one — the same courtesy rskj extends to us.
    #[test]
    fn a_later_extension_does_not_break_this_one() {
        let mut list = Vec::new();
        9_000_000u64.encode(&mut list);
        B256::repeat_byte(0x11).encode(&mut list);
        B256::repeat_byte(0x22).encode(&mut list);
        U256::from(7u64).encode(&mut list);
        8_992_000u64.encode(&mut list);
        99u64.encode(&mut list); // whatever comes next
        let mut buf = Vec::new();
        RlpHeader { list: true, payload_length: list.len() }.encode(&mut buf);
        buf.extend_from_slice(&list);

        let decoded = RskStatus::decode(&mut buf.as_slice()).expect("decodes");
        assert_eq!(decoded.earliest_block, Some(8_992_000));
    }

    #[test]
    fn a_block_range_round_trips() {
        let range = BlockRange {
            earliest_block: 9_270_000,
            latest_block: 9_280_000,
            latest_block_hash: B256::repeat_byte(0x44),
        };
        let mut buf = Vec::new();
        range.encode(&mut buf);
        assert_eq!(BlockRange::decode(&mut buf.as_slice()).expect("decodes"), range);
    }

    /// 26 is free in rskj, whose highest message type is 25.
    #[test]
    fn the_new_message_type_does_not_collide() {
        assert_eq!(RskMessageType::BlockRangeUpdate as u8, 26);
        assert_eq!(RskMessageType::SnapBlocksResponse as u8, 25);
    }
}

#[cfg(test)]
mod rskj_interop {
    use super::*;

    /// Dumps the exact bytes this node puts on the wire for an extended
    /// status, so they can be fed to rskj's own decoder.
    ///
    /// Run with `cargo test -p rustock-networking rskj_interop -- --nocapture --ignored`.
    /// The companion Java probe decodes the output through
    /// `co.rsk.net.messages.Message.create`, which is what an rskj node runs.
    #[test]
    #[ignore = "produces input for the cross-implementation check"]
    fn dump_wire_bytes_for_rskj() {
        for (label, earliest) in [("four-element", None), ("extended", Some(8_992_000u64))] {
            let status = RskStatus {
                best_block_number: 9_000_000,
                best_block_hash: B256::repeat_byte(0x11),
                best_block_parent_hash: Some(B256::repeat_byte(0x22)),
                total_difficulty: Some(U256::from(123_456_789u64)),
                earliest_block: earliest,
            };
            let msg = RskMessage::new(RskSubMessage::Status(status));
            let mut buf = Vec::new();
            alloy_rlp::Encodable::encode(&msg, &mut buf);
            let hex: String = buf.iter().map(|b| format!("{b:02x}")).collect();
            println!("{label} {hex}");
        }
    }
}
