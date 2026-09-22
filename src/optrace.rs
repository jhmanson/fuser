//! Opt-in (`FUSER_TRACE`) per-opcode counters for the session event loop:
//! request count and dispatch-thread time per opcode, plus time blocked in
//! `receive`. Diagnostic only.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

const SLOTS: usize = 64;

struct Slot {
    calls: AtomicU64,
    nanos: AtomicU64,
}

#[allow(clippy::declare_interior_mutable_const)]
const EMPTY: Slot = Slot {
    calls: AtomicU64::new(0),
    nanos: AtomicU64::new(0),
};
static OPS: [Slot; SLOTS] = [EMPTY; SLOTS];
static RECEIVE: Slot = EMPTY;

pub(crate) fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("FUSER_TRACE").is_ok_and(|v| !v.is_empty() && v != "0"))
}

pub(crate) fn now() -> Option<Instant> {
    enabled().then(Instant::now)
}

fn add(slot: &Slot, start: Instant) {
    slot.calls.fetch_add(1, Ordering::Relaxed);
    slot.nanos
        .fetch_add(start.elapsed().as_nanos() as u64, Ordering::Relaxed);
}

pub(crate) fn receive_done(start: Option<Instant>) {
    if let Some(start) = start {
        add(&RECEIVE, start);
    }
}

/// `buf` is the raw request; its opcode is the `u32` at byte 4.
pub(crate) fn dispatch_done(buf: &[u8], start: Option<Instant>) {
    if let (Some(start), Some(op)) = (start, buf.get(4..8)) {
        let op = u32::from_ne_bytes(op.try_into().unwrap()) as usize;
        add(&OPS[op.min(SLOTS - 1)], start);
    }
}

fn name(op: usize) -> &'static str {
    match op {
        1 => "LOOKUP",
        2 => "FORGET",
        3 => "GETATTR",
        5 => "READLINK",
        14 => "OPEN",
        15 => "READ",
        17 => "STATFS",
        18 => "RELEASE",
        22 => "GETXATTR",
        23 => "LISTXATTR",
        25 => "FLUSH",
        27 => "OPENDIR",
        28 => "READDIR",
        29 => "RELEASEDIR",
        34 => "ACCESS",
        42 => "BATCH_FORGET",
        44 => "READDIRPLUS",
        _ => "other",
    }
}

/// Prints the counters to stderr when the session loop ends; no-op unless
/// `FUSER_TRACE` is set.
pub(crate) fn report() {
    if !enabled() {
        return;
    }
    let line = |label: &str, slot: &Slot| {
        let calls = slot.calls.load(Ordering::Relaxed);
        let nanos = slot.nanos.load(Ordering::Relaxed);
        if calls > 0 {
            eprintln!(
                "fuser trace: {label:<24} calls={calls:<9} total={:>10.1}ms  avg={:>8.1}µs",
                nanos as f64 / 1e6,
                nanos as f64 / calls as f64 / 1e3
            );
        }
    };
    line("receive (blocked)", &RECEIVE);
    for (op, slot) in OPS.iter().enumerate() {
        line(&format!("dispatch.{}({op})", name(op)), slot);
    }
}
