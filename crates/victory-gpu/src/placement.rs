//! Where host copies of title memory live, and what that placement costs:
//! the observing half of an adaptive placement of copies between the PC's
//! memory and the card's (research checklist R23, R27: "telemetry-only").
//! It changes nothing: no copy moves, none is made elsewhere, no switch's
//! default changes.
//!
//! Every 30 s ([`tick`], from the sweep) a line per placement says, of the
//! copies the GPU side used in that time, how many there were, their
//! bytes, how often lists asked for them, how many bytes the CPU side
//! brought up to date in them and how many the GPU wrote that went back to
//! title memory; and a grid of the copies by how often they were asked
//! for against how much of each was rewritten (its refreshed bytes over
//! its size, per second). The grid has no thresholds in it: it shows
//! whether there is a group worth the card's memory, before any rule says
//! which. A last line puts the card's budget beside it, and the bytes of
//! the copies in the PC's memory that were asked for often and rewritten
//! little ("hot and stable"), which is what a first rule would take, with
//! the room in the card's memory they would need.
//!
//! What each placement means for the card (for the reader of the lines):
//! a copy in the PC's memory is read by shaders over the bus at every
//! access, and the CPU writes it in place; one in the card's memory is
//! read there, and what the CPU changed is sent up by copies; one in a GPU
//! upload heap is in the card's memory and written by the CPU through the
//! resizable BAR; in place is no copy (the card reads title memory).
//!
//! `VICTORY_PLACEMENT_LOOK=0` turns it off (nothing is counted or said).

use std::sync::Mutex;

/// Where a copy is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Placement {
    /// In the PC's memory (an upload heap): read over the bus.
    Pc = 0,
    /// In the card's memory, with a side in the PC's that the CPU writes.
    Card = 1,
    /// In a GPU upload heap (a window, `VICTORY_WINDOW_UPLOAD=1`).
    Upload = 2,
    /// Not a copy: title memory itself (`crate::inplace`).
    InPlace = 3,
}

impl Placement {
    const ALL: [Placement; 4] = [Placement::Pc, Placement::Card, Placement::Upload, Placement::InPlace];

    fn name(self) -> &'static str {
        match self {
            Placement::Pc => "PC memory (read over the bus)",
            Placement::Card => "card memory",
            Placement::Upload => "GPU upload heap",
            Placement::InPlace => "in place",
        }
    }
}

/// One copy as the look found it: what it was asked and did since the
/// last look.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Seen {
    pub(crate) placement: Placement,
    /// A window of title memory outside buffers (else a buffer's copy).
    pub(crate) window: bool,
    /// The title bytes it copies.
    pub(crate) len: u64,
    /// Lists that asked for it.
    pub(crate) asks: u64,
    /// Bytes the CPU side brought up to date in it.
    pub(crate) refreshed: u64,
    /// Bytes the GPU wrote in it that went back to title memory.
    pub(crate) written_back: u64,
}

/// Asks a second: under 1, 1 to 10, 10 to 100, 100 and more.
const RATES: [&str; 4] = ["<1/s", "1-10/s", "10-100/s", ">=100/s"];
/// The copy's bytes rewritten a second over its size: none, under a tenth,
/// under all of it, all of it or more.
const CHURNS: [&str; 4] = ["0", "<0.1", "<1", ">=1"];

fn rate_bucket(asks: u64, secs: f64) -> usize {
    let per = asks as f64 / secs.max(1e-3);
    if per < 1.0 {
        0
    } else if per < 10.0 {
        1
    } else if per < 100.0 {
        2
    } else {
        3
    }
}

fn churn_bucket(refreshed: u64, len: u64, secs: f64) -> usize {
    if refreshed == 0 {
        return 0;
    }
    let per = refreshed as f64 / secs.max(1e-3) / len.max(1) as f64;
    if per < 0.1 {
        1
    } else if per < 1.0 {
        2
    } else {
        3
    }
}

/// The card's memory a copy of `len` bytes would take there: its buffer
/// (a committed buffer is 64 KiB at the least, in 64 KiB steps), and as
/// much again for the "before" twin when shaders write it.
fn card_cost(len: u64, written: bool) -> u64 {
    let one = len.max(1).next_multiple_of(64 << 10);
    if written { 2 * one } else { one }
}

/// "Hot and stable": asked 10 times a second or more, and under a tenth of
/// it rewritten a second. Not a rule: the cell of the grid a first rule
/// would take, named for the summary line.
fn hot_and_stable(s: &Seen, secs: f64) -> bool {
    rate_bucket(s.asks, secs) >= 2 && churn_bucket(s.refreshed, s.len, secs) <= 1
}

/// The totals of one placement.
#[derive(Default, Debug, PartialEq)]
struct Totals {
    copies: u64,
    bytes: u64,
    asks: u64,
    refreshed: u64,
    written_back: u64,
    windows: u64,
    /// Copies and bytes by [rate][churn].
    grid: [[(u64, u64); 4]; 4],
}

/// What a look found, by placement, and the hot-and-stable copies in the
/// PC's memory with the card's memory they would take.
#[derive(Default, Debug)]
struct Summary {
    by: [Totals; 4],
    hot: (u64, u64, u64),
}

fn summarize(seen: &[Seen], secs: f64) -> Summary {
    let mut sum = Summary::default();
    for s in seen {
        let t = &mut sum.by[s.placement as usize];
        t.copies += 1;
        t.bytes += s.len;
        t.asks += s.asks;
        t.refreshed += s.refreshed;
        t.written_back += s.written_back;
        t.windows += u64::from(s.window);
        let cell = &mut t.grid[rate_bucket(s.asks, secs)][churn_bucket(s.refreshed, s.len, secs)];
        cell.0 += 1;
        cell.1 += s.len;
        if s.placement == Placement::Pc && hot_and_stable(s, secs) {
            sum.hot.0 += 1;
            sum.hot.1 += s.len;
            sum.hot.2 += card_cost(s.len, s.written_back != 0);
        }
    }
    sum
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1 << 20) as f64
}

/// The line of one placement, `None` if nothing there was used.
fn line(p: Placement, t: &Totals, secs: f64) -> Option<String> {
    if t.copies == 0 {
        return None;
    }
    let mut grid = Vec::new();
    for (r, row) in t.grid.iter().enumerate() {
        for (c, &(n, bytes)) in row.iter().enumerate() {
            if n != 0 {
                grid.push(format!("{} x {}: {n} ({:.1} MiB)", RATES[r], CHURNS[c], mib(bytes)));
            }
        }
    }
    Some(format!(
        "{}: {} copies ({} windows) {:.1} MiB; per second {:.0} asks, {:.1} MiB brought up to date, {:.1} MiB written back; \
         by asks x rewritten share: {}",
        p.name(),
        t.copies,
        t.windows,
        mib(t.bytes),
        t.asks as f64 / secs,
        mib(t.refreshed) / secs,
        mib(t.written_back) / secs,
        grid.join(", ")
    ))
}

/// Whether the look is on (`VICTORY_PLACEMENT_LOOK=0`: off).
pub(crate) fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_PLACEMENT_LOOK").is_none_or(|v| v != "0"))
}

/// Once a second at most (the sweep): every 30 s, the lines.
#[cfg(windows)]
pub(crate) fn tick() {
    static LAST: Mutex<Option<std::time::Instant>> = Mutex::new(None);
    if !on() {
        return;
    }
    let now = std::time::Instant::now();
    let secs = {
        let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
        let Some(at) = *last else {
            *last = Some(now);
            // The first window starts here: what was counted before it
            // is taken and dropped, so the first lines cover 30 s.
            let _ = crate::shadow::placement_look();
            return;
        };
        let secs = now.duration_since(at).as_secs_f64();
        if secs < 30.0 {
            return;
        }
        *last = Some(now);
        secs
    };
    let (seen, cold) = crate::shadow::placement_look();
    let sum = summarize(&seen, secs);
    for p in Placement::ALL {
        if let Some(text) = line(p, &sum.by[p as usize], secs) {
            tracing::info!("placement of host copies over {secs:.0} s, {text}");
        }
    }
    let [card, twins] = crate::shadow::card_bytes();
    let budget = crate::host::host().and_then(|h| h.video_memory());
    let (budget, used) = budget.map_or((0, 0), |m| (m[0], m[1]));
    let kept = crate::shadow::card_room_kept();
    let room = budget.saturating_sub(used + kept);
    tracing::info!(
        "placement: the card's budget {:.0} MiB, {:.0} MiB in use; copies in the card's memory {:.0} MiB and their twins {:.0} MiB, \
         {:.0} MiB kept for those not made yet; {} copies there {:.1} MiB not asked for in {secs:.0} s; in the PC's memory {} copies \
         {:.1} MiB were hot and stable (asked 10 times a second or more, under a tenth rewritten a second), which would take {:.1} MiB \
         of the card's memory: {} in the {:.0} MiB free beside what is kept (observed only; nothing moves; VICTORY_PLACEMENT_LOOK=0)",
        mib(budget),
        mib(used),
        mib(card),
        mib(twins),
        mib(kept),
        cold.0,
        mib(cold.1),
        sum.hot.0,
        mib(sum.hot.1),
        mib(sum.hot.2),
        if budget == 0 {
            "budget unknown"
        } else if sum.hot.2 <= room {
            "would fit"
        } else {
            "would not fit"
        },
        mib(room)
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seen(placement: Placement, len: u64, asks: u64, refreshed: u64, written_back: u64) -> Seen {
        Seen { placement, window: false, len, asks, refreshed, written_back }
    }

    #[test]
    fn rates_and_churn_fall_in_their_buckets() {
        assert_eq!(rate_bucket(0, 30.0), 0);
        assert_eq!(rate_bucket(29, 30.0), 0);
        assert_eq!(rate_bucket(30, 30.0), 1);
        assert_eq!(rate_bucket(300, 30.0), 2);
        assert_eq!(rate_bucket(3000, 30.0), 3);
        assert_eq!(churn_bucket(0, 1 << 20, 30.0), 0);
        // 1 MiB copy, 1 MiB rewritten in 30 s: a thirtieth a second.
        assert_eq!(churn_bucket(1 << 20, 1 << 20, 30.0), 1);
        assert_eq!(churn_bucket(2 << 20, 1 << 20, 30.0), 1);
        assert_eq!(churn_bucket(3 << 20, 1 << 20, 30.0), 2, "a tenth a second is no longer under a tenth");
        assert_eq!(churn_bucket(2 << 20, 1 << 20, 3.0), 2);
        assert_eq!(churn_bucket(3 << 20, 1 << 20, 3.0), 3, "all of it a second");
        assert_eq!(churn_bucket(30 << 20, 1 << 20, 30.0), 3);
    }

    #[test]
    fn the_card_cost_rounds_to_64_kib_and_doubles_for_written_copies() {
        assert_eq!(card_cost(288, false), 64 << 10);
        assert_eq!(card_cost(288, true), 128 << 10);
        assert_eq!(card_cost(1 << 20, false), 1 << 20);
        assert_eq!(card_cost((1 << 20) + 1, false), (1 << 20) + (64 << 10));
    }

    #[test]
    fn only_hot_stable_copies_in_the_pcs_memory_are_candidates() {
        let secs = 30.0;
        let found = [
            // Hot (20/s) and never rewritten, a 288-byte buffer shaders write.
            seen(Placement::Pc, 288, 600, 0, 4),
            // Hot but rewritten whole every second.
            seen(Placement::Pc, 1 << 20, 600, 30 << 20, 0),
            // Stable but cold.
            seen(Placement::Pc, 1 << 20, 10, 0, 0),
            // Hot and stable, but already on the card.
            seen(Placement::Card, 4 << 20, 6000, 0, 0),
        ];
        let sum = summarize(&found, secs);
        assert_eq!(sum.hot, (1, 288, 128 << 10));
        assert_eq!(sum.by[Placement::Pc as usize].copies, 3);
        assert_eq!(sum.by[Placement::Card as usize].copies, 1);
        // The grid: [10-100/s][0] holds the first, [10-100/s][>=1] the
        // second, [<1/s][0] the third.
        let pc = &sum.by[Placement::Pc as usize].grid;
        assert_eq!(pc[2][0], (1, 288));
        assert_eq!(pc[2][3], (1, 1 << 20));
        assert_eq!(pc[0][0], (1, 1 << 20));
    }

    #[test]
    fn a_placement_with_nothing_used_says_nothing() {
        let sum = summarize(&[], 30.0);
        assert!(line(Placement::Pc, &sum.by[0], 30.0).is_none());
        let sum = summarize(&[seen(Placement::Upload, 4 << 20, 90, 1 << 20, 0)], 30.0);
        let text = line(Placement::Upload, &sum.by[Placement::Upload as usize], 30.0).unwrap();
        assert!(text.starts_with("GPU upload heap: 1 copies"), "{text}");
        assert!(text.contains("1-10/s x <0.1: 1 (4.0 MiB)"), "{text}");
    }
}
