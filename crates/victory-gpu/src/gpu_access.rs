//! What the title says the GPU may do with each part of its memory.
//!
//! On the console, `XMemVirtualAlloc` takes protection flags beyond the
//! host's: the CPU's (`PAGE_READWRITE` and the like, bits 0-8) and the
//! GPU's (`PAGE_GRAPHICS_*`, bits 11-18; the values are the public ones in
//! Windows' `winnt.h`). `xmem` hands the host only the CPU's, since on the
//! host our GPU side reads title memory through the CPU anyway. Until now
//! the GPU's were dropped, so every page had to be treated as one the GPU
//! may write: copy-back, snapshots and write marks for all of it.
//!
//! This module keeps them. It only records and reports so far (a census
//! line, `GPU access by the title's own flags`): nothing reads
//! [`gpu_access`] to skip work yet. Before anything does, a run must show
//! which flags this title really passes and that no GPU write lands in
//! memory marked read-only or no-access (see [`note_gpu_write`]).
//!
//! What it cannot see: the title's own `VirtualFree`, which goes to the
//! host directly. A range freed and then allocated again is overwritten by
//! the new call; a range freed and never allocated again keeps its old
//! entry. Until frees are seen, a stale entry must not be trusted for
//! memory that is not known to be the same allocation.

use std::collections::BTreeMap;
use std::sync::{Mutex, RwLock};
use std::sync::atomic::{AtomicU64, Ordering};

pub const PAGE_GRAPHICS_NOACCESS: u32 = 0x0800;
pub const PAGE_GRAPHICS_READONLY: u32 = 0x1000;
pub const PAGE_GRAPHICS_READWRITE: u32 = 0x2000;
pub const PAGE_GRAPHICS_EXECUTE: u32 = 0x4000;
pub const PAGE_GRAPHICS_EXECUTE_READ: u32 = 0x8000;
pub const PAGE_GRAPHICS_EXECUTE_READWRITE: u32 = 0x1_0000;
pub const PAGE_GRAPHICS_COHERENT: u32 = 0x2_0000;
pub const PAGE_GRAPHICS_NOCACHE: u32 = 0x4_0000;

/// What the GPU may do with a range, in order of what it allows: a range
/// of mixed kinds answers the most permissive. [`GpuAccess::Unstated`]
/// allows everything (no flag, or memory never seen allocated).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum GpuAccess {
    /// `PAGE_GRAPHICS_NOACCESS`: the GPU does not touch it.
    None,
    /// `PAGE_GRAPHICS_READONLY` or `_EXECUTE`/`_EXECUTE_READ`: the GPU
    /// reads it, never writes it.
    Read,
    /// `PAGE_GRAPHICS_READWRITE` or `_EXECUTE_READWRITE`.
    ReadWrite,
    /// No GPU flag given, or nothing known.
    Unstated,
}

impl GpuAccess {
    const ALL: [GpuAccess; 4] = [GpuAccess::None, GpuAccess::Read, GpuAccess::ReadWrite, GpuAccess::Unstated];

    pub(crate) fn index(self) -> usize {
        self as usize
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            GpuAccess::None => "none",
            GpuAccess::Read => "read",
            GpuAccess::ReadWrite => "read-write",
            GpuAccess::Unstated => "unstated",
        }
    }
}

/// The GPU's access from a console protection value. Write wins over
/// read where both are set; no-access is only no-access alone.
pub fn from_protect(protect: u32) -> GpuAccess {
    if protect & (PAGE_GRAPHICS_READWRITE | PAGE_GRAPHICS_EXECUTE_READWRITE) != 0 {
        GpuAccess::ReadWrite
    } else if protect & (PAGE_GRAPHICS_READONLY | PAGE_GRAPHICS_EXECUTE | PAGE_GRAPHICS_EXECUTE_READ) != 0 {
        GpuAccess::Read
    } else if protect & PAGE_GRAPHICS_NOACCESS != 0 {
        GpuAccess::None
    } else {
        GpuAccess::Unstated
    }
}

/// Disjoint ranges: start -> (end, access, the console protection).
#[derive(Default)]
struct Ranges(BTreeMap<usize, (usize, GpuAccess, u32)>);

impl Ranges {
    /// `[at, end)` is now `access`: what it overlaps is cut back or gone.
    fn set(&mut self, at: usize, end: usize, access: GpuAccess, protect: u32) {
        if end <= at {
            return;
        }
        // A range starting before `at` that reaches into it: keep its head,
        // and its tail if it reaches past `end`.
        if let Some((&s, &(e, a, p))) = self.0.range(..at).next_back()
            && e > at
        {
            self.0.insert(s, (at, a, p));
            if e > end {
                self.0.insert(end, (e, a, p));
            }
        }
        // Ranges starting inside: gone, but a tail past `end` stays.
        let inside: Vec<usize> = self.0.range(at..end).map(|(&s, _)| s).collect();
        for s in inside {
            let (e, a, p) = self.0.remove(&s).unwrap();
            if e > end {
                self.0.insert(end, (e, a, p));
            }
        }
        self.0.insert(at, (end, access, protect));
    }

    /// The most permissive access over `[at, end)`; a part nothing covers
    /// is [`GpuAccess::Unstated`].
    fn get(&self, at: usize, end: usize) -> GpuAccess {
        if end <= at {
            return GpuAccess::None;
        }
        let mut most = GpuAccess::None;
        let mut covered = at;
        let first = self.0.range(..=at).next_back().filter(|(_, (e, _, _))| *e > at);
        for (&s, &(e, a, _)) in first.into_iter().chain(self.0.range(at.saturating_add(1)..end)) {
            if s > covered {
                return GpuAccess::Unstated;
            }
            most = most.max(a);
            covered = covered.max(e);
            if covered >= end {
                return most;
            }
        }
        GpuAccess::Unstated
    }

    /// Bytes and ranges of each kind.
    fn census(&self) -> [(u64, u64); 4] {
        let mut out = [(0u64, 0u64); 4];
        for (&s, &(e, a, _)) in &self.0 {
            let slot = &mut out[a.index()];
            slot.0 += (e - s) as u64;
            slot.1 += 1;
        }
        out
    }
}

/// Read far more often than written (lookups from the submission and list
/// threads, writes only at allocations): a read-write lock.
static RANGES: RwLock<Ranges> = RwLock::new(Ranges(BTreeMap::new()));

/// The title allocated or committed `[at, at + len)` with the console
/// protection `protect` (its own value, before `xmem` cleans it for the
/// host). Called by `xmem` for every `XMemVirtualAlloc` that succeeds.
pub fn title_memory_gpu_access(at: usize, len: usize, protect: u32) {
    let access = from_protect(protect);
    SEEN[access.index()].fetch_add(1, Ordering::Relaxed);
    RANGES.write().unwrap_or_else(|e| e.into_inner()).set(at, at.saturating_add(len), access, protect);
}

/// What the title said the GPU may do over `[at, at + len)`: the most
/// permissive of what covers it, [`GpuAccess::Unstated`] where nothing
/// does. Not used to skip work yet (see the module's notes).
pub fn gpu_access(at: usize, len: usize) -> GpuAccess {
    RANGES.read().unwrap_or_else(|e| e.into_inner()).get(at, at.saturating_add(len))
}

/// The GPU side's work over title memory that the title's flags could
/// spare, counted by the access of the memory it lands on: what tells, in a
/// run, which use of the flags is worth making.
#[derive(Clone, Copy, Debug)]
pub enum Cost {
    /// Ranges a list asks to bring up to date before the GPU reads them
    /// (`Shadow::refresh`; asked, not the bytes found changed).
    Refreshed = 0,
    /// Ranges a submission's shaders may write, on copies in the card's
    /// memory: what the card hashes before and compares after.
    Declared = 1,
    /// Bytes found written by the GPU and stored back into title memory.
    WrittenBack = 2,
}

impl Cost {
    const ALL: [Cost; 3] = [Cost::Refreshed, Cost::Declared, Cost::WrittenBack];

    fn name(self) -> &'static str {
        match self {
            Cost::Refreshed => "refreshed",
            Cost::Declared => "declared written",
            Cost::WrittenBack => "written back",
        }
    }
}

/// Bytes of each [`Cost`] by [`GpuAccess`], since the last census line.
static COSTS: [[AtomicU64; 4]; 3] = [const { [const { AtomicU64::new(0) }; 4] }; 3];

/// `len` bytes of `cost` at title memory `at`, counted by its access.
/// `VICTORY_GPU_ACCESS_COSTS=0` turns the counting off.
pub fn count(cost: Cost, at: usize, len: usize) {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if len == 0 || !*ON.get_or_init(|| std::env::var_os("VICTORY_GPU_ACCESS_COSTS").is_none_or(|v| v != "0")) {
        return;
    }
    let access = gpu_access(at, len);
    COSTS[cost as usize][access.index()].fetch_add(len as u64, Ordering::Relaxed);
}

/// Calls of each kind so far, and GPU writes found where the title said
/// the GPU does not write.
static SEEN: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
static WRITES_AGAINST: AtomicU64 = AtomicU64::new(0);

/// The GPU side found that the GPU wrote `[at, at + len)` (a marked page,
/// a copy-back that changed bytes). Counted, and logged the first times,
/// where the title said the GPU does not write there: the check that has
/// to come out at zero before the flags may be trusted to skip work.
pub fn note_gpu_write(at: usize, len: usize) {
    note_gpu_writes(at, len, len);
}

/// [`note_gpu_write`] once for a stretch `[at, at + span)` in which
/// `bytes` were written (a merge's runs of words): one lookup and one count
/// where a call a run made tens of thousands a frame on several threads
/// (run 15: about 54,000 runs a frame).
pub fn note_gpu_writes(at: usize, span: usize, bytes: usize) {
    if span == 0 || bytes == 0 {
        return;
    }
    let access = gpu_access(at, span);
    COSTS[Cost::WrittenBack as usize][access.index()].fetch_add(bytes as u64, Ordering::Relaxed);
    if matches!(access, GpuAccess::None | GpuAccess::Read) {
        let n = WRITES_AGAINST.fetch_add(1, Ordering::Relaxed);
        if n < 16 {
            tracing::warn!(
                at = format_args!("{at:#x}"),
                len = span,
                said = access.name(),
                "GPU write into title memory the title marked as not GPU-written"
            );
        }
    }
}

/// Once a second at most (from the sweep): every 30 s, the census line if
/// anything was allocated since the last one.
pub(crate) fn tick() {
    static LAST: Mutex<Option<(std::time::Instant, u64)>> = Mutex::new(None);
    let calls: u64 = SEEN.iter().map(|c| c.load(Ordering::Relaxed)).sum();
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    let now = std::time::Instant::now();
    if let Some((at, seen)) = *last
        && (now.duration_since(at).as_secs() < 30 || seen == calls)
    {
        return;
    }
    *last = Some((now, calls));
    let census = RANGES.read().unwrap_or_else(|e| e.into_inner()).census();
    let line: Vec<String> = GpuAccess::ALL
        .iter()
        .map(|&a| {
            let (bytes, n) = census[a.index()];
            format!("{}={:.1} MiB/{n} ranges/{} calls", a.name(), bytes as f64 / (1 << 20) as f64, SEEN[a.index()].load(Ordering::Relaxed))
        })
        .collect();
    tracing::info!(
        writes_against = WRITES_AGAINST.load(Ordering::Relaxed),
        "GPU access by the title's own flags: {}",
        line.join(", ")
    );
    // What the GPU side did over each kind since the last line.
    let costs: Vec<String> = Cost::ALL
        .iter()
        .map(|&c| {
            let by: Vec<String> = GpuAccess::ALL
                .iter()
                .map(|&a| format!("{} {:.1} MiB", a.name(), COSTS[c as usize][a.index()].swap(0, Ordering::Relaxed) as f64 / (1 << 20) as f64))
                .collect();
            format!("{}: {}", c.name(), by.join(" / "))
        })
        .collect();
    tracing::info!("GPU side's work by the title's flags since the last line: {}", costs.join("; "));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_map_to_the_most_permissive_access() {
        assert_eq!(from_protect(0x04), GpuAccess::Unstated); // PAGE_READWRITE alone
        assert_eq!(from_protect(0x04 | PAGE_GRAPHICS_NOACCESS), GpuAccess::None);
        assert_eq!(from_protect(0x04 | PAGE_GRAPHICS_READONLY), GpuAccess::Read);
        assert_eq!(from_protect(0x02 | PAGE_GRAPHICS_EXECUTE_READ), GpuAccess::Read);
        assert_eq!(from_protect(0x04 | PAGE_GRAPHICS_READWRITE), GpuAccess::ReadWrite);
        assert_eq!(from_protect(PAGE_GRAPHICS_READONLY | PAGE_GRAPHICS_READWRITE), GpuAccess::ReadWrite);
        assert_eq!(from_protect(PAGE_GRAPHICS_NOACCESS | PAGE_GRAPHICS_READONLY), GpuAccess::Read);
        assert_eq!(
            from_protect(PAGE_GRAPHICS_READWRITE | PAGE_GRAPHICS_COHERENT | PAGE_GRAPHICS_NOCACHE),
            GpuAccess::ReadWrite
        );
    }

    #[test]
    fn a_later_call_replaces_what_it_overlaps() {
        let mut r = Ranges::default();
        r.set(0x1000, 0x5000, GpuAccess::Read, 0);
        // A commit in the middle with other flags splits the reservation.
        r.set(0x2000, 0x3000, GpuAccess::ReadWrite, 0);
        assert_eq!(r.get(0x1000, 0x2000), GpuAccess::Read);
        assert_eq!(r.get(0x2000, 0x3000), GpuAccess::ReadWrite);
        assert_eq!(r.get(0x3000, 0x5000), GpuAccess::Read);
        assert_eq!(r.get(0x1800, 0x2800), GpuAccess::ReadWrite, "a mixed range answers the most permissive");
        assert_eq!(r.0.len(), 3);
        // One call over all of it.
        r.set(0x0800, 0x6000, GpuAccess::None, 0);
        assert_eq!(r.0.len(), 1);
        assert_eq!(r.get(0x1000, 0x5000), GpuAccess::None);
    }

    #[test]
    fn what_nothing_covers_is_unstated() {
        let mut r = Ranges::default();
        r.set(0x1000, 0x2000, GpuAccess::None, 0);
        r.set(0x3000, 0x4000, GpuAccess::None, 0);
        assert_eq!(r.get(0x1000, 0x2000), GpuAccess::None);
        assert_eq!(r.get(0x1800, 0x3800), GpuAccess::Unstated, "the gap between them");
        assert_eq!(r.get(0x0, 0x1000), GpuAccess::Unstated);
        assert_eq!(r.get(0x3800, 0x4800), GpuAccess::Unstated, "past the end");
        assert_eq!(r.get(0x1000, 0x1000), GpuAccess::None, "an empty range");
        // Adjacent ranges cover without a gap.
        r.set(0x2000, 0x3000, GpuAccess::Read, 0);
        assert_eq!(r.get(0x1000, 0x4000), GpuAccess::Read);
    }

    #[test]
    fn a_tail_past_the_new_range_is_kept() {
        let mut r = Ranges::default();
        r.set(0x1000, 0x2000, GpuAccess::Read, 0);
        r.set(0x2000, 0x8000, GpuAccess::ReadWrite, 0);
        r.set(0x1800, 0x3000, GpuAccess::None, 0);
        assert_eq!(r.get(0x1000, 0x1800), GpuAccess::Read);
        assert_eq!(r.get(0x1800, 0x3000), GpuAccess::None);
        assert_eq!(r.get(0x3000, 0x8000), GpuAccess::ReadWrite);
        let census = r.census();
        assert_eq!(census[GpuAccess::Read.index()], (0x800, 1));
        assert_eq!(census[GpuAccess::None.index()], (0x1800, 1));
        assert_eq!(census[GpuAccess::ReadWrite.index()], (0x5000, 1));
    }
}
