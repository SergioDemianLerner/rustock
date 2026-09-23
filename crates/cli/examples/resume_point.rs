//! Where would execution resume? A read-only diagnostic.
//!
//! Applies exactly the rule the node applies — `rollback::choose_resume_point`
//! — against a real database, so the answer can be known **before** starting a
//! node rather than inferred from its logs afterwards.
//!
//! Everything is opened read-only: no lock, no WAL replay, no writes. It
//! cannot damage the database. The absence of WAL replay means the view can be
//! a block or two behind a node that was not cleanly stopped, which is stated
//! in the output rather than hidden.
//!
//! ```text
//! cargo run --release --example resume_point -- \
//!     --data-dir /var/lib/rustock --trie-dir /var/lib/rustock/trie-epochs
//! ```

use alloy_primitives::B256;
use anyhow::{Context, Result};
use rustock_core::Header;
use rustock_storage::epoch_store::{EpochConfig, EpochTrieStore};
use rustock_storage::{BlockRef, BlockStore};
use rustock_sync::rollback::{choose_resume_point, ChainView};
use rustock_trie::TrieStore;

fn arg(name: &str, default: &str) -> String {
    let args: Vec<String> = std::env::args().collect();
    args.windows(2)
        .find(|w| w[0] == format!("--{name}"))
        .map(|w| w[1].clone())
        .unwrap_or_else(|| default.to_string())
}

struct View {
    store: BlockStore,
    trie: EpochTrieStore,
}

impl ChainView for View {
    fn header(&self, hash: B256) -> Option<Header> {
        self.store.header(hash).ok().flatten()
    }
    fn canonical_hash(&self, number: u64) -> Option<B256> {
        self.store.canonical_hash(number).ok().flatten()
    }
    fn has_state(&self, root: B256) -> bool {
        self.trie.get(root.as_slice()).is_some()
    }
}

fn main() -> Result<()> {
    let data_dir = arg("data-dir", "/var/lib/rustock");
    let trie_dir = arg("trie-dir", "/var/lib/rustock/trie-epochs");

    println!("Opening READ-ONLY: {data_dir} and {trie_dir}\n");

    let store = BlockStore::open_read_only(&data_dir)?;
    let cfg = EpochConfig { epochs: 4, burial_depth: 4_000, rotate_bytes: 1 << 30 };
    let trie = EpochTrieStore::open_read_only(&trie_dir, cfg)?;
    let view = View { store, trie };

    let (exec_hash, exec_root) = view
        .store
        .exec_head()?
        .context("no executed head recorded")?;
    let exec_header = view.header(exec_hash).context("executed head header missing")?;
    let head_number = view
        .store
        .head()?
        .and_then(|h| view.header(h))
        .map(|h| h.number);

    println!("executed head   #{} {exec_hash}", exec_header.number);
    println!("  state root    {exec_root}");
    println!("  canonical?    {}", view.canonical_hash(exec_header.number) == Some(exec_hash));
    println!("  state present {}", view.has_state(exec_root));
    println!("download head   {head_number:?}\n");

    let from = BlockRef::new(exec_header.number, exec_hash);
    match choose_resume_point(&view, from) {
        Ok(target) if target == from => {
            println!("HEALTHY: the executed head is canonical and its state is present.");
            println!("No rollback would happen.");
        }
        Ok(target) => {
            println!("ROLLBACK: execution would resume at {target}");
            println!(
                "  {} block(s) re-executed",
                exec_header.number.saturating_sub(target.number)
            );
            println!("\nThe markers are recoverable. The chain data itself is intact:");
            println!("every block between the resume point and the head is still downloaded.");
        }
        Err(why) => {
            println!("UNRECOVERABLE: {why:?}");
            println!("\nNo block on the executed head's ancestry is both canonical and has");
            println!("its state in the trie store. This needs a resync from a state that exists.");
        }
    }

    println!(
        "\n(Read-only mode does not replay the write-ahead log, so this view may be a block\n\
         or two behind a node that was not cleanly stopped.)"
    );
    Ok(())
}
