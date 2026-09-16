//! Diff two unitrie roots that live in two different stores.
//!
//! `diff_self` diffs two roots rustock computed in one store; this diffs an
//! arbitrary pair, which is what you need when one side is the chain's real
//! post-state (from an archival store) and the other is what rustock computed
//! and wrote elsewhere. Nodes are content-addressed, so a union view over both
//! stores resolves either side's nodes by hash.
//!
//! Hash-pruned dual walk: descends only where subtree hashes differ, so it
//! visits a handful of nodes on a multi-million-leaf state.
//!
//! Usage: diff_roots <store-a> <root-a> <store-b> <root-b> [attribute-address]

use rustock_storage::RocksDbTrieStore;
use rustock_trie::{NodeRef, TrieNode, TrieStore};
use std::sync::Arc;

type Leaf = (Vec<u8>, Option<Vec<u8>>);

/// Reads from `a`, falling back to `b`. Safe because trie keys are hashes of
/// the node contents: whichever store answers, the answer is the same node.
struct Union {
    a: Arc<dyn TrieStore>,
    b: Arc<dyn TrieStore>,
}
impl TrieStore for Union {
    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.a.get(key).or_else(|| self.b.get(key))
    }
    fn put(&self, _k: &[u8], _v: &[u8]) {}
}

fn pack(bits: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; bits.len().div_ceil(8)];
    for (k, &bit) in bits.iter().enumerate() {
        if bit != 0 { out[k / 8] |= 0x80 >> (k % 8); }
    }
    out
}
fn leaf_of(node: &TrieNode, path: &[u8]) -> Option<Leaf> {
    node.value_hash.as_ref().map(|_| (pack(path), node.value.clone()))
}
fn collect(node: &TrieNode, prefix: &mut Vec<u8>, s: &dyn TrieStore, out: &mut Vec<Leaf>) {
    let start = prefix.len();
    prefix.extend_from_slice(node.shared_path.expand());
    if let Some(l) = leaf_of(node, prefix) { out.push(l); }
    if let Some(left) = node.left.resolve(s) { prefix.push(0); collect(&left, prefix, s, out); prefix.pop(); }
    if let Some(right) = node.right.resolve(s) { prefix.push(1); collect(&right, prefix, s, out); prefix.pop(); }
    prefix.truncate(start);
}
#[allow(clippy::too_many_arguments)]
fn diff(a: &NodeRef, b: &NodeRef, ap: &mut Vec<u8>, bp: &mut Vec<u8>, s: &dyn TrieStore,
        ao: &mut Vec<Leaf>, bo: &mut Vec<Leaf>) {
    if a.get_hash(s) == b.get_hash(s) { return; }
    match (a.resolve(s), b.resolve(s)) {
        (None, Some(n)) => collect(&n, bp, s, bo),
        (Some(n), None) => collect(&n, ap, s, ao),
        (Some(an), Some(bn)) => {
            let (astart, bstart) = (ap.len(), bp.len());
            ap.extend_from_slice(an.shared_path.expand());
            bp.extend_from_slice(bn.shared_path.expand());
            if let Some(l) = leaf_of(&an, ap) { ao.push(l); }
            if let Some(l) = leaf_of(&bn, bp) { bo.push(l); }
            ap.push(0); bp.push(0);
            diff(&an.left, &bn.left, ap, bp, s, ao, bo);
            ap.pop(); bp.pop();
            ap.push(1); bp.push(1);
            diff(&an.right, &bn.right, ap, bp, s, ao, bo);
            ap.truncate(astart); bp.truncate(bstart);
        }
        (None, None) => unreachable!("hashes differ but both empty"),
    }
}

fn describe(key: &[u8], acct_prefix: &[u8], label: &str) -> String {
    if !acct_prefix.is_empty() && key.starts_with(acct_prefix) {
        let rest = &key[acct_prefix.len()..];
        return match rest.first() {
            None => format!("{label} ACCOUNT"),
            Some(0x80) => format!("{label} CODE"),
            Some(0x00) => {
                // storage: 0x00 || keccak(slot)[0..10] || strip_leading_zeros(slot)
                let tail = &rest[1..];
                if tail.len() > 10 {
                    format!("{label} STORAGE slot-tail 0x{}", hex::encode(&tail[10..]))
                } else {
                    format!("{label} STORAGE (slot hashed only)")
                }
            }
            Some(p) => format!("{label} prefix 0x{p:02x} rest 0x{}", hex::encode(rest)),
        };
    }
    format!("0x{}", hex::encode(key))
}

fn main() -> anyhow::Result<()> {
    let a: Vec<String> = std::env::args().skip(1).collect();
    if a.len() < 4 {
        eprintln!("usage: diff_roots <store-a> <root-a> <store-b> <root-b> [attribute-address]");
        std::process::exit(2);
    }
    let sa: Arc<dyn TrieStore> = Arc::new(RocksDbTrieStore::open_read_only(&a[0])?);
    let sb: Arc<dyn TrieStore> = Arc::new(RocksDbTrieStore::open_read_only(&a[2])?);
    let ra = hex::decode(a[1].trim_start_matches("0x"))?;
    let rb = hex::decode(a[3].trim_start_matches("0x"))?;
    let (acct_prefix, label) = if let Some(addr) = a.get(4) {
        let bytes = hex::decode(addr.trim_start_matches("0x"))?;
        (rustock_trie::account_key_from_bytes(&bytes), format!("[{addr}]"))
    } else { (Vec::new(), String::new()) };

    let u = Arc::new(Union { a: sa.clone(), b: sb.clone() });
    let us: &dyn TrieStore = u.as_ref();
    let na = TrieNode::from_message(&us.get(&ra).ok_or_else(|| anyhow::anyhow!("root A not found"))?, us);
    let nb = TrieNode::from_message(&us.get(&rb).ok_or_else(|| anyhow::anyhow!("root B not found"))?, us);

    let (mut ao, mut bo) = (Vec::new(), Vec::new());
    let (mut ap, mut bp) = (Vec::new(), Vec::new());
    diff(&NodeRef::Node(Box::new(na)), &NodeRef::Node(Box::new(nb)), &mut ap, &mut bp, us, &mut ao, &mut bo);

    println!("A ({}): {} differing leaves", a[1], ao.len());
    for (k, v) in &ao {
        println!("  {}\n     = {}", describe(k, &acct_prefix, &label),
                 v.as_ref().map(|x| format!("0x{}", hex::encode(x))).unwrap_or("<none>".into()));
    }
    println!("\nB ({}): {} differing leaves", a[3], bo.len());
    for (k, v) in &bo {
        println!("  {}\n     = {}", describe(k, &acct_prefix, &label),
                 v.as_ref().map(|x| format!("0x{}", hex::encode(x))).unwrap_or("<none>".into()));
    }
    Ok(())
}
