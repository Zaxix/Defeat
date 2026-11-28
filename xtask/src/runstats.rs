//! `cargo xtask runstats <run.txt>...`: one table per run log of the
//! things a run is judged by, compared like for like.
//!
//! Whole-run averages mislead: menus run at 30 fps and loading frames at
//! 15, and two runs that spent their time in different rooms differ by
//! their rooms. So frames are put in bands of draws per frame (the world
//! has 3,000 and more) and each band gets its own medians. Every switch a
//! run turned in turn (`VICTORY_X=alt`) splits the frames by its phases,
//! leaving out those within a few seconds of a turn: a frame line covers
//! about five seconds, and work of one phase lands in the next (research
//! R62: with a 4 s guard run 15's write-back phases were 204 against 206 ms
//! a frame, 193 against 215 without one).
//!
//! The per-second lines (the card's clock, the merges, the preparing) are
//! joined to the nearest frame line within 2.6 s. Their figures overlap
//! (work on several threads at once): they are not to be added up.
//!
//! Reads the log as the program wrote it (colour codes and all); nothing
//! of the game is read or kept.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};

/// Draws per frame: the bands frames are compared in.
const BANDS: [(&str, f64, f64); 6] = [
    ("world, 3,000+ draws", 3000.0, f64::INFINITY),
    ("3,000-4,600", 3000.0, 4600.0),
    ("4,500-5,500", 4500.0, 5500.0),
    ("4,600-6,900", 4600.0, 6900.0),
    ("6,900+", 6900.0, f64::INFINITY),
    ("under 1,000 (menus, loading)", 0.0, 1000.0),
];

/// How far a per-second line may be from the frame line it is joined to.
const JOIN_S: f64 = 2.6;

/// One frame line (about five seconds of frames).
#[derive(Clone, Default, Debug, PartialEq)]
struct Frame {
    at: f64,
    ms: f64,
    preparing: f64,
    copies_ms: f64,
    copies_mb: f64,
    back_ms: f64,
    draws: f64,
    dropped: f64,
    /// From the nearest card's-clock line: nothing of ours queued, and of
    /// that the time the submitting thread was working.
    idle: Option<f64>,
    idle_submitting: Option<f64>,
    /// From the nearest merges-by-part line: written back (all threads).
    written_back: Option<f64>,
}

/// What a run log says.
#[derive(Default, Debug)]
struct Run {
    source: Option<String>,
    switches: Option<String>,
    frames: Vec<Frame>,
    /// Turns of `alt` switches: (time, switch, phase).
    turns: Vec<(f64, String, String)>,
    /// (time, used at most, budget at least), MB.
    vram: Vec<(f64, f64, f64)>,
    /// DirectStorage 5 s lines: (slowest ms, requests over 250 ms, over 1 s).
    storage: Vec<(f64, f64, f64)>,
    /// The guards' lock lines: (waited, held) ms a frame.
    guards: Vec<(f64, f64)>,
    /// The latest guards' lock line's "held longest" list.
    guards_last: Option<String>,
}

pub fn run(paths: &[std::path::PathBuf], guard_s: f64) -> Result<()> {
    anyhow::ensure!(!paths.is_empty(), "name one or more run logs (run15.txt ...)");
    for path in paths {
        let run = read(path)?;
        print!("{}", report(path, &run, guard_s));
    }
    Ok(())
}

fn read(path: &Path) -> Result<Run> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(parse(&String::from_utf8_lossy(&bytes)))
}

/// The line without terminal colour codes.
fn plain(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // ESC [ ... letter
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// Seconds since 1970 of a log line's leading `2026-11-28T00:25:09.961624Z`.
fn stamp(line: &str) -> Option<f64> {
    let s = line.get(..26)?;
    if s.as_bytes().get(10) != Some(&b'T') {
        return None;
    }
    let n = |r: std::ops::Range<usize>| s.get(r).and_then(|t| t.parse::<i64>().ok());
    let (y, m, d) = (n(0..4)?, n(5..7)?, n(8..10)?);
    let (hh, mm) = (n(11..13)?, n(14..16)?);
    let secs: f64 = s.get(17..26)?.trim_end_matches('Z').parse().ok()?;
    // Days from the civil date (Howard Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some((days * 86_400 + hh * 3600 + mm * 60) as f64 + secs)
}

/// The number right after `key`.
fn after(line: &str, key: &str) -> Option<f64> {
    let rest = &line[line.find(key)? + key.len()..];
    let end = rest.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// The number right before `key`.
fn before(line: &str, key: &str) -> Option<f64> {
    let head = line[..line.find(key)?].trim_end();
    head.rsplit(|c: char| c.is_whitespace() || c == '(').next()?.parse().ok()
}

fn parse(text: &str) -> Run {
    let mut run = Run::default();
    // Per-second lines, joined afterwards: (time, a, b).
    let mut clock: Vec<(f64, f64, f64)> = Vec::new();
    let mut merges: Vec<(f64, f64)> = Vec::new();
    for raw in text.lines() {
        let line = plain(raw);
        if let Some(rest) = line.strip_prefix("source ") {
            run.source = Some(rest.trim().to_owned());
            continue;
        }
        if let Some(rest) = line.strip_prefix("switches ") {
            run.switches = Some(rest.trim().to_owned());
            continue;
        }
        let Some(at) = stamp(&line) else { continue };
        if line.contains(" ms/frame (") && line.contains(" draws/frame") {
            let frame = Frame {
                at,
                ms: before(&line, " ms/frame").unwrap_or(0.0),
                preparing: after(&line, "ms/frame (").unwrap_or(0.0),
                copies_ms: after(&line, "preparing: copies ").unwrap_or(0.0),
                copies_mb: before(&line, " MB, textures").unwrap_or(0.0),
                back_ms: before(&line, " copying back").unwrap_or(0.0),
                draws: before(&line, " draws/frame").unwrap_or(0.0),
                dropped: after(&line, "draws/frame, dropped ").unwrap_or(0.0),
                ..Frame::default()
            };
            run.frames.push(frame);
        } else if line.contains("the card's clock per frame: nothing of ours queued") {
            if let (Some(idle), Some(busy)) =
                (after(&line, "nothing of ours queued "), before(&line, " ms of that while the submitting thread"))
            {
                clock.push((at, idle, busy));
            }
        } else if line.contains("the card copies' merges by part") {
            // "written back 139.2" (run 14) or "; written back 91.3 (" (run 15 on).
            if let Some(w) = after(&line, "written back ") {
                merges.push((at, w));
            }
        } else if let Some(i) = line.find("=alt)") {
            // "from now (VICTORY_X=alt): PHASE" or "(VICTORY_X=alt) from now: PHASE".
            let Some(open) = line[..i].rfind('(') else { continue };
            let switch = line[open + 1..i].to_owned();
            if !switch.starts_with("VICTORY_") || !line.contains("from now") {
                continue;
            }
            let tail = &line[i + 5..];
            let tail = tail.trim_start_matches(" from now").trim_start_matches(':').trim();
            let phase: String = tail.split([',', '(', ';']).next().unwrap_or("").trim().to_owned();
            if !phase.is_empty() {
                run.turns.push((at, switch, phase));
            }
        } else if line.contains("the card's own in use at most") {
            if let (Some(used), Some(budget)) =
                (after(&line, "in use at most "), after(&line, "its budget at least "))
            {
                run.vram.push((at, used, budget));
            }
        } else if line.contains("dstorage, slowest request in the last") {
            let slowest = after(&line, " s: ").unwrap_or(0.0);
            let over = |key: &str| {
                let i = line.find(key)?;
                let part = &line[i + key.len()..];
                let a = part.trim_start().split(' ').next()?.parse::<f64>().ok()?;
                let b = after(part, "plain, ")?;
                Some(a + b)
            };
            run.storage.push((slowest, over("250 ms:").unwrap_or(0.0), over("over 1 s:").unwrap_or(0.0)));
        } else if line.contains("the write-watch guards' lock per frame") {
            if let (Some(w), Some(h)) = (after(&line, "all threads (by the line in watch.rs that takes it): waited "), after(&line, ", held ")) {
                run.guards.push((w, h));
            }
            if let Some(i) = line.find("held longest: ") {
                let rest = &line[i + 14..];
                let end = rest.find("; waited longest").unwrap_or(rest.len());
                run.guards_last = Some(rest[..end].to_owned());
            }
        }
    }
    let nearest = |at: f64, rows: &[(f64, f64, f64)]| -> Option<(f64, f64)> {
        rows.iter()
            .filter(|r| (r.0 - at).abs() <= JOIN_S)
            .min_by(|a, b| (a.0 - at).abs().total_cmp(&(b.0 - at).abs()))
            .map(|r| (r.1, r.2))
    };
    let merges3: Vec<(f64, f64, f64)> = merges.iter().map(|&(t, w)| (t, w, 0.0)).collect();
    for f in &mut run.frames {
        if let Some((idle, busy)) = nearest(f.at, &clock) {
            f.idle = Some(idle);
            f.idle_submitting = Some(busy);
        }
        f.written_back = nearest(f.at, &merges3).map(|m| m.0);
    }
    run
}

/// The `q` quantile (0 to 1) by nearest rank; `None` for no values.
fn quantile(values: &mut [f64], q: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    let rank = ((q * values.len() as f64).ceil() as usize).clamp(1, values.len());
    Some(values[rank - 1])
}

fn median_of(frames: &[&Frame], f: impl Fn(&Frame) -> Option<f64>) -> String {
    let mut v: Vec<f64> = frames.iter().filter_map(|x| f(x)).collect();
    quantile(&mut v, 0.5).map_or_else(|| "-".to_owned(), |m| format!("{m:.1}"))
}

/// One row of a table: count, frame p50/p90 (and fps), the parts' medians.
fn row(name: &str, frames: &[&Frame]) -> String {
    if frames.is_empty() {
        return format!("  {name:<30} N=0\n");
    }
    let mut ms: Vec<f64> = frames.iter().map(|f| f.ms).collect();
    let p50 = quantile(&mut ms, 0.5).unwrap_or(0.0);
    let p90 = quantile(&mut ms, 0.9).unwrap_or(0.0);
    format!(
        "  {name:<30} N={:<4} frame p50 {p50:>6.1} ms ({:>4.1} fps, {:>3.0} % of 30) p90 {p90:>6.1} | draws {:>6} | preparing {:>6} copies {:>5} ms {:>5} MB | copying back {:>6} | card idle {:>6} (behind us {:>6}) | written back, all threads {:>6} | dropped {:>5}\n",
        frames.len(),
        1000.0 / p50.max(1e-9),
        100.0 * 1000.0 / p50.max(1e-9) / 30.0,
        median_of(frames, |f| Some(f.draws)),
        median_of(frames, |f| Some(f.preparing)),
        median_of(frames, |f| Some(f.copies_ms)),
        median_of(frames, |f| Some(f.copies_mb)),
        median_of(frames, |f| Some(f.back_ms)),
        median_of(frames, |f| f.idle),
        median_of(frames, |f| f.idle_submitting),
        median_of(frames, |f| f.written_back),
        median_of(frames, |f| Some(f.dropped)),
    )
}

/// The phase of `switch` at `at`, and whether `at` is at least `guard_s`
/// from every turn of it.
fn phase_at<'a>(turns: &'a [(f64, String, String)], switch: &str, at: f64, guard_s: f64) -> Option<&'a str> {
    let mine: Vec<&(f64, String, String)> = turns.iter().filter(|t| t.1 == switch).collect();
    if mine.iter().any(|t| (t.0 - at).abs() < guard_s) {
        return None;
    }
    mine.iter().rev().find(|t| t.0 <= at).map(|t| t.2.as_str())
}

fn report(path: &Path, run: &Run, guard_s: f64) -> String {
    let mut out = String::new();
    out.push_str(&format!("== {}\n", path.display()));
    out.push_str(&format!(
        "  source {}; switches {}\n",
        run.source.as_deref().unwrap_or("(not said: a build before 2026-11-28)"),
        run.switches.as_deref().unwrap_or("(not said)")
    ));
    if let (Some(first), Some(last)) = (run.frames.first(), run.frames.last()) {
        out.push_str(&format!("  {} frame lines over {:.1} minutes\n", run.frames.len(), (last.at - first.at) / 60.0));
    }
    out.push_str("  by draws per frame (medians of the frame lines; the parts overlap, do not add them):\n");
    for (name, lo, hi) in BANDS {
        let frames: Vec<&Frame> = run.frames.iter().filter(|f| f.draws >= lo && f.draws < hi).collect();
        out.push_str(&row(name, &frames));
    }
    let mut switches: Vec<&str> = run.turns.iter().map(|t| t.1.as_str()).collect();
    switches.sort_unstable();
    switches.dedup();
    for switch in switches {
        let turns = run.turns.iter().filter(|t| t.1 == switch).count();
        out.push_str(&format!(
            "  {switch}=alt, {turns} turns; world frames (3,000+ draws) by phase, those within {guard_s} s of a turn left out:\n"
        ));
        let mut by: BTreeMap<&str, Vec<&Frame>> = BTreeMap::new();
        for f in run.frames.iter().filter(|f| f.draws >= 3000.0) {
            if let Some(p) = phase_at(&run.turns, switch, f.at, guard_s) {
                by.entry(p).or_default().push(f);
            }
        }
        for (phase, frames) in &by {
            out.push_str(&row(phase, frames));
        }
    }
    if !run.vram.is_empty() {
        let headroom: Vec<f64> = run.vram.iter().map(|v| v.2 - v.1).collect();
        let mut h = headroom.clone();
        let last = run.vram.last().copied().unwrap_or_default();
        out.push_str(&format!(
            "  video memory: last {:.0} of {:.0} MB; budget minus use p50 {:.0} MB, least {:.0} MB, {} of {} lines under 256 MB\n",
            last.1,
            last.2,
            quantile(&mut h, 0.5).unwrap_or(0.0),
            headroom.iter().copied().fold(f64::INFINITY, f64::min),
            headroom.iter().filter(|&&x| x < 256.0).count(),
            headroom.len()
        ));
    }
    if !run.storage.is_empty() {
        let mut slowest: Vec<f64> = run.storage.iter().map(|s| s.0).collect();
        out.push_str(&format!(
            "  storage: {} 5 s lines, slowest request p50 {:.0} p90 {:.0} max {:.0} ms; requests over 250 ms {:.0}, over 1 s {:.0}\n",
            run.storage.len(),
            quantile(&mut slowest, 0.5).unwrap_or(0.0),
            quantile(&mut slowest, 0.9).unwrap_or(0.0),
            slowest.iter().copied().fold(0.0, f64::max),
            run.storage.iter().map(|s| s.1).sum::<f64>(),
            run.storage.iter().map(|s| s.2).sum::<f64>(),
        ));
    }
    if !run.guards.is_empty() {
        let mut w: Vec<f64> = run.guards.iter().map(|g| g.0).collect();
        let mut h: Vec<f64> = run.guards.iter().map(|g| g.1).collect();
        out.push_str(&format!(
            "  write-watch guards' lock, ms a frame (all threads): waited p50 {:.1} p90 {:.1}, held p50 {:.1} p90 {:.1}\n",
            quantile(&mut w, 0.5).unwrap_or(0.0),
            quantile(&mut w, 0.9).unwrap_or(0.0),
            quantile(&mut h, 0.5).unwrap_or(0.0),
            quantile(&mut h, 0.9).unwrap_or(0.0),
        ));
        if let Some(last) = &run.guards_last {
            out.push_str(&format!("    held longest at the end: {last}\n"));
        }
    }
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOG: &str = "source         be0ed3b7da1f\n\
switches       VICTORY_BVH_BUILDER=0 VICTORY_WRITE_PLAIN=alt\n\
2026-11-28T00:28:42.858141Z  INFO victory_gpu::plaincopy: what shaders wrote is stored into title memory from now (VICTORY_WRITE_PLAIN=alt): IN PLACE\n\
2026-11-28T00:28:50.000000Z  INFO victory_gpu::host: the card's clock per frame: nothing of ours queued 90.0 ms (the card about 44% busy by this count), 70.0 ms of that while the submitting thread was working (the card idle behind us), the rest\n\
2026-11-28T00:28:51.000000Z  INFO victory_gpu::host: the card copies' merges by part, ms per frame (any thread): the copy's merge lock waited for 0.7, what the card changed found 5.2, the title's bytes sent since gathered 0.4, title memory read 11.3, merged 20.5; written back 91.3 (the fills read ahead told 0.3)\n\
2026-11-28T00:28:52.000000Z  INFO victory_gpu::host: 5.2 fps, 193.0 ms/frame (90.6 preparing: copies 40.0 ms 70.5 MB, textures 5.0 ms 20.0 MB; 0.0 on the GPU; 66.5 copying back 60.0 MB), 5568 draws/frame, dropped 390 without a host pipeline\n\
2026-11-28T00:29:02.883000Z  INFO victory_gpu::plaincopy: what shaders wrote is stored into title memory from now (VICTORY_WRITE_PLAIN=alt): through the kernel\n\
2026-11-28T00:29:12.000000Z  INFO victory_gpu::host: 4.6 fps, 215.0 ms/frame (113.1 preparing: copies 45.0 ms 80.0 MB, textures 5.0 ms 20.0 MB; 0.0 on the GPU; 96.5 copying back 60.0 MB), 5870 draws/frame, dropped 390 without a host pipeline\n\
2026-11-27T23:42:45.846485Z  INFO victory_gpu::framelog: our threads' priority (VICTORY_OUR_PRIORITY=alt) from now: normal\n\
\x1b[2m2026-11-28T00:37:23.234922Z\x1b[0m  INFO victory_gpu::host: video memory in the 5511 ms since the last such line, at 141 looks: the card's own in use at most 10222 MB, its budget at least 10781 MB, the PC's\n";

    #[test]
    fn a_log_is_read_into_frames_turns_and_joined_lines() {
        let run = parse(LOG);
        assert_eq!(run.source.as_deref(), Some("be0ed3b7da1f"));
        assert_eq!(run.frames.len(), 2);
        let f = &run.frames[0];
        assert_eq!((f.ms, f.preparing, f.copies_ms, f.copies_mb, f.back_ms, f.draws, f.dropped), (193.0, 90.6, 40.0, 70.5, 66.5, 5568.0, 390.0));
        assert_eq!((f.idle, f.idle_submitting, f.written_back), (Some(90.0), Some(70.0), Some(91.3)));
        // The second frame line has no per-second line within 2.6 s.
        assert_eq!(run.frames[1].idle, None);
        let turns: Vec<(&str, &str)> = run.turns.iter().map(|t| (t.1.as_str(), t.2.as_str())).collect();
        assert_eq!(
            turns,
            [
                ("VICTORY_WRITE_PLAIN", "IN PLACE"),
                ("VICTORY_WRITE_PLAIN", "through the kernel"),
                ("VICTORY_OUR_PRIORITY", "normal")
            ]
        );
        assert_eq!(run.vram, [(stamp("2026-11-28T00:37:23.234922Z").unwrap(), 10222.0, 10781.0)]);
    }

    #[test]
    fn frames_near_a_turn_are_left_out_of_its_phases() {
        let run = parse(LOG);
        let at = |i: usize| run.frames[i].at;
        // 9.1 s after the first turn; 9.1 s after the second.
        assert_eq!(phase_at(&run.turns, "VICTORY_WRITE_PLAIN", at(0), 4.0), Some("IN PLACE"));
        assert_eq!(phase_at(&run.turns, "VICTORY_WRITE_PLAIN", at(1), 4.0), Some("through the kernel"));
        assert_eq!(phase_at(&run.turns, "VICTORY_WRITE_PLAIN", at(1), 10.0), None);
    }

    #[test]
    fn stamps_count_across_midnight_and_months() {
        let a = stamp("2026-11-30T23:59:59.500000Z").unwrap();
        let b = stamp("2026-12-01T00:00:00.500000Z").unwrap();
        assert!((b - a - 1.0).abs() < 1e-6);
        assert_eq!(stamp("2026-01-01T00:00:00.000000Z"), Some(1_767_225_600.0));
    }

    #[test]
    fn quantiles_take_the_nearest_rank() {
        let mut v = vec![5.0, 1.0, 3.0, 2.0, 4.0];
        assert_eq!(quantile(&mut v, 0.5), Some(3.0));
        assert_eq!(quantile(&mut v, 0.9), Some(5.0));
        assert_eq!(quantile(&mut [], 0.5), None);
    }
}
