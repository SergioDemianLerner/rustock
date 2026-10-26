//! `--import-blocks` against blocks rskj mined.
//!
//! `fixtures/rskj-regtest-ecdsa-1-15.txt.gz` is blocks 1 to 15 of a chain rskj's
//! regtest mined from `fixtures/rskj-regtest-genesis.json`, as rskj's
//! `ExportBlocks` writes them: version-2 headers, twenty contract calls a
//! block, a flush at block 10 and REMASC's first payout at block 15, which
//! pays rskj's regtest federation.

use std::path::Path;
use std::process::Command;

const DATA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

fn import(blocks: &Path, dir: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_rustock-cli"))
        .args(["--network-id", "33", "--no-rpc", "--log-to-stdout"])
        .arg("--data-dir")
        .arg(dir.join("db"))
        .args(["--trie-backend", "external", "--trie-dir"])
        .arg(dir.join("trie"))
        .arg("--genesis")
        .arg(Path::new(DATA).join("rskj-regtest-genesis.json"))
        .args(["--skip-pow-validation", "--consensus-rule", "rskip97=0"])
        .args([
            "--min-gas-limit",
            "3000000",
            "--max-gas-limit",
            "18446744073709551615",
        ])
        .arg("--import-blocks")
        .arg(blocks)
        .output()
        .expect("rustock-cli runs")
}

#[test]
fn imports_rskj_regtest_blocks() {
    let dir = tempfile::tempdir().unwrap();
    let out = import(
        &Path::new(DATA).join("rskj-regtest-ecdsa-1-15.txt.gz"),
        dir.path(),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "import failed:\n{stdout}\n{stderr}");
    // The best block is rskj's block 15.
    assert!(stdout.contains("Imported 15 blocks"), "{stdout}");
    assert!(
        stdout.contains(
            "best block #15 0xc06f0fa7bd7aae0c0c3066bf2c5b8e5fb8eea326238a67f5bbdf338221cae448"
        ),
        "{stdout}"
    );
}

#[test]
fn a_block_that_does_not_connect_stops_the_import() {
    use std::io::{BufRead, Write};
    let dir = tempfile::tempdir().unwrap();
    // Block 2 onwards: block 2's parent is not genesis.
    let file = std::fs::File::open(Path::new(DATA).join("rskj-regtest-ecdsa-1-15.txt.gz")).unwrap();
    let lines: Vec<String> = std::io::BufReader::new(flate2::read::GzDecoder::new(file))
        .lines()
        .map(Result::unwrap)
        .collect();
    let path = dir.path().join("from-2.txt");
    let mut out = std::fs::File::create(&path).unwrap();
    for line in &lines[1..3] {
        writeln!(out, "{line}").unwrap();
    }
    drop(out);

    let out = import(&path, dir.path());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(
        stderr.contains("import failed at block 2 hash "),
        "{stderr}"
    );
    assert!(stderr.contains("check parent"), "{stderr}");
}
