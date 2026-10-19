//! Generate a block workload with a known collision structure, mine it, and
//! report how the RSKIP-144 miner partitioned it.
//!
//! The question this answers: given transactions whose dependencies we chose
//! on purpose, does the miner split them the way we expect, and is the block
//! it produces one a node would accept?
//!
//! Transfers go through a token contract rather than moving rBTC, because a
//! token transfer's collisions are **storage** collisions and those are the
//! ones we can place freely. A native transfer touches two account cells and
//! nothing else; a token transfer touches two cells of our choosing, so the
//! transfer graph is the collision graph and we can draw it.
//!
//! Everything is deterministic from `--seed`. That is the point rather than
//! tidiness: the same workload can be handed to another implementation and
//! the partitions compared, and transactions regenerated rather than
//! replayed would make a difference in the input indistinguishable from a
//! difference in the decision. `--dump` writes the signed transactions for
//! exactly that.
//!
//! ```text
//! parallel_workload [scenario] [options]
//!
//!   scenario           independent | chain | one-sender | hot-recipient
//!                      | with-bridge-call | gas-pressure | all   (default: all)
//!   --count N          transfers to generate (default 4)
//!   --seed N           the workload is a function of this (default 1)
//!   --gas-limit N      block gas limit; below the sequential minimum
//!                      nothing can be parallel at all
//!   --dump DIR         write, per scenario, DIR/<scenario>.txs -- the
//!                      signed transactions as hex RLP, one per line -- and
//!                      DIR/<scenario>.world, the accounts, keys, token code
//!                      and balances they run against, plus what this miner
//!                      decided. The transactions alone are not the
//!                      experiment: without the world, another
//!                      implementation answers a different question.
//! ```
//!
//! Read-only: everything runs in a temporary directory and an in-memory trie.

use anyhow::{Context, Result};
use rustock_execution::mining::workload::{Scenario, Workload, WorkloadReport};

fn main() -> Result<()> {
    let mut scenario: Option<Scenario> = None;
    let mut count = 4usize;
    let mut seed = 1u64;
    let mut gas_limit: Option<u64> = None;
    let mut dump: Option<String> = None;
    let mut all = true;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--count" => count = args.next().context("--count needs a value")?.parse()?,
            "--seed" => seed = args.next().context("--seed needs a value")?.parse()?,
            "--gas-limit" => {
                gas_limit = Some(args.next().context("--gas-limit needs a value")?.parse()?)
            }
            "--dump" => dump = Some(args.next().context("--dump needs a directory")?),
            "all" => all = true,
            other if other.starts_with("--") => {
                anyhow::bail!("unknown option {other}");
            }
            name => {
                scenario = Some(
                    Scenario::parse(name)
                        .with_context(|| format!("unknown scenario {name:?}"))?,
                );
                all = false;
            }
        }
    }

    let scenarios: Vec<Scenario> = match scenario {
        Some(one) => vec![one],
        None if all => Scenario::all().to_vec(),
        None => vec![Scenario::Independent],
    };

    if let Some(dir) = &dump {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating the dump directory {dir}"))?;
    }

    println!("seed {seed}, {count} transfer(s) per scenario\n");

    let mut failures = 0usize;
    for scenario in scenarios {
        let workdir = tempfile::tempdir().context("a temporary directory")?;
        let mut workload = Workload { scenario, seed, count, ..Default::default() };
        if let Some(limit) = gas_limit {
            workload.block_gas_limit = limit;
        }

        match workload.run(workdir.path()) {
            Ok(report) => {
                report_on(&report);
                if report.validated.is_err() {
                    failures += 1;
                }
                if let Some(dir) = &dump {
                    let txs = format!("{dir}/{}.txs", scenario.name());
                    let body: String = report
                        .submitted
                        .iter()
                        .map(|tx| format!("{}\n", alloy_primitives::hex::encode(tx.rlp_for_trie())))
                        .collect();
                    std::fs::write(&txs, body).with_context(|| format!("writing {txs}"))?;

                    // The transactions alone are not the experiment: without
                    // the world they run against, another implementation is
                    // answering a different question.
                    let world = format!("{dir}/{}.world", scenario.name());
                    std::fs::write(&world, report.describe_world())
                        .with_context(|| format!("writing {world}"))?;
                    println!("    dumped to {txs} and {world}");
                }
            }
            Err(e) => {
                println!("{:<18} could not run: {e}", scenario.name());
                failures += 1;
            }
        }
        println!();
    }

    if failures > 0 {
        anyhow::bail!("{failures} scenario(s) did not produce an acceptable block");
    }
    Ok(())
}

fn report_on(report: &WorkloadReport) {
    let sizes = report.sizes();
    let (parallel, sequential) = sizes.split_at(sizes.len() - 1);

    println!("{}", report.scenario.name());
    println!("    transactions    {}", report.block.transactions.len());
    if report.edges.is_empty() {
        println!("    partition       none -- every transaction is sequential");
    } else {
        println!("    edges           {:?}", report.edges);
        let split: Vec<String> = parallel.iter().map(|n| n.to_string()).collect();
        println!(
            "    parallel        {} ({} transaction(s))",
            split.join(" + "),
            parallel.iter().sum::<usize>()
        );
    }
    println!("    sequential      {}", sequential[0]);

    match &report.validated {
        Ok(()) => println!("    validated       yes"),
        Err(e) => println!("    validated       NO -- {e}"),
    }
}
