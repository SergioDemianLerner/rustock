//! Addressing a single Unitrie node by its path, and describing what is there.
//!
//! # Why paths rather than hashes
//!
//! Every key in the trie store is a hash, so a hash is the natural handle for a
//! node -- but a useless one for a human: it says nothing about where the node
//! sits or what it holds, and you cannot arrive at it by reasoning. A path is
//! the opposite. It is the sequence of bits from the root, which is exactly the
//! key material, so a path both locates the node and says what it is about.
//!
//! # Path notation
//!
//! `<hex>/<bits>` -- hex digits holding the path bits most-significant first,
//! padded with zeros to a byte boundary, followed by how many of those bits are
//! part of the path. The suffix is what makes it unambiguous: the trie is
//! binary, so paths are bit strings of any length, and `0f` alone cannot say
//! whether it means four bits or eight.
//!
//! Written without the suffix, `<hex>` means all `4 x len(hex)` bits, so `a3f`
//! is twelve bits. Both forms parse; `format_path` always emits the explicit
//! one so that output can be fed straight back in.

use alloy_primitives::B256;
use anyhow::{bail, Context, Result};
use rustock_trie::{NodeRef, TrieKeySlice, TrieNode, TrieStore};

/// Bytes of a value printed before truncating.
pub const VALUE_PREVIEW_BYTES: usize = 80;

/// Renders a bit path as `<hex>/<bits>`.
pub fn format_path(bits: &[u8]) -> String {
    let mut bytes = vec![0u8; bits.len().div_ceil(8)];
    for (i, &b) in bits.iter().enumerate() {
        if b & 1 == 1 {
            bytes[i / 8] |= 0x80 >> (i % 8);
        }
    }
    format!("{}/{}", hex_encode(&bytes), bits.len())
}

/// Parses `<hex>/<bits>` or bare `<hex>` into one byte per bit.
pub fn parse_path(s: &str) -> Result<Vec<u8>> {
    let s = s.trim();
    let (hex, declared) = match s.split_once('/') {
        Some((h, n)) => (
            h,
            Some(n.trim().parse::<usize>().context("path bit length is not a number")?),
        ),
        None => (s, None),
    };
    let hex = hex.trim().trim_start_matches("0x");
    if hex.is_empty() {
        return Ok(Vec::new());
    }
    if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("path \"{hex}\" is not hex");
    }
    let bits_available = hex.len() * 4;
    let want = declared.unwrap_or(bits_available);
    if want > bits_available {
        bail!("path declares {want} bits but only {bits_available} are given");
    }
    let mut out = Vec::with_capacity(want);
    for (i, c) in hex.chars().enumerate() {
        let nib = c.to_digit(16).unwrap() as u8;
        for k in 0..4 {
            if out.len() == want {
                break;
            }
            let _ = i;
            out.push((nib >> (3 - k)) & 1);
        }
    }
    Ok(out)
}

pub fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A node found at a path, with how it is stored.
pub struct NodeAt {
    pub node: TrieNode,
    pub hash: B256,
    /// The path actually reached, which equals the requested one on success.
    pub path: Vec<u8>,
    /// Steps from the root, counting the root as 1.
    pub depth: usize,
    /// True when the node lives inside its parent's message rather than as its
    /// own entry in the store.
    pub embedded: bool,
}

/// Walks from `root_hash` to the node whose full path is `target`.
///
/// A node's path is every shared-path bit and every branch bit from the root
/// down to and including the node's own shared path -- the same accumulation
/// the statistics scan performs, so a path printed by one is accepted by the
/// other.
///
/// Returns `Ok(None)` when the path leads nowhere: it may run off an empty
/// child, or diverge inside a shared path, or stop in the middle of one. That
/// last case is worth stating plainly -- a shared path is a run of bits the
/// trie stores compressed in a single node, so **no node exists at a path that
/// ends partway through one**. It is not an error, it is a path that addresses
/// nothing.
pub fn find_by_path(
    store: &dyn TrieStore,
    root_hash: B256,
    target: &[u8],
) -> Result<Option<NodeAt>> {
    let data = store
        .get(root_hash.as_slice())
        .with_context(|| format!("state root {root_hash:?} not found"))?;
    let mut node = TrieNode::from_message(&data, store);
    let mut hash = root_hash;
    let mut embedded = false;
    let mut acc: Vec<u8> = Vec::new();
    let mut depth = 1usize;

    // An empty path means "the root", which is worth answering even though the
    // root's own path is its shared path and so is usually not empty. Both
    // spellings reach it: the empty path by this shortcut, and the root's
    // shared path by the walk below.
    if target.is_empty() {
        let sp: &TrieKeySlice = &node.shared_path;
        let path = (0..sp.length()).map(|i| sp.get(i)).collect();
        return Ok(Some(NodeAt { node, hash, path, depth, embedded }));
    }

    loop {
        let sp: &TrieKeySlice = &node.shared_path;
        for i in 0..sp.length() {
            let bit = sp.get(i);
            if acc.len() == target.len() {
                // Target ends inside this node's shared path.
                return Ok(None);
            }
            if target[acc.len()] != bit {
                return Ok(None); // diverged
            }
            acc.push(bit);
        }

        if acc.len() == target.len() {
            return Ok(Some(NodeAt { node, hash, path: acc, depth, embedded }));
        }

        let bit = target[acc.len()];
        acc.push(bit);
        // Read what the chosen child is before reassigning `node`, so the
        // borrow of the current node ends here rather than spanning the move.
        enum Step {
            Empty,
            Embedded(TrieNode),
            Stored(B256),
        }
        let step = match if bit == 0 { &node.left } else { &node.right } {
            NodeRef::Empty => Step::Empty,
            NodeRef::Node(n) => Step::Embedded((**n).clone()),
            NodeRef::Hash(h) => Step::Stored(*h),
        };
        match step {
            Step::Empty => return Ok(None),
            Step::Embedded(child) => {
                hash = child.compute_hash(store);
                node = child;
                embedded = true;
            }
            Step::Stored(h) => {
                let d = match store.get(h.as_slice()) {
                    Some(d) => d,
                    None => return Ok(None), // dangling reference
                };
                node = TrieNode::from_message(&d, store);
                hash = h;
                embedded = false;
            }
        }
        depth += 1;
    }
}

/// Renders every field of a node, plus what its key says it holds.
pub fn describe(found: &NodeAt, store: &dyn TrieStore) -> String {
    let n = &found.node;
    let mut out = String::new();
    let mut line = |s: String| {
        out.push_str(&s);
        out.push('\n');
    };

    line(format!("path            {}", format_path(&found.path)));
    line(format!("hash            {:?}", found.hash));
    line(format!(
        "stored as       {}",
        if found.embedded {
            "embedded in parent (no entry of its own)"
        } else {
            "its own entry in the store"
        }
    ));
    line(format!("depth           {} (root = 1)", found.depth));
    line(format!(
        "kind            {}",
        if n.is_terminal() { "terminal (leaf)" } else { "branch" }
    ));

    let sp: &TrieKeySlice = &n.shared_path;
    let sp_bits: Vec<u8> = (0..sp.length()).map(|i| sp.get(i)).collect();
    line(format!(
        "shared_path     {} ({} bits)",
        if sp_bits.is_empty() { "-".to_string() } else { format_path(&sp_bits) },
        sp.length()
    ));

    line(format!("children_size   {}", n.children_size));
    line(format!("message_length  {}", n.message_length(store)));
    line(format!("left            {}", describe_ref(&n.left)));
    line(format!("right           {}", describe_ref(&n.right)));

    // What the key says this is. Only meaningful for a terminal node whose path
    // is a whole key: a branch sits mid-key and classifies as nothing.
    let type_line = if n.is_terminal() {
        match pack_path(&found.path) {
            Some(key) => format!(
                "{} (key {} bytes: {})",
                crate::trie_stats::classify(&key).label(),
                key.len(),
                hex_encode(&key)
            ),
            None => "unknown (path is not byte-aligned, so it is not a whole key)".into(),
        }
    } else {
        "n/a (branch nodes sit inside a key, not at the end of one)".into()
    };
    line(format!("type            {type_line}"));

    match &n.value {
        None => line("value           none".to_string()),
        Some(v) => {
            line(format!("value_length    {}", v.len()));
            line(format!(
                "value_storage   {}",
                if n.has_long_value() {
                    "separate entry, keyed by value hash (over 32 bytes)"
                } else {
                    "inline in the node"
                }
            ));
            if let Some(vh) = n.value_hash {
                line(format!("value_hash      {vh:?}"));
            }
            line(format!("value           {}", preview(v)));
        }
    }
    out
}

fn describe_ref(r: &NodeRef) -> String {
    match r {
        NodeRef::Empty => "empty".into(),
        NodeRef::Hash(h) => format!("{h:?}"),
        NodeRef::Node(_) => "embedded node".into(),
    }
}

/// Hex of the first `VALUE_PREVIEW_BYTES`, with `....` when more follows.
pub fn preview(v: &[u8]) -> String {
    if v.len() <= VALUE_PREVIEW_BYTES {
        hex_encode(v)
    } else {
        format!("{}....", hex_encode(&v[..VALUE_PREVIEW_BYTES]))
    }
}

/// Packs a bit path into bytes, or `None` when it is not byte-aligned.
pub fn pack_path(bits: &[u8]) -> Option<Vec<u8>> {
    if bits.len() % 8 != 0 {
        return None;
    }
    Some(
        bits.chunks(8)
            .map(|c| c.iter().fold(0u8, |acc, &b| (acc << 1) | (b & 1)))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_round_trips_at_every_length() {
        for bits in [
            vec![],
            vec![1],
            vec![0, 1, 0],
            vec![1, 1, 1, 1, 0, 0, 0, 0],
            vec![1, 0, 1, 1, 0, 0, 1, 0, 1],
        ] {
            let s = format_path(&bits);
            assert_eq!(parse_path(&s).unwrap(), bits, "round trip of {s}");
        }
    }

    #[test]
    fn bare_hex_means_four_bits_per_digit() {
        assert_eq!(parse_path("a").unwrap(), vec![1, 0, 1, 0]);
        assert_eq!(parse_path("0xa").unwrap(), vec![1, 0, 1, 0]);
        assert_eq!(parse_path("a3").unwrap().len(), 8);
    }

    #[test]
    fn explicit_bit_length_truncates() {
        assert_eq!(parse_path("ff/3").unwrap(), vec![1, 1, 1]);
        assert_eq!(parse_path("80/1").unwrap(), vec![1]);
    }

    #[test]
    fn rejects_more_bits_than_supplied() {
        assert!(parse_path("f/8").is_err());
        assert!(parse_path("zz").is_err());
    }

    #[test]
    fn preview_truncates_with_a_marker() {
        let short = vec![0xAB; 4];
        assert_eq!(preview(&short), "abababab");
        let long = vec![0xCD; VALUE_PREVIEW_BYTES + 1];
        let p = preview(&long);
        assert!(p.ends_with("...."), "{p}");
        assert_eq!(p.len(), VALUE_PREVIEW_BYTES * 2 + 4);
    }

    #[test]
    fn exactly_the_preview_limit_is_not_truncated() {
        let v = vec![0x11; VALUE_PREVIEW_BYTES];
        assert!(!preview(&v).ends_with("...."));
    }
}
