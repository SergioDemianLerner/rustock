//! A VM tracer shaped like rskj's, for `debug_traceTransaction`.
//!
//! # Why this is not go-ethereum's tracer
//!
//! geth returns `{gas, failed, returnValue, structLogs}`. rskj returns its
//! `DetailedProgramTrace`: `contractAddress`, `initStorage`, `structLogs`,
//! `result`, `error`, `reverted`, `storageSize`, `currentStorage`. Consumers
//! of an RSK node are written against the second, so that is what is built
//! here. `docs/debug-namespace.md` has the full comparison.
//!
//! Three details inside `structLogs` are rskj's and easy to get wrong:
//!
//! * **`gas` is measured before the opcode runs, `gasCost` after.** rskj calls
//!   `addOp` with the remaining gas, then `saveGasCost` sets the cost on the
//!   entry it just added. Reporting the post-execution gas instead shifts
//!   every row by one opcode.
//! * **Stack and memory are bare hex, not `0x`-prefixed**, and the stack is
//!   listed **bottom first** -- `stack[0]` is the deepest item, not the top.
//!   Memory is split into 32-byte chunks, the last one short if the memory
//!   size is not a multiple of 32.
//! * **`storage` lags by one opcode, on purpose.** rskj notes the key when it
//!   sees SSTORE or SLOAD (`storageKey = stack.peek()`) and reads its value on
//!   the *next* `addOp`, once the opcode has run. So a step's `storage` shows
//!   the effect of the step before it, and the map accumulates every key
//!   touched so far rather than describing the contract's whole storage.
//!
//! # It must not change what executes
//!
//! A tracer that alters execution is a consensus hazard, not a debugging tool.
//! Two things protect that here: the traced path is separate from block
//! processing -- nothing in `BlockProcessor` consults it -- and the storage
//! read below uses `sload_skip_cold_load`, which does not mark the slot warm.
//!
//! **That second one is defensive rather than load-bearing, and the
//! distinction is worth stating.** On Ethereum a tracer calling plain `sload`
//! would warm the slot and change EIP-2929 gas for every access after it. RSK
//! has no EIP-2929 -- `make_cfg_env` zeroes `cold_storage_cost`,
//! `warm_storage_read_cost` and the rest, which
//! `test_cfg_env_no_eip2929_cold_access_cost` pins -- so on this chain that
//! particular mistake would cost nothing. The skip is used anyway because
//! "this side effect happens to be free here" is an assumption a future RSKIP
//! could quietly invalidate, and not depending on it is free.
//!
//! It follows that `tracing_does_not_change_execution` does **not** prove the
//! tracer is side-effect free; nothing in gas could reveal a warmth change on
//! RSK. What it proves is that the traced and untraced paths agree on gas,
//! output, success and logs -- which is what catches the two setups drifting
//! apart, the likelier failure.

use alloy_primitives::{Address, U256};
use revm::bytecode::opcode::OpCode;
use revm::context::ContextTr;
use revm::context_interface::JournalTr;
use revm::inspector::{inspectors::GasInspector, Inspector};
use revm::interpreter::{
    interpreter_types::{InputsTr, Jumps, MemoryTr, StackTr},
    Interpreter, InterpreterTypes,
};
use std::collections::BTreeMap;

/// How many steps to record before giving up on the rest.
///
/// A full trace of a large transaction is big -- tens of megabytes for a
/// contract in a loop -- and a tracer that holds every step will exhaust
/// memory on exactly the transaction someone most wants to look at. rskj caps
/// its trace through `vmTrace` config; this is the same idea with a default
/// rather than a setting.
pub const DEFAULT_MAX_STRUCT_LOGS: usize = 200_000;

/// One entry of rskj's `structLogs`, mirroring `org.ethereum.vm.trace.Op`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructLog {
    /// The opcode's name, as `OpCode.code(op).name()` renders it.
    pub op: String,
    pub pc: u64,
    pub depth: u64,
    /// Gas remaining **before** this opcode ran.
    pub gas: u64,
    /// What this opcode cost.
    pub gas_cost: u64,
    /// Bottom first, 64 hex characters each, no `0x`.
    pub stack: Vec<String>,
    /// 32-byte chunks, hex, no `0x`; the last may be shorter.
    pub memory: Vec<String>,
    /// Keys touched so far by SSTORE/SLOAD, with their values after the
    /// opcode that touched them. Both 64 hex characters, no `0x`.
    pub storage: BTreeMap<String, String>,
}

/// rskj's `DetailedProgramTrace`.
#[derive(Debug, Clone, Default)]
pub struct ProgramTrace {
    pub contract_address: String,
    pub struct_logs: Vec<StructLog>,
    /// The return data, hex, no `0x`.
    pub result: String,
    pub error: String,
    pub reverted: bool,
    pub storage_size: usize,
    pub current_storage: BTreeMap<String, String>,
    /// True when the step cap cut the trace short, so a consumer can tell a
    /// truncated trace from a short one.
    pub truncated: bool,
}

/// Records every opcode of an execution in rskj's shape.
pub struct RskTracer {
    gas_inspector: GasInspector,
    logs: Vec<StructLog>,
    /// The accumulated storage view described in the module docs.
    current_storage: BTreeMap<String, String>,
    /// Set when the previous opcode was SSTORE or SLOAD: its key, whose value
    /// is read on the next step, once that opcode has run.
    pending_storage: Option<(Address, U256)>,
    max_logs: usize,
    truncated: bool,
}

impl Default for RskTracer {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_STRUCT_LOGS)
    }
}

impl RskTracer {
    pub fn new(max_logs: usize) -> Self {
        Self {
            gas_inspector: GasInspector::new(),
            logs: Vec::new(),
            current_storage: BTreeMap::new(),
            pending_storage: None,
            max_logs,
            truncated: false,
        }
    }

    /// The recorded steps and the storage view they ended with.
    pub fn finish(self) -> (Vec<StructLog>, BTreeMap<String, String>, bool) {
        (self.logs, self.current_storage, self.truncated)
    }

    pub fn truncated(&self) -> bool {
        self.truncated
    }
}

/// rskj renders a `DataWord` as 64 hex characters with no prefix
/// (`ByteUtil.toHexString(data)` over the full 32 bytes).
fn word_hex(v: &U256) -> String {
    hex::encode(v.to_be_bytes::<32>())
}

impl<CTX, INTR> Inspector<CTX, INTR> for RskTracer
where
    CTX: ContextTr,
    INTR: InterpreterTypes,
{
    fn step(&mut self, interp: &mut Interpreter<INTR>, context: &mut CTX) {
        self.gas_inspector.step(&interp.gas);

        // rskj resolves the previous SSTORE/SLOAD here, not when it saw it:
        // the value is only meaningful once that opcode has run.
        if let Some((address, key)) = self.pending_storage.take() {
            // `skip_cold_load = true`: do not warm the slot. On RSK that
            // changes no gas -- there is no EIP-2929 here -- so this is
            // defensive, not load-bearing. See the module docs.
            let value = context
                .journal_mut()
                .sload_skip_cold_load(address, key, true)
                .ok()
                .map(|loaded| loaded.data);
            match value {
                Some(v) => {
                    self.current_storage.insert(word_hex(&key), word_hex(&v));
                }
                None => {
                    self.current_storage.remove(&word_hex(&key));
                }
            }
        }

        if self.logs.len() >= self.max_logs {
            self.truncated = true;
            return;
        }

        let opcode = interp.bytecode.opcode();
        let stack: Vec<String> = interp.stack.data().iter().map(word_hex).collect();

        let size = interp.memory.size();
        let mut memory = Vec::with_capacity(size.div_ceil(32));
        let mut at = 0;
        while at < size {
            let end = (at + 32).min(size);
            memory.push(hex::encode(interp.memory.slice(at..end).as_ref()));
            at = end;
        }

        self.logs.push(StructLog {
            op: OpCode::new(opcode).map(|o| o.as_str().to_string()).unwrap_or_else(|| {
                // rskj's OpCode.code() returns null for an unknown byte and the
                // serializer would NPE; naming it keeps the trace readable.
                format!("UNKNOWN(0x{opcode:02x})")
            }),
            pc: interp.bytecode.pc() as u64,
            depth: context.journal_mut().depth() as u64,
            gas: interp.gas.remaining(),
            gas_cost: 0, // filled in by `step_end`
            stack,
            memory,
            storage: self.current_storage.clone(),
        });

        // SSTORE and SLOAD both take the key from the top of the stack.
        if opcode == revm::bytecode::opcode::SSTORE || opcode == revm::bytecode::opcode::SLOAD {
            if let Some(key) = interp.stack.data().last().copied() {
                let address = interp.input.target_address();
                self.pending_storage = Some((address, key));
            }
        }
    }

    fn step_end(&mut self, interp: &mut Interpreter<INTR>, _context: &mut CTX) {
        self.gas_inspector.step_end(&interp.gas);
        if let Some(last) = self.logs.last_mut() {
            last.gas_cost = self.gas_inspector.last_gas_cost();
        }
    }
}
