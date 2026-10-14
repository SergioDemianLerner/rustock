//! Find `registerBtcTransaction` calls by reading blocks, without executing them.
//!
//! The replay in `verify_range --record-silent` answers "what did the Bridge
//! decide" by re-executing every block against rskj's own state. That is
//! authoritative but slow: it resolves each block's pre-state from a 130 GB
//! unitrie. This reads the same calls straight out of the transactions, which
//! costs a sequential pass over the blocks and nothing else.
//!
//! It exists as an independent check on the replay. Two methods that share no
//! code and agree are worth much more than either alone, and where they
//! disagree, the disagreement is itself the finding.
//!
//! ```text
//! scan_pegin_calls <data-dir> <from> <to> [--fed-p2sh ADDR,ADDR] [--json]
//! scan_pegin_calls <data-dir> --blocks FILE   [--fed-p2sh ADDR,ADDR] [--json]
//! ```
//!
//! Reading blocks is far cheaper *per block* than replaying them, but a range
//! scan still has to deserialise every body in it. Over #6,223,964..#8,488,233
//! that is 2.26M blocks and takes hours -- longer than replaying the 33,447
//! blocks that actually contain a rejection. `--blocks` is what makes the
//! static pass quick: given the same list the replay used, it answers in
//! minutes. Narrowing the set is where the speed comes from, not the lack of
//! execution.
//!
//! # What this tool CANNOT see
//!
//! **It only finds calls made as the top-level transaction** -- that is, where
//! the transaction's `to` is the Bridge. `registerBtcTransaction` is
//! `TransactionCallable` and carries no `isContractTx` restriction (unlike
//! `registerFlyoverBtcTransaction` and `releaseBtc`, which do), so **a
//! contract may call it internally**, and such a call is invisible here: it
//! exists only as an internal transaction created during execution, and
//! finding it requires executing the block.
//!
//! This is not a hypothetical gap. Classifying blocks by top-level `to` has
//! already produced a wrong answer on this chain once -- mainnet #7,629,709
//! reaches the Bridge through an internal call and looks like an ordinary
//! contract transaction from the outside.
//!
//! So: a transaction this tool reports is really there, but a peg-in it does
//! not report may still exist. Use it to corroborate a replay, never to
//! replace one.
//!
//! Two smaller limits, for the same reason -- nothing is executed:
//!
//! - It reports calls, not outcomes. Whether a peg-in was accepted, refused,
//!   or refused because the amount was below the minimum depends on state
//!   (the federation of the day, the whitelist, the locking cap) and on
//!   fork-dependent rules. A call appearing here may have been accepted.
//! - A call in a transaction that reverted still appears.

use anyhow::{Context, Result};
use bitcoin::consensus::deserialize;
use bitcoin::Transaction as BtcTransaction;
use rustock_storage::BlockStore;
use std::collections::BTreeMap;

const USAGE: &str =
    "usage: scan_pegin_calls <data-dir> <from> <to> [--fed-p2sh ADDR,ADDR] [--json]";

/// `registerBtcTransaction(bytes,int256,bytes)`.
const SELECTOR: [u8; 4] = [0x43, 0xdc, 0x06, 0x56];
/// The Bridge's address. Taken from the execution crate rather than written
/// out here: a transcribed address that is wrong by one nibble finds nothing
/// and looks exactly like a clean result.
use rustock_execution::precompiles::BRIDGE_ADDR;

/// Reads the first `bytes` argument of an ABI call: a 32-byte offset, then at
/// that offset a 32-byte length followed by the data.
fn abi_first_bytes(args: &[u8]) -> Option<&[u8]> {
    if args.len() < 32 {
        return None;
    }
    let offset = u32::from_be_bytes(args[28..32].try_into().ok()?) as usize;
    let len_at = offset.checked_add(32)?;
    if args.len() < len_at {
        return None;
    }
    let len = u32::from_be_bytes(args[len_at - 4..len_at].try_into().ok()?) as usize;
    args.get(len_at..len_at.checked_add(len)?)
}

/// The shape of a peg-in's first input, as rskj's sender classifier sees it.
/// Every shape it recognises needs a non-empty scriptSig, so an empty one is
/// an unclassifiable sender and the bitcoin cannot be refunded.
fn input_shape(tx: &BtcTransaction) -> &'static str {
    let Some(first) = tx.input.first() else { return "NO_INPUTS" };
    let script_sig = first.script_sig.as_bytes();
    let witness: Vec<&[u8]> = first.witness.iter().collect();
    if script_sig.is_empty() {
        return match witness.len() {
            0 => "EMPTY_SCRIPTSIG_NO_WITNESS",
            2 if witness[1].len() == 33 => "NATIVE_P2WPKH",
            _ => "NATIVE_SEGWIT_OTHER",
        };
    }
    if !witness.is_empty() {
        return "P2SH_WITNESS";
    }
    "P2SH_OR_P2PKH"
}

struct Seen {
    calls: u64,
    first_block: u64,
    last_block: u64,
    shape: &'static str,
    /// Value paid to the federation addresses given, or to every P2SH output
    /// when none were given.
    fed_sat: u64,
    total_out_sat: u64,
}

fn main() -> Result<()> {
    let mut argv = std::env::args().skip(1);
    let data_dir = argv.next().ok_or_else(|| anyhow::anyhow!(USAGE))?;
    let mut fed: Vec<[u8; 20]> = Vec::new();
    let mut json = false;
    let mut rest: Vec<String> = argv.collect();
    // The positional range is absent when a block list is given.
    let blocks_file = rest
        .iter()
        .position(|a| a == "--blocks")
        .map(|i| {
            let v = rest.get(i + 1).cloned().context("--blocks needs a value")?;
            rest.drain(i..=i + 1);
            Ok::<String, anyhow::Error>(v)
        })
        .transpose()?;
    let targets: Vec<u64> = match &blocks_file {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading the block list at {path}"))?;
            let mut v: Vec<u64> = text
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .map(|l| l.parse::<u64>().with_context(|| format!("block number {l:?}")))
                .collect::<Result<_>>()?;
            v.sort_unstable();
            v.dedup();
            anyhow::ensure!(!v.is_empty(), "the block list at {path} is empty");
            v
        }
        None => {
            anyhow::ensure!(rest.len() >= 2, "{USAGE}");
            let from: u64 = rest.remove(0).parse().context("<from>")?;
            let to: u64 = rest.remove(0).parse().context("<to>")?;
            anyhow::ensure!(from <= to, "empty range: {from} > {to}");
            (from..=to).collect()
        }
    };
    let from = *targets.first().expect("non-empty");
    let to = *targets.last().expect("non-empty");
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--json" => json = true,
            "--fed-p2sh" => {
                i += 1;
                let list = rest.get(i).context("--fed-p2sh needs a value")?;
                for a in list.split(',').filter(|s| !s.is_empty()) {
                    let decoded = bitcoin::base58::decode_check(a)
                        .with_context(|| format!("address {a} is not valid base58check"))?;
                    anyhow::ensure!(decoded.len() == 21, "address {a} is not 21 bytes decoded");
                    fed.push(decoded[1..21].try_into().expect("checked above"));
                }
            }
            other => anyhow::bail!("unknown option {other}\n{USAGE}"),
        }
        i += 1;
    }

    eprintln!(
        "scanning {} block(s) in #{from}..=#{to} for registerBtcTransaction calls",
        targets.len()
    );
    eprintln!(
        "NOTE: top-level transactions only. The Bridge permits internal calls to \n\
         this method, and those are invisible without executing the block."
    );
    if fed.is_empty() {
        eprintln!("no --fed-p2sh given: 'fed' value counts every P2SH output");
    }

    let store = BlockStore::open_read_only(&data_dir)
        .with_context(|| format!("opening block store at {data_dir} read-only"))?;

    let mut seen: BTreeMap<String, Seen> = BTreeMap::new();
    let mut calls = 0u64;
    let mut blocks_with_calls = 0u64;
    let started = std::time::Instant::now();

    for (done, n) in targets.iter().copied().enumerate() {
        let Some(hash) = store.canonical_hash(n)? else { continue };
        let Some((transactions, _)) = store.body(hash)? else { continue };
        let mut hit = false;
        for tx in &transactions {
            if tx.to.as_ref() != BRIDGE_ADDR.as_slice() || !tx.input.starts_with(&SELECTOR) {
                continue;
            }
            let Some(raw) = abi_first_bytes(&tx.input[4..]) else { continue };
            let Ok(btc) = deserialize::<BtcTransaction>(raw) else { continue };
            calls += 1;
            hit = true;

            let mut txid = *bitcoin::hashes::Hash::as_byte_array(&btc.compute_txid().to_raw_hash());
            txid.reverse();
            let key = format!("0x{}", alloy_primitives::hex::encode(txid));

            let mut fed_sat = 0u64;
            let mut total = 0u64;
            for o in &btc.output {
                total += o.value.to_sat();
                let spk = o.script_pubkey.to_bytes();
                let is_p2sh = spk.len() == 23 && spk[0] == 0xa9 && spk[1] == 0x14;
                let matches = if fed.is_empty() {
                    is_p2sh
                } else {
                    is_p2sh && fed.iter().any(|f| spk[2..22] == f[..])
                };
                if matches {
                    fed_sat += o.value.to_sat();
                }
            }

            let e = seen.entry(key).or_insert(Seen {
                calls: 0,
                first_block: n,
                last_block: n,
                shape: input_shape(&btc),
                fed_sat,
                total_out_sat: total,
            });
            e.calls += 1;
            e.last_block = n;
        }
        if hit {
            blocks_with_calls += 1;
        }
        if (done + 1) % 5_000 == 0 {
            eprintln!(
                "  #{n}  {calls} calls, {} distinct txs | {}/{} blocks",
                seen.len(),
                done + 1,
                targets.len()
            );
        }
    }

    let sat = |v: u64| format!("{}.{:08}", v / 100_000_000, v % 100_000_000);
    if json {
        for (k, s) in &seen {
            println!(
                "{{\"btcTxHash\":\"{k}\",\"calls\":{},\"firstBlock\":{},\"lastBlock\":{},\"shape\":\"{}\",\"fedSat\":{},\"totalOutSat\":{}}}",
                s.calls, s.first_block, s.last_block, s.shape, s.fed_sat, s.total_out_sat
            );
        }
    } else {
        println!("\n{:>8}  {:>16}  {:<22} {:>10} {}", "calls", "fed value", "first input", "first blk", "btcTxHash");
        let mut rows: Vec<(&String, &Seen)> = seen.iter().collect();
        rows.sort_by(|a, b| b.1.calls.cmp(&a.1.calls));
        for (k, s) in rows {
            println!(
                "{:>8}  {:>16}  {:<22} {:>10} {}",
                s.calls, sat(s.fed_sat), s.shape, s.first_block, k
            );
        }
    }

    eprintln!(
        "\n{calls} call(s) in {blocks_with_calls} block(s); {} distinct bitcoin transaction(s); took {:.0}s",
        seen.len(),
        started.elapsed().as_secs_f64()
    );
    eprintln!("(top-level calls only -- internal Bridge calls are not visible here)");
    Ok(())
}
