//! Peer scoring, punishment and banning.
//!
//! A port of rskj's `co.rsk.scoring` package: `EventType`, `PeerScoring`,
//! `ScoringCalculator`, `PunishmentCalculator`, `PeerScoringManager`,
//! `InetAddressTable` and `InetAddressCidrBlock`. `docs/peer-scoring.md`
//! records the differences and the reasoning.
//!
//! # The shape, in one paragraph
//!
//! Every peer accumulates a counter per event type, keyed **twice** -- once by
//! node id and once by IP address. From those counters a score and a
//! `good_reputation` flag are derived. Losing good reputation starts a
//! *punishment* with an expiry that lengthens each time the same peer is
//! punished again. When the punishment expires the counters reset and the peer
//! is welcome back. Separately, an operator can ban an address or a CIDR block
//! outright, which no amount of good behaviour lifts.
//!
//! # Why keyed twice
//!
//! rskj's own comment: "to collect events for the same node_id, but maybe with
//! different address along the time. Or same address with different node id."
//! A peer that reconnects with a fresh node id keeps its address history, and
//! a peer that changes address keeps its id history. Scoring only by node id
//! would be defeated by regenerating a key, which costs nothing.

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::IpAddr;
use std::time::{Duration, Instant};

use alloy_primitives::{hex, B512};

/// rskj's `EventType`, in declaration order.
///
/// The order is not cosmetic: rskj indexes `counters[evt.ordinal()]`, and
/// `PeerScoringInformation` reports the counters by name. Keeping the order
/// keeps the two readable side by side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventType {
    InvalidNetwork,
    InvalidBlock,
    InvalidTransaction,
    InvalidMessage,
    ValidBlock,
    ValidTransaction,
    SuccessfulHandshake,
    FailedHandshake,
    IncompatibleProtocol,
    UnexpectedGenesis,
    Disconnection,
    RepeatedMessage,
    UnexpectedMessage,
    TimeoutMessage,
    InvalidHeader,
}

impl EventType {
    pub const ALL: [EventType; 15] = [
        EventType::InvalidNetwork,
        EventType::InvalidBlock,
        EventType::InvalidTransaction,
        EventType::InvalidMessage,
        EventType::ValidBlock,
        EventType::ValidTransaction,
        EventType::SuccessfulHandshake,
        EventType::FailedHandshake,
        EventType::IncompatibleProtocol,
        EventType::UnexpectedGenesis,
        EventType::Disconnection,
        EventType::RepeatedMessage,
        EventType::UnexpectedMessage,
        EventType::TimeoutMessage,
        EventType::InvalidHeader,
    ];

    fn index(self) -> usize {
        Self::ALL.iter().position(|e| *e == self).unwrap_or(0)
    }

    /// The name `sco_peerList` reports this counter under, matching rskj's
    /// `PeerScoringInformation` getters.
    pub fn report_name(self) -> &'static str {
        match self {
            EventType::InvalidNetwork => "invalidNetworks",
            EventType::InvalidBlock => "invalidBlocks",
            EventType::InvalidTransaction => "invalidTransactions",
            EventType::InvalidMessage => "invalidMessages",
            EventType::ValidBlock => "validBlocks",
            EventType::ValidTransaction => "validTransactions",
            EventType::SuccessfulHandshake => "successfulHandshakes",
            EventType::FailedHandshake => "failedHandshakes",
            EventType::IncompatibleProtocol => "incompatibleProtocol",
            EventType::UnexpectedGenesis => "unexpectedGenesis",
            EventType::Disconnection => "disconnections",
            EventType::RepeatedMessage => "repeatedMessages",
            EventType::UnexpectedMessage => "unexpectedMessages",
            EventType::TimeoutMessage => "timeoutMessages",
            EventType::InvalidHeader => "invalidHeader",
        }
    }
}

/// rskj's `PunishmentParameters`.
#[derive(Debug, Clone, Copy)]
pub struct PunishmentParameters {
    /// First punishment's length.
    pub duration: Duration,
    /// Percentage added on each repeat, as rskj's `increment`.
    pub increment_rate: u64,
    /// `None` is rskj's `maximum: 0`, meaning no ceiling.
    pub maximum: Option<Duration>,
}

impl PunishmentParameters {
    /// rskj `reference.conf`: `scoring.nodes = { duration: 12, increment: 10,
    /// maximum: 0 }` -- minutes, and **no maximum**, so a node id punished
    /// often enough is punished for a very long time. That is deliberate:
    /// a node id is free to regenerate, so there is no cost to being wrong
    /// about one.
    pub fn nodes() -> Self {
        Self {
            duration: Duration::from_secs(12 * 60),
            increment_rate: 10,
            maximum: None,
        }
    }

    /// rskj `reference.conf`: `scoring.addresses = { duration: 12,
    /// increment: 10, maximum: 6000 }` -- 6,000 minutes is a little over four
    /// days. An address **is** costly to be wrong about (a NAT can put many
    /// honest peers behind one), so this one is capped.
    pub fn addresses() -> Self {
        Self {
            duration: Duration::from_secs(12 * 60),
            increment_rate: 10,
            maximum: Some(Duration::from_secs(6_000 * 60)),
        }
    }
}

/// rskj's `PunishmentCalculator.calculate`.
///
/// The duration grows by `increment_rate` percent per previous punishment,
/// and is then multiplied by `-score` when the score is negative. Both steps
/// are clamped by the maximum, and rskj returns **early** at the maximum
/// inside the growth loop -- before the score multiplier -- which this keeps.
pub fn punishment_duration(
    params: &PunishmentParameters,
    punishment_counter: u32,
    score: i32,
) -> Duration {
    let mut result = params.duration.as_millis() as u128;
    let rate = 100u128 + params.increment_rate as u128;
    let max = params.maximum.map(|d| d.as_millis());

    for _ in 0..punishment_counter {
        result = result.saturating_mul(rate) / 100;
        if let Some(max) = max {
            if result > max {
                return Duration::from_millis(max as u64);
            }
        }
    }

    if score < 0 {
        // rskj: `result *= -score`. `score` is an `int`, so `-score` of
        // `Integer.MIN_VALUE` overflows there; saturating here instead, since
        // a score that low means "punish for the maximum" either way.
        result = result.saturating_mul(score.unsigned_abs() as u128);
    }

    let result = match max {
        Some(max) => result.min(max),
        None => result,
    };
    Duration::from_millis(result.min(u64::MAX as u128) as u64)
}

/// One peer's counters, score and punishment state -- rskj's `PeerScoring`.
#[derive(Debug, Clone)]
pub struct PeerScoring {
    counters: [u32; 15],
    score: i32,
    good_reputation: bool,
    /// When good reputation was lost, and for how long.
    punished_at: Option<Instant>,
    punishment: Duration,
    punishment_counter: u32,
    punishment_enabled: bool,
}

impl Default for PeerScoring {
    fn default() -> Self {
        Self::new(true)
    }
}

impl PeerScoring {
    pub fn new(punishment_enabled: bool) -> Self {
        Self {
            counters: [0; 15],
            score: 0,
            good_reputation: true,
            punished_at: None,
            punishment: Duration::ZERO,
            punishment_counter: 0,
            punishment_enabled,
        }
    }

    /// rskj `PeerScoring.updateScoring`.
    ///
    /// Three groups, and the middle one is the surprise:
    ///
    /// * the five "invalid" events **reset a positive score to zero** before
    ///   decrementing, so a peer with a long good history gets no credit
    ///   against a single invalid block;
    /// * `UNEXPECTED_MESSAGE`, `FAILED_HANDSHAKE`, `SUCCESSFUL_HANDSHAKE`,
    ///   `REPEATED_MESSAGE` and `TIMEOUT_MESSAGE` **do not move the score at
    ///   all** -- they only increment their counter. A failed handshake is
    ///   recorded and costs nothing, which is worth knowing before assuming
    ///   the score reflects everything in the report;
    /// * everything else increments, but only while the score is already
    ///   non-negative -- so a punished peer cannot climb back out by being
    ///   useful. Only the punishment expiring clears it.
    pub fn update(&mut self, event: EventType) {
        self.counters[event.index()] = self.counters[event.index()].saturating_add(1);

        match event {
            EventType::InvalidNetwork
            | EventType::InvalidBlock
            | EventType::InvalidTransaction
            | EventType::InvalidMessage
            | EventType::InvalidHeader => {
                if self.score > 0 {
                    self.score = 0;
                }
                self.score = self.score.saturating_sub(1);
            }
            EventType::UnexpectedMessage
            | EventType::FailedHandshake
            | EventType::SuccessfulHandshake
            | EventType::RepeatedMessage
            | EventType::TimeoutMessage => {}
            _ => {
                if self.score >= 0 {
                    self.score = self.score.saturating_add(1);
                }
            }
        }
    }

    /// rskj `ScoringCalculator.hasGoodScore`: **only three** of the fifteen
    /// counters decide it.
    ///
    /// A peer can fail every handshake, time out every message and disconnect
    /// repeatedly without ever losing reputation. rskj's own comment says why
    /// timeouts are excluded -- "implement empty messages as responses so
    /// timeout can be handled as it should" -- which is a TODO, not a
    /// decision. Reproduced, because a node that punished on timeouts would
    /// ban peers rskj keeps, and disagreeing about peers is how a node ends up
    /// partitioned.
    pub fn has_good_score(&self) -> bool {
        self.counter(EventType::InvalidBlock) < 1
            && self.counter(EventType::InvalidMessage) < 1
            && self.counter(EventType::InvalidHeader) < 1
    }

    pub fn counter(&self, event: EventType) -> u32 {
        self.counters[event.index()]
    }

    pub fn total_events(&self) -> u32 {
        self.counters.iter().copied().fold(0u32, u32::saturating_add)
    }

    pub fn score(&self) -> i32 {
        self.score
    }

    pub fn punishment_counter(&self) -> u32 {
        self.punishment_counter
    }

    /// When the current punishment ends, if one is running.
    pub fn punished_until(&self, now: Instant) -> Option<Duration> {
        let at = self.punished_at?;
        let end = at + self.punishment;
        (end > now).then(|| end - now)
    }

    /// rskj `refreshReputationAndPunishment`: ends an expired punishment and
    /// reports the reputation.
    pub fn refresh(&mut self, now: Instant) -> bool {
        if self.good_reputation {
            return true;
        }
        if let Some(at) = self.punished_at {
            if !self.punishment.is_zero() && at + self.punishment <= now {
                self.end_punishment();
            }
        }
        self.good_reputation
    }

    /// rskj `startPunishment`. Returns false when punishment is disabled by
    /// configuration, which is how rskj lets an operator run with scoring
    /// observable but inert.
    pub fn start_punishment(&mut self, duration: Duration, now: Instant) -> bool {
        if !self.punishment_enabled {
            return false;
        }
        self.good_reputation = false;
        self.punishment = duration;
        self.punishment_counter = self.punishment_counter.saturating_add(1);
        self.punished_at = Some(now);
        true
    }

    /// rskj `endPunishment`: every counter back to zero, score back to zero,
    /// reputation restored. **The punishment counter is not reset** -- that is
    /// what makes the next punishment longer.
    fn end_punishment(&mut self) {
        if !self.punishment_enabled {
            return;
        }
        self.counters = [0; 15];
        self.good_reputation = true;
        self.punishment = Duration::ZERO;
        self.punished_at = None;
        self.score = 0;
    }
}

/// A banned CIDR block -- rskj's `InetAddressCidrBlock`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CidrBlock {
    description: String,
    bytes: Vec<u8>,
    /// Mask for the one partially-covered byte; zero when the prefix lands on
    /// a byte boundary.
    mask: u8,
    full_bytes: usize,
}

impl CidrBlock {
    /// `address/prefix`, e.g. `192.168.51.1/16`.
    ///
    /// rskj computes the partial-byte mask as
    /// `(byte)(0xFF00 >> (cidr & 0x07))`, which is zero exactly when the
    /// prefix is a whole number of bytes -- the case where no partial
    /// comparison is wanted. The same expression is used here so the two
    /// agree on every prefix length, including the odd ones.
    pub fn new(address: IpAddr, prefix: u32) -> Result<Self, ScoringError> {
        let bytes = match address {
            IpAddr::V4(a) => a.octets().to_vec(),
            IpAddr::V6(a) => a.octets().to_vec(),
        };
        // rskj `createCidrBlock`: `nbits <= 0 || nbits > length * 8` is
        // invalid, so `/0` is rejected -- you cannot ban the whole internet
        // by accident.
        if prefix == 0 || prefix as usize > bytes.len() * 8 {
            return Err(ScoringError::InvalidMask);
        }
        Ok(Self {
            description: format!("{address}/{prefix}"),
            bytes,
            mask: (0xFF00u32 >> (prefix & 0x07)) as u8,
            full_bytes: (prefix / 8) as usize,
        })
    }

    pub fn contains(&self, address: IpAddr) -> bool {
        let other = match address {
            IpAddr::V4(a) => a.octets().to_vec(),
            IpAddr::V6(a) => a.octets().to_vec(),
        };
        if other.len() != self.bytes.len() {
            return false;
        }
        if other[..self.full_bytes] != self.bytes[..self.full_bytes] {
            return false;
        }
        if self.mask != 0 {
            // `full_bytes` is in range whenever the mask is non-zero: a
            // non-zero mask means the prefix is not a whole number of bytes,
            // so it is strictly less than `bytes.len() * 8`.
            return other[self.full_bytes] & self.mask == self.bytes[self.full_bytes] & self.mask;
        }
        true
    }

    pub fn description(&self) -> &str {
        &self.description
    }
}

/// rskj's `InetAddressTable`: banned single addresses and banned blocks.
#[derive(Debug, Default, Clone)]
pub struct AddressTable {
    addresses: HashSet<IpAddr>,
    blocks: Vec<CidrBlock>,
}

impl AddressTable {
    pub fn contains(&self, address: IpAddr) -> bool {
        self.addresses.contains(&address) || self.blocks.iter().any(|b| b.contains(address))
    }

    pub fn ban_address(&mut self, address: IpAddr) {
        self.addresses.insert(address);
    }

    pub fn unban_address(&mut self, address: IpAddr) {
        self.addresses.remove(&address);
    }

    pub fn ban_block(&mut self, block: CidrBlock) {
        if !self.blocks.contains(&block) {
            self.blocks.push(block);
        }
    }

    pub fn unban_block(&mut self, block: &CidrBlock) {
        self.blocks.retain(|b| b != block);
    }

    /// rskj `getBannedAddresses`: single addresses first, then block
    /// descriptions, both as strings in one list.
    pub fn descriptions(&self) -> Vec<String> {
        let mut out: Vec<String> = self.addresses.iter().map(|a| a.to_string()).collect();
        out.sort();
        out.extend(self.blocks.iter().map(|b| b.description.clone()));
        out
    }

    pub fn is_empty(&self) -> bool {
        self.addresses.is_empty() && self.blocks.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScoringError {
    EmptyAddress,
    /// rskj refuses to ban a loopback or wildcard address.
    LocalAddress(String),
    UnknownHost(String),
    InvalidMask,
}

impl std::fmt::Display for ScoringError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScoringError::EmptyAddress => write!(f, "empty address"),
            ScoringError::LocalAddress(a) => write!(f, "local address: '{a}'"),
            ScoringError::UnknownHost(a) => write!(f, "unknown host: '{a}'"),
            ScoringError::InvalidMask => write!(f, "Invalid mask"),
        }
    }
}

impl std::error::Error for ScoringError {}

/// rskj `InetAddressUtils.getAddressForBan`.
///
/// Note it **rejects loopback and wildcard addresses**, so `127.0.0.1` and
/// `0.0.0.0` cannot be banned. That is a guard against an operator locking out
/// their own tooling, and it applies to the config file as much as to the RPC.
pub fn parse_ban_address(text: &str) -> Result<IpAddr, ScoringError> {
    let name = text.trim();
    if name.is_empty() {
        return Err(ScoringError::EmptyAddress);
    }
    let address: IpAddr = name
        .parse()
        .map_err(|_| ScoringError::UnknownHost(name.to_string()))?;
    if address.is_loopback() || address.is_unspecified() {
        return Err(ScoringError::LocalAddress(name.to_string()));
    }
    Ok(address)
}

/// rskj `InetAddressUtils.hasMask` plus `createCidrBlock`: a ban target is a
/// block when it contains a `/` with non-empty text on both sides.
pub fn parse_ban_target(text: &str) -> Result<BanTarget, ScoringError> {
    let parts: Vec<&str> = text.split('/').collect();
    if parts.len() == 2 && !parts[0].is_empty() && !parts[1].is_empty() {
        let address = parse_ban_address(parts[0])?;
        let prefix: u32 = parts[1].parse().map_err(|_| ScoringError::InvalidMask)?;
        return Ok(BanTarget::Block(CidrBlock::new(address, prefix)?));
    }
    Ok(BanTarget::Address(parse_ban_address(text)?))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BanTarget {
    Address(IpAddr),
    Block(CidrBlock),
}

/// How many node ids to keep scoring for -- rskj `scoring.nodes.number`.
pub const DEFAULT_NODE_CAPACITY: usize = 100;

/// A snapshot of one entry, for `sco_peerList`.
#[derive(Debug, Clone)]
pub struct ScoringInformation {
    pub id: String,
    /// `"node"` or `"address"`, as rskj labels them.
    pub kind: &'static str,
    pub counters: Vec<(&'static str, u32)>,
    pub score: i32,
    pub punishments: u32,
    pub good_reputation: bool,
    /// Milliseconds from now until the punishment ends; 0 when not punished.
    pub punished_until_ms: u64,
}

/// rskj's `PeerScoringManager`.
///
/// Not internally locked: the sync service owns one and reaches it through the
/// same lock it uses for the rest of its peer state. rskj synchronises inside
/// because its callers are spread across threads; here they are not.
pub struct PeerScoringManager {
    by_node: HashMap<B512, PeerScoring>,
    /// Insertion/most-recent-use order for the node map, so it can be capped
    /// the way rskj's access-ordered `LinkedHashMap` is.
    node_order: VecDeque<B512>,
    node_capacity: usize,
    by_address: HashMap<IpAddr, PeerScoring>,
    banned_addresses: AddressTable,
    banned_node_ids: HashSet<B512>,
    node_params: PunishmentParameters,
    address_params: PunishmentParameters,
    punishment_enabled: bool,
}

impl Default for PeerScoringManager {
    fn default() -> Self {
        Self::new(DEFAULT_NODE_CAPACITY, true)
    }
}

impl PeerScoringManager {
    pub fn new(node_capacity: usize, punishment_enabled: bool) -> Self {
        Self {
            by_node: HashMap::new(),
            node_order: VecDeque::new(),
            node_capacity: node_capacity.max(1),
            by_address: HashMap::new(),
            banned_addresses: AddressTable::default(),
            banned_node_ids: HashSet::new(),
            node_params: PunishmentParameters::nodes(),
            address_params: PunishmentParameters::addresses(),
            punishment_enabled,
        }
    }

    /// rskj `recordEvent`: the event is recorded against the node id **and**
    /// the address, whichever of the two the caller has.
    pub fn record(&mut self, id: Option<B512>, address: Option<IpAddr>, event: EventType) {
        self.record_at(id, address, event, Instant::now())
    }

    pub fn record_at(
        &mut self,
        id: Option<B512>,
        address: Option<IpAddr>,
        event: EventType,
        now: Instant,
    ) {
        if let Some(id) = id {
            self.touch_node(id);
            let params = self.node_params;
            let enabled = self.punishment_enabled;
            let scoring = self
                .by_node
                .entry(id)
                .or_insert_with(|| PeerScoring::new(enabled));
            Self::record_and_maybe_punish(scoring, event, &params, now);
        }
        if let Some(address) = address {
            let params = self.address_params;
            let enabled = self.punishment_enabled;
            let scoring = self
                .by_address
                .entry(address)
                .or_insert_with(|| PeerScoring::new(enabled));
            Self::record_and_maybe_punish(scoring, event, &params, now);
        }
    }

    /// rskj `recordEventAndStartPunishment`, in its order, which matters.
    ///
    /// The event is counted **first**, then the reputation is refreshed. So an
    /// event arriving after a punishment has expired is counted and then
    /// immediately wiped by `endPunishment`'s counter reset -- the first event
    /// after an expiry is effectively swallowed. That is rskj's behaviour and
    /// is reproduced rather than tidied: a peer that misbehaves once every
    /// punishment period never accumulates, on either node.
    ///
    /// The refresh also has to happen *here* and not only when someone asks
    /// about reputation. Otherwise a peer whose punishment expired while
    /// nobody was looking keeps its stale counters, is never re-punished, and
    /// its punishment counter stops growing.
    fn record_and_maybe_punish(
        scoring: &mut PeerScoring,
        event: EventType,
        params: &PunishmentParameters,
        now: Instant,
    ) {
        scoring.update(event);

        // Already serving a punishment: not re-punished, so the counter (and
        // therefore the next duration) grows once per episode rather than once
        // per bad message.
        if !scoring.refresh(now) {
            return;
        }

        if !scoring.has_good_score() {
            let duration = punishment_duration(params, scoring.punishment_counter, scoring.score);
            scoring.start_punishment(duration, now);
        }
    }

    /// Keep the node map bounded, evicting least-recently-touched first, as
    /// rskj's access-ordered `LinkedHashMap` with `removeEldestEntry` does.
    fn touch_node(&mut self, id: B512) {
        if let Some(pos) = self.node_order.iter().position(|n| *n == id) {
            self.node_order.remove(pos);
        }
        self.node_order.push_back(id);
        while self.node_order.len() > self.node_capacity {
            if let Some(evicted) = self.node_order.pop_front() {
                self.by_node.remove(&evicted);
            }
        }
    }

    /// rskj `hasGoodReputation(NodeID)`.
    pub fn node_has_good_reputation(&mut self, id: B512) -> bool {
        self.node_has_good_reputation_at(id, Instant::now())
    }

    pub fn node_has_good_reputation_at(&mut self, id: B512, now: Instant) -> bool {
        if self.banned_node_ids.contains(&id) {
            return false;
        }
        match self.by_node.get_mut(&id) {
            Some(scoring) => scoring.refresh(now),
            // rskj's `getPeerScoring` inserts an empty scoring here, which is
            // always good. Not inserting avoids growing the map from a
            // read-only question; the answer is the same.
            None => true,
        }
    }

    /// rskj `hasGoodReputation(InetAddress)`.
    pub fn address_has_good_reputation(&mut self, address: IpAddr) -> bool {
        self.address_has_good_reputation_at(address, Instant::now())
    }

    pub fn address_has_good_reputation_at(&mut self, address: IpAddr, now: Instant) -> bool {
        if self.banned_addresses.contains(address) {
            return false;
        }
        match self.by_address.get_mut(&address) {
            Some(scoring) => scoring.refresh(now),
            None => true,
        }
    }

    pub fn is_address_banned(&self, address: IpAddr) -> bool {
        self.banned_addresses.contains(address)
    }

    pub fn is_node_banned(&self, id: B512) -> bool {
        self.banned_node_ids.contains(&id)
    }

    /// `sco_banAddress`: an address or a CIDR block.
    pub fn ban(&mut self, text: &str) -> Result<(), ScoringError> {
        match parse_ban_target(text)? {
            BanTarget::Address(a) => self.banned_addresses.ban_address(a),
            BanTarget::Block(b) => self.banned_addresses.ban_block(b),
        }
        Ok(())
    }

    /// `sco_unbanAddress`.
    pub fn unban(&mut self, text: &str) -> Result<(), ScoringError> {
        match parse_ban_target(text)? {
            BanTarget::Address(a) => self.banned_addresses.unban_address(a),
            BanTarget::Block(b) => self.banned_addresses.unban_block(&b),
        }
        Ok(())
    }

    pub fn ban_node_id(&mut self, id: B512) {
        self.banned_node_ids.insert(id);
    }

    /// `sco_bannedAddresses`.
    pub fn banned_addresses(&self) -> Vec<String> {
        self.banned_addresses.descriptions()
    }

    pub fn address_table(&self) -> &AddressTable {
        &self.banned_addresses
    }

    /// `sco_clearPeerScoring`, by address or by node id.
    ///
    /// Note this drops the **punishment counter** along with the rest, so a
    /// cleared peer's next punishment is a first punishment again. rskj does
    /// the same -- the whole entry is removed from the map.
    pub fn clear_address(&mut self, address: IpAddr) -> bool {
        self.by_address.remove(&address).is_some()
    }

    pub fn clear_node(&mut self, id: B512) -> bool {
        if let Some(pos) = self.node_order.iter().position(|n| *n == id) {
            self.node_order.remove(pos);
        }
        self.by_node.remove(&id).is_some()
    }

    /// `sco_peerList`: every entry, by node id then by address.
    pub fn information(&mut self) -> Vec<ScoringInformation> {
        self.information_at(Instant::now())
    }

    pub fn information_at(&mut self, now: Instant) -> Vec<ScoringInformation> {
        let mut out = Vec::with_capacity(self.by_node.len() + self.by_address.len());
        // rskj reports the node id truncated to its first 8 hex characters.
        let nodes: Vec<B512> = self.node_order.iter().copied().collect();
        for id in nodes {
            if let Some(scoring) = self.by_node.get_mut(&id) {
                let label = hex::encode(id.as_slice());
                out.push(Self::info(&label[..8.min(label.len())], "node", scoring, now));
            }
        }
        let mut addresses: Vec<IpAddr> = self.by_address.keys().copied().collect();
        addresses.sort();
        for address in addresses {
            if let Some(scoring) = self.by_address.get_mut(&address) {
                out.push(Self::info(&address.to_string(), "address", scoring, now));
            }
        }
        out
    }

    fn info(
        id: &str,
        kind: &'static str,
        scoring: &mut PeerScoring,
        now: Instant,
    ) -> ScoringInformation {
        // rskj calls `refreshReputationAndPunishment()` while building the
        // report, so simply asking for the peer list can end an expired
        // punishment. Kept, because an operator watching the list would
        // otherwise see a stale "bad" that clears only on the next event.
        let good_reputation = scoring.refresh(now);
        ScoringInformation {
            id: id.to_string(),
            kind,
            counters: EventType::ALL
                .iter()
                .map(|e| (e.report_name(), scoring.counter(*e)))
                .collect(),
            score: scoring.score(),
            punishments: scoring.punishment_counter(),
            good_reputation,
            punished_until_ms: scoring
                .punished_until(now)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
        }
    }
}

// ---------------------------------------------------------------------------
// The shared service
// ---------------------------------------------------------------------------

/// The manager, shareable, with its ban list persisted to disk.
///
/// rskj has no equivalent: its bans come from `peer.bannedPeerIPs` in the
/// config file and last only as long as the process, so a ban issued through
/// `sco_banAddress` is gone after a restart. That is a gap rather than a
/// behaviour to reproduce -- an operator who banned an abusive range at 3am
/// did not mean "until the next deploy" -- so bans made at runtime are
/// appended to a file and reloaded at start-up.
///
/// The file is plain text, one entry per line, `#` for comments, in exactly
/// the syntax `sco_banAddress` accepts. That makes it editable by hand and
/// diffable, which a binary format would not be.
pub struct ScoringService {
    inner: std::sync::Mutex<PeerScoringManager>,
    ban_file: Option<std::path::PathBuf>,
    /// Node id to remote address for currently-connected peers.
    ///
    /// rskj's callers pass both because they hold a `Peer` object carrying
    /// each. Here most callers -- the sync service, the transaction relay --
    /// hold only a node id, and the address lives behind an `async` lock in
    /// `PeerStore`. Keeping a synchronous copy here means every caller can
    /// record against **both** keys, which is the whole point of keying
    /// twice: a peer that reconnects with a fresh node id would otherwise
    /// shed its history.
    addresses: std::sync::Mutex<HashMap<B512, IpAddr>>,
}

/// The file bans are persisted to, inside the data directory.
pub const BAN_FILE_NAME: &str = "banned-peers.txt";

impl ScoringService {
    /// Load from `data_dir/banned-peers.txt`, plus any statically configured
    /// bans, and keep writing there.
    pub fn open(
        data_dir: &std::path::Path,
        configured_bans: &[String],
        node_capacity: usize,
        punishment_enabled: bool,
    ) -> Self {
        let ban_file = data_dir.join(BAN_FILE_NAME);
        let mut manager = PeerScoringManager::new(node_capacity, punishment_enabled);

        for entry in configured_bans {
            if let Err(why) = manager.ban(entry) {
                tracing::warn!(
                    target: "rustock::scoring",
                    "Ignoring configured ban {entry:?}: {why}"
                );
            }
        }

        let mut loaded = 0usize;
        if let Ok(text) = std::fs::read_to_string(&ban_file) {
            for line in text.lines() {
                let line = line.split('#').next().unwrap_or("").trim();
                if line.is_empty() {
                    continue;
                }
                match manager.ban(line) {
                    Ok(()) => loaded += 1,
                    // A file edited by hand can contain anything. One bad line
                    // must not cost the operator the rest of their ban list,
                    // so it is reported and skipped.
                    Err(why) => tracing::warn!(
                        target: "rustock::scoring",
                        "Ignoring line {line:?} in {}: {why}", ban_file.display()
                    ),
                }
            }
        }
        if loaded > 0 {
            tracing::info!(
                target: "rustock::scoring",
                "Loaded {loaded} persisted peer ban(s) from {}", ban_file.display()
            );
        }

        Self {
            inner: std::sync::Mutex::new(manager),
            ban_file: Some(ban_file),
            addresses: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// An in-memory service, for tests and for a node run without a data
    /// directory.
    pub fn in_memory() -> Self {
        Self {
            inner: std::sync::Mutex::new(PeerScoringManager::default()),
            ban_file: None,
            addresses: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Run something against the manager.
    ///
    /// A `std::sync::Mutex` rather than tokio's: every operation here is a few
    /// map lookups with no await inside, and the RPC layer is synchronous.
    /// A poisoned lock cannot happen without a panic while holding it, and the
    /// recovery -- carry on with the state as it was -- is right either way.
    pub fn with<R>(&self, f: impl FnOnce(&mut PeerScoringManager) -> R) -> R {
        let mut guard = match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        f(&mut guard)
    }

    pub fn record(&self, id: Option<B512>, address: Option<IpAddr>, event: EventType) {
        self.with(|m| m.record(id, address, event));
    }

    /// Record against a node id, resolving its address from the connection
    /// map so the event lands on both keys.
    pub fn record_peer(&self, id: B512, event: EventType) {
        let address = self.address_of(id);
        self.record(Some(id), address, event);
    }

    /// Remember a connected peer's address, so `record_peer` can find it.
    pub fn note_address(&self, id: B512, address: IpAddr) {
        self.lock_addresses().insert(id, address);
    }

    /// Forget a disconnected peer's address. The *scoring* is kept -- only
    /// this lookup table is pruned, so the map does not grow with churn.
    pub fn forget_address(&self, id: &B512) {
        self.lock_addresses().remove(id);
    }

    pub fn address_of(&self, id: B512) -> Option<IpAddr> {
        self.lock_addresses().get(&id).copied()
    }

    fn lock_addresses(&self) -> std::sync::MutexGuard<'_, HashMap<B512, IpAddr>> {
        match self.addresses.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// True when this address may connect: not banned, and not mid-punishment.
    pub fn address_is_welcome(&self, address: IpAddr) -> bool {
        self.with(|m| m.address_has_good_reputation(address))
    }

    pub fn node_is_welcome(&self, id: B512) -> bool {
        self.with(|m| m.node_has_good_reputation(id))
    }

    /// Ban and persist. The ban takes effect whether or not the write does:
    /// failing to record it is worth a warning, not a refusal to act.
    pub fn ban(&self, text: &str) -> Result<(), ScoringError> {
        self.with(|m| m.ban(text))?;
        self.persist();
        Ok(())
    }

    pub fn unban(&self, text: &str) -> Result<(), ScoringError> {
        self.with(|m| m.unban(text))?;
        self.persist();
        Ok(())
    }

    fn persist(&self) {
        let Some(path) = &self.ban_file else { return };
        let entries = self.with(|m| m.banned_addresses());
        let mut text = String::from(
            "# Peer bans, reloaded at start-up. One address or CIDR block per line.\n             # Written by sco_banAddress; safe to edit by hand.\n",
        );
        for entry in entries {
            text.push_str(&entry);
            text.push('\n');
        }
        // Write to a temporary file and rename, so a crash mid-write leaves
        // the previous list intact rather than a truncated one.
        let tmp = path.with_extension("txt.tmp");
        if let Err(why) = std::fs::write(&tmp, text).and_then(|()| std::fs::rename(&tmp, path)) {
            tracing::warn!(
                target: "rustock::scoring",
                "Could not persist peer bans to {}: {why}", path.display()
            );
        }
    }
}

impl Default for ScoringService {
    fn default() -> Self {
        Self::in_memory()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn node(b: u8) -> B512 {
        B512::repeat_byte(b)
    }

    // ------------------------------------------------------------- scoring --

    /// The three groups of `updateScoring`, including the one that does
    /// nothing.
    #[test]
    fn the_score_moves_the_way_rskj_moves_it() {
        let mut s = PeerScoring::default();

        // Group three: increments while non-negative.
        for _ in 0..5 {
            s.update(EventType::ValidBlock);
        }
        assert_eq!(s.score(), 5);

        // Group two: recorded, but the score does not move.
        for e in [
            EventType::UnexpectedMessage,
            EventType::FailedHandshake,
            EventType::SuccessfulHandshake,
            EventType::RepeatedMessage,
            EventType::TimeoutMessage,
        ] {
            s.update(e);
        }
        assert_eq!(s.score(), 5, "these five events never move the score in rskj");
        assert_eq!(s.counter(EventType::FailedHandshake), 1, "but they are counted");

        // Group one: a positive score is reset to zero *before* decrementing,
        // so five good blocks buy nothing against one invalid one.
        s.update(EventType::InvalidBlock);
        assert_eq!(s.score(), -1);

        // And a negative score cannot be climbed back out of.
        for _ in 0..20 {
            s.update(EventType::ValidBlock);
        }
        assert_eq!(s.score(), -1, "only the punishment expiring clears a negative score");
    }

    /// Only three counters decide reputation, and rskj's TODO is the reason.
    #[test]
    fn reputation_ignores_twelve_of_the_fifteen_counters() {
        let mut s = PeerScoring::default();
        for e in EventType::ALL {
            if matches!(
                e,
                EventType::InvalidBlock | EventType::InvalidMessage | EventType::InvalidHeader
            ) {
                continue;
            }
            for _ in 0..50 {
                s.update(e);
            }
        }
        assert!(
            s.has_good_score(),
            "fifty of every other event, including timeouts and failed handshakes, \
             is still a good score in rskj"
        );

        s.update(EventType::InvalidHeader);
        assert!(!s.has_good_score(), "one invalid header is not");
    }

    /// The failure nobody notices: an honest peer punished by accident.
    ///
    /// The schedule below is adversarial in shape -- a peer that disconnects
    /// constantly, times out, fails handshakes, repeats messages and sends
    /// unexpected ones -- but never sends anything *invalid*. rskj keeps it,
    /// so this must too: punishing it would exclude peers every rskj node
    /// still talks to.
    #[test]
    fn a_peer_that_is_only_unreliable_is_never_punished() {
        let mut m = PeerScoringManager::default();
        let id = node(1);
        let address = ip("203.0.113.9");
        let now = Instant::now();

        for round in 0..200 {
            for e in [
                EventType::Disconnection,
                EventType::TimeoutMessage,
                EventType::FailedHandshake,
                EventType::RepeatedMessage,
                EventType::UnexpectedMessage,
                EventType::IncompatibleProtocol,
                EventType::UnexpectedGenesis,
            ] {
                m.record_at(Some(id), Some(address), e, now + Duration::from_secs(round));
            }
        }

        assert!(m.node_has_good_reputation_at(id, now), "node punished for unreliability alone");
        assert!(
            m.address_has_good_reputation_at(address, now),
            "address punished for unreliability alone"
        );
    }

    /// Punishment starts on the transition, lengthens on repeat, and clears
    /// the counters when it expires.
    #[test]
    fn punishment_lengthens_and_then_lets_the_peer_back() {
        let params = PunishmentParameters::nodes();
        let mut m = PeerScoringManager::default();
        let id = node(2);
        let t0 = Instant::now();

        m.record_at(Some(id), None, EventType::InvalidBlock, t0);
        assert!(!m.node_has_good_reputation_at(id, t0), "an invalid block costs reputation");

        // First punishment: the base duration (score is -1, so the `* -score`
        // multiplier is 1).
        let first = punishment_duration(&params, 0, -1);
        assert_eq!(first, params.duration);

        // Still punished one tick before the end, good again one tick after.
        assert!(!m.node_has_good_reputation_at(id, t0 + first - Duration::from_secs(1)));
        assert!(m.node_has_good_reputation_at(id, t0 + first + Duration::from_secs(1)));

        // Expiry cleared the counters, so the peer really is back.
        let after = t0 + first + Duration::from_secs(2);
        m.record_at(Some(id), None, EventType::ValidBlock, after);
        assert!(m.node_has_good_reputation_at(id, after));

        // The second punishment is longer, because the punishment counter
        // survived the expiry.
        m.record_at(Some(id), None, EventType::InvalidBlock, after);
        assert!(!m.node_has_good_reputation_at(id, after));
        let second = punishment_duration(&params, 1, -1);
        assert!(second > first, "{second:?} should exceed {first:?}");
        assert_eq!(second, params.duration.mul_f64(1.1), "rskj's +10% per repeat");
        assert!(m.node_has_good_reputation_at(id, after + second + Duration::from_secs(1)));
    }

    /// The first event after a punishment expires is swallowed, because rskj
    /// counts it and then refreshes -- and the refresh clears the counters.
    #[test]
    fn the_first_event_after_an_expiry_is_swallowed_as_in_rskj() {
        let mut m = PeerScoringManager::default();
        let id = node(7);
        let t0 = Instant::now();
        m.record_at(Some(id), None, EventType::InvalidBlock, t0);
        assert!(!m.node_has_good_reputation_at(id, t0));

        let after = t0 + PunishmentParameters::nodes().duration + Duration::from_secs(1);

        // A second invalid block, the instant the punishment has expired.
        m.record_at(Some(id), None, EventType::InvalidBlock, after);
        assert!(
            m.node_has_good_reputation_at(id, after),
            "rskj counts the event, then the refresh clears it -- no new punishment"
        );

        // The one after it does punish, and for longer than the first.
        m.record_at(Some(id), None, EventType::InvalidBlock, after);
        assert!(!m.node_has_good_reputation_at(id, after));
        let second = punishment_duration(&PunishmentParameters::nodes(), 1, -1);
        assert!(m.node_has_good_reputation_at(id, after + second + Duration::from_secs(1)));
    }

    /// Addresses are capped; node ids are not.
    #[test]
    fn only_the_address_punishment_has_a_ceiling() {
        let addresses = PunishmentParameters::addresses();
        let nodes = PunishmentParameters::nodes();
        let huge = punishment_duration(&addresses, 500, -1);
        assert_eq!(huge, addresses.maximum.unwrap(), "capped at rskj's 6,000 minutes");

        let node_punishment = punishment_duration(&nodes, 500, -1);
        assert!(
            node_punishment > addresses.maximum.unwrap(),
            "a node id has no ceiling, because regenerating one is free"
        );
    }

    /// rskj multiplies the duration by `-score`, but only after the growth
    /// loop -- and the loop returns early at the maximum, so the multiplier
    /// never applies once the cap is reached.
    #[test]
    fn a_worse_score_is_punished_for_longer() {
        let params = PunishmentParameters::nodes();
        assert_eq!(
            punishment_duration(&params, 0, -3),
            params.duration * 3,
            "the score multiplies the first punishment"
        );
        assert_eq!(
            punishment_duration(&params, 0, 5),
            params.duration,
            "a non-negative score does not"
        );
    }

    /// Scoring is keyed twice, so a fresh node id does not shed the address's
    /// history.
    #[test]
    fn a_reconnect_with_a_new_node_id_keeps_the_address_punishment() {
        let mut m = PeerScoringManager::default();
        let address = ip("198.51.100.4");
        let t0 = Instant::now();

        m.record_at(Some(node(3)), Some(address), EventType::InvalidBlock, t0);
        assert!(!m.address_has_good_reputation_at(address, t0));

        // A brand-new node id from the same address: the id is clean...
        let fresh = node(4);
        assert!(m.node_has_good_reputation_at(fresh, t0));
        // ...but the address is not, which is the point of keying twice.
        assert!(!m.address_has_good_reputation_at(address, t0));
    }

    /// The node map is capped, evicting least-recently-touched first.
    #[test]
    fn the_node_table_is_bounded() {
        let mut m = PeerScoringManager::new(3, true);
        let t0 = Instant::now();
        for b in 1..=3u8 {
            m.record_at(Some(node(b)), None, EventType::ValidBlock, t0);
        }
        // Touch the first so the second becomes the eldest.
        m.record_at(Some(node(1)), None, EventType::ValidBlock, t0);
        m.record_at(Some(node(9)), None, EventType::ValidBlock, t0);

        let ids: Vec<String> = m.information_at(t0).iter().map(|i| i.id.clone()).collect();
        assert_eq!(ids.len(), 3, "capacity is three: {ids:?}");
        assert!(!ids.contains(&"02020202".to_string()), "the eldest was evicted: {ids:?}");
        assert!(ids.contains(&"01010101".to_string()), "the touched one survived: {ids:?}");
    }

    // -------------------------------------------------------------- banning --

    #[test]
    fn a_banned_cidr_covers_the_block_and_nothing_else() {
        let mut m = PeerScoringManager::default();
        m.ban("192.168.51.1/16").unwrap();

        assert!(m.is_address_banned(ip("192.168.0.1")));
        assert!(m.is_address_banned(ip("192.168.255.255")));
        assert!(!m.is_address_banned(ip("192.169.0.1")));
        assert!(!m.is_address_banned(ip("10.0.0.1")));
        assert_eq!(m.banned_addresses(), vec!["192.168.51.1/16".to_string()]);

        m.unban("192.168.51.1/16").unwrap();
        assert!(!m.is_address_banned(ip("192.168.0.1")));
        assert!(m.banned_addresses().is_empty());
    }

    /// A prefix that does not land on a byte boundary: rskj's
    /// `(byte)(0xFF00 >> (cidr & 7))` mask.
    #[test]
    fn a_cidr_prefix_inside_a_byte_masks_the_right_bits() {
        let block = CidrBlock::new(ip("10.20.48.0"), 20).unwrap();
        assert!(block.contains(ip("10.20.48.1")));
        assert!(block.contains(ip("10.20.63.255")), "the /20 covers 10.20.48-63");
        assert!(!block.contains(ip("10.20.64.0")), "and stops there");
        assert!(!block.contains(ip("10.21.48.1")));

        // A /32 is a single address.
        let single = CidrBlock::new(ip("10.20.48.7"), 32).unwrap();
        assert!(single.contains(ip("10.20.48.7")));
        assert!(!single.contains(ip("10.20.48.8")));
    }

    #[test]
    fn an_ipv6_block_does_not_match_an_ipv4_address() {
        let block = CidrBlock::new(IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0)), 32)
            .unwrap();
        assert!(block.contains(ip("2001:db8::1")));
        assert!(!block.contains(ip("2001:db9::1")));
        assert!(
            !block.contains(IpAddr::V4(Ipv4Addr::new(32, 1, 13, 184))),
            "different address lengths never match, however the bytes line up"
        );
    }

    /// rskj refuses to ban loopback or wildcard addresses, and rejects `/0`.
    #[test]
    fn the_ban_parser_refuses_what_rskj_refuses() {
        assert_eq!(parse_ban_address("127.0.0.1"), Err(ScoringError::LocalAddress("127.0.0.1".into())));
        assert_eq!(parse_ban_address("0.0.0.0"), Err(ScoringError::LocalAddress("0.0.0.0".into())));
        assert_eq!(parse_ban_address("::1"), Err(ScoringError::LocalAddress("::1".into())));
        assert_eq!(parse_ban_address("  "), Err(ScoringError::EmptyAddress));
        assert!(matches!(parse_ban_address("nope"), Err(ScoringError::UnknownHost(_))));

        assert_eq!(parse_ban_target("10.0.0.1/0"), Err(ScoringError::InvalidMask));
        assert_eq!(parse_ban_target("10.0.0.1/33"), Err(ScoringError::InvalidMask));
        assert_eq!(parse_ban_target("10.0.0.1/x"), Err(ScoringError::InvalidMask));

        // A bare address is not a block.
        assert_eq!(parse_ban_target("10.0.0.1"), Ok(BanTarget::Address(ip("10.0.0.1"))));
    }

    /// A ban outranks a good score, and is not lifted by behaving.
    #[test]
    fn a_ban_is_not_a_punishment() {
        let mut m = PeerScoringManager::default();
        let address = ip("203.0.113.44");
        let t0 = Instant::now();
        m.record_at(None, Some(address), EventType::ValidBlock, t0);
        assert!(m.address_has_good_reputation_at(address, t0));

        m.ban("203.0.113.44").unwrap();
        assert!(!m.address_has_good_reputation_at(address, t0));
        // Years later, and after more good behaviour, still banned.
        let much_later = t0 + Duration::from_secs(60 * 60 * 24 * 365);
        m.record_at(None, Some(address), EventType::ValidBlock, much_later);
        assert!(!m.address_has_good_reputation_at(address, much_later));

        m.unban("203.0.113.44").unwrap();
        assert!(m.address_has_good_reputation_at(address, much_later));
    }

    /// `sco_clearPeerScoring` drops the punishment counter too, so the next
    /// punishment is a first punishment.
    #[test]
    fn clearing_a_peer_resets_its_punishment_history() {
        let mut m = PeerScoringManager::default();
        let address = ip("198.51.100.77");
        let t0 = Instant::now();
        m.record_at(None, Some(address), EventType::InvalidBlock, t0);
        assert!(!m.address_has_good_reputation_at(address, t0));

        assert!(m.clear_address(address));
        assert!(m.address_has_good_reputation_at(address, t0), "a cleared peer starts over");
        assert!(!m.clear_address(address), "clearing twice reports nothing to clear");
    }

    /// The report carries every counter, and asking for it ends an expired
    /// punishment -- rskj calls `refreshReputationAndPunishment()` while
    /// building it.
    #[test]
    fn the_report_refreshes_reputation_as_rskj_does() {
        let mut m = PeerScoringManager::default();
        let address = ip("203.0.113.5");
        let t0 = Instant::now();
        m.record_at(None, Some(address), EventType::InvalidMessage, t0);

        let now = m.information_at(t0);
        assert_eq!(now.len(), 1);
        assert_eq!(now[0].kind, "address");
        assert!(!now[0].good_reputation);
        assert_eq!(now[0].punishments, 1);
        assert!(now[0].punished_until_ms > 0);
        assert_eq!(
            now[0].counters.iter().find(|(n, _)| *n == "invalidMessages").unwrap().1,
            1
        );

        let later = t0 + PunishmentParameters::addresses().duration + Duration::from_secs(1);
        let report = m.information_at(later);
        assert!(report[0].good_reputation, "the report itself ended the punishment");
        assert_eq!(report[0].punished_until_ms, 0);
        assert_eq!(
            report[0].counters.iter().find(|(n, _)| *n == "invalidMessages").unwrap().1,
            0,
            "expiry clears the counters"
        );
        assert_eq!(report[0].punishments, 1, "but not the punishment count");
    }

    // ---------------------------------------------------------- persistence --

    /// Bans survive a restart, which rskj's do not.
    #[test]
    fn bans_are_reloaded_from_disk() {
        let dir = tempfile::tempdir().unwrap();

        {
            let service = ScoringService::open(dir.path(), &[], DEFAULT_NODE_CAPACITY, true);
            service.ban("203.0.113.10").unwrap();
            service.ban("198.51.100.0/24").unwrap();
            assert!(!service.address_is_welcome(ip("198.51.100.200")));
        }

        // A fresh service over the same directory -- as after a restart.
        let reopened = ScoringService::open(dir.path(), &[], DEFAULT_NODE_CAPACITY, true);
        assert!(!reopened.address_is_welcome(ip("203.0.113.10")));
        assert!(!reopened.address_is_welcome(ip("198.51.100.200")));
        assert!(reopened.address_is_welcome(ip("203.0.113.11")));

        // And an unban persists too.
        reopened.unban("198.51.100.0/24").unwrap();
        let again = ScoringService::open(dir.path(), &[], DEFAULT_NODE_CAPACITY, true);
        assert!(again.address_is_welcome(ip("198.51.100.200")));
        assert!(!again.address_is_welcome(ip("203.0.113.10")));
    }

    /// A hand-edited file with a bad line loses that line, not the rest.
    #[test]
    fn a_malformed_ban_line_does_not_discard_the_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(BAN_FILE_NAME),
            "# a comment\n\n203.0.113.10\nnot-an-address\n10.0.0.1/99\n198.51.100.0/24  # trailing\n",
        )
        .unwrap();

        let service = ScoringService::open(dir.path(), &[], DEFAULT_NODE_CAPACITY, true);
        assert!(!service.address_is_welcome(ip("203.0.113.10")));
        assert!(!service.address_is_welcome(ip("198.51.100.7")), "trailing comment is stripped");
        assert!(service.address_is_welcome(ip("10.0.0.1")), "the /99 line was skipped");
    }

    /// Bans from configuration are applied alongside the persisted ones.
    #[test]
    fn configured_bans_are_applied_too() {
        let dir = tempfile::tempdir().unwrap();
        let service = ScoringService::open(
            dir.path(),
            &["203.0.113.0/24".to_string(), "garbage".to_string()],
            DEFAULT_NODE_CAPACITY,
            true,
        );
        assert!(!service.address_is_welcome(ip("203.0.113.5")));
        assert!(service.address_is_welcome(ip("203.0.114.5")));
    }

    /// Punishment can be turned off while leaving the counters observable.
    #[test]
    fn punishment_can_be_disabled_without_losing_the_counters() {
        let mut m = PeerScoringManager::new(DEFAULT_NODE_CAPACITY, false);
        let address = ip("203.0.113.6");
        let t0 = Instant::now();
        for _ in 0..10 {
            m.record_at(None, Some(address), EventType::InvalidBlock, t0);
        }
        assert!(m.address_has_good_reputation_at(address, t0), "punishment disabled");
        let report = m.information_at(t0);
        assert_eq!(
            report[0].counters.iter().find(|(n, _)| *n == "invalidBlocks").unwrap().1,
            10,
            "still counted, so an operator can see what would have happened"
        );
    }
}
