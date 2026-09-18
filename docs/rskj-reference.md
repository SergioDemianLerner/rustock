# The rskj reference revision

rustock reproduces rskj's behaviour, including its bugs, so the code and
documentation cite rskj constantly — 112 `File.java:NNN` references across
`docs/` and `crates/` at the time of writing. A line number is only meaningful
against a specific revision. This is that revision.

## Pinned revision

```
commit  3dc9d977228d938385f0ee7e025162da5c220f1b
date    2026-09-09 15:47:45 +0200
subject Merge pull request #3676 from rsksmart/is/docs-import-sync
tree    3e5a15ccad1fd419106cc3446a40b013fb76ec36
repo    https://github.com/rsksmart/rskj
```

To obtain exactly what the citations refer to:

```sh
git clone https://github.com/rsksmart/rskj
cd rskj && git checkout 3dc9d977228d938385f0ee7e025162da5c220f1b
```

## The convention

Any `SomeFile.java:NNN` in this repository — in prose, in a code comment, or in
a commit message — means **that line in the pinned revision above**, under
`rskj-core/src/main/java/`. Paths are given relative to that root, or by class
name where it is unambiguous.

Citations without a line number (`Bridge.validateLocalCall`,
`BridgeSupport.registerBtcTransaction`) are revision-independent and remain
correct as long as the method exists.

## Why this matters more than it looks

The citations are the evidence for consensus decisions. When
`crates/execution/src/bridge/mod.rs` says a local-only getter must throw
because of `Bridge.java:431`, that line is the argument. A reader who checks it
against a newer rskj may land in the middle of an unrelated method and conclude
the comment is wrong, when it is the revision that moved.

Rustock's own history shows how sharp this is: three of the defects found in
September 2026 — the DER signature check, the `registerBtcTransaction`
throw/swallow split, and the missing local-call flag — were each settled by
reading a specific rskj line and matching it exactly.

## Verifying the pin

Two spot checks that should hold at the pinned revision:

```sh
sed -n '431p' rskj-core/src/main/java/co/rsk/peg/Bridge.java
#   private void validateLocalCall(BridgeParsedData bridgeParsedData) ...

sed -n '446p' rskj-core/src/main/java/co/rsk/peg/Bridge.java
#   private void validateCallMessageType(BridgeParsedData bridgeParsedData) ...
```

If those do not match, the checkout is not the pinned revision and every line
number in this repository should be treated as approximate.

## Re-pinning

Moving to a newer rskj is a deliberate act, not a side effect of running
`git pull` in a scratch checkout. When it happens:

1. Update the revision block above, with date and subject.
2. Re-verify the spot checks, and expect most line numbers to have shifted.
3. Treat existing citations as referring to the OLD revision until each is
   re-checked. Do not silently renumber: a citation that moved may also have
   changed meaning, which is the thing worth noticing.
4. Note in the commit which behaviours were re-verified against the new
   revision, rather than implying all of them were.

Because of point 3, re-pinning is best done when there is a reason — a new
hard fork, or a behaviour under investigation — rather than routinely.

## Note on local checkouts

A clone used for this purpose should be left unmodified and unfetched, so it
keeps matching the pin. Do not assume any particular path on any particular
machine holds it: verify with the spot checks above before trusting line
numbers, and re-clone at the pinned commit if in doubt.
