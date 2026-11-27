//! The faster set of defaults (`VICTORY_FAST`; on unless `=0`).
//!
//! Switches the gta6 base's tester measured as gains on an RTX 4070
//! (README, "Frame rate, second night" and after: 4.5-5 frames a second
//! in the first rooms with the old defaults, 6-7 and later 9.6-10.3 with
//! these among others), that the owner's runs 1-9 never had because they
//! were off by default:
//!
//! - `VICTORY_ASYNC_SUBMIT`: submissions prepared and sent by a thread of
//!   ours, not the title's render thread;
//! - `VICTORY_QUEUE2=1`: fetches of what shaders wrote go to a copy
//!   queue (the copy-back thread's wait 125 -> 7 ms a frame there);
//! - `VICTORY_COPY_PLAIN`: copies of title memory in place, the kernel's
//!   copy behind them for a missing page (submit thread's kernel time
//!   44 -> 20 ms a frame);
//! - `VICTORY_JOIN_PARTS`: parts of one submission go to the host as one
//!   (about 6 %);
//! - `VICTORY_BACK_OWN_VIEW`: what shaders wrote goes back through a view
//!   of our own, so the copy that wrote it is not read in again (about
//!   9 %; 0 stores astray in 1.36 million checked);
//! - `VICTORY_CARD_REHASH_RUNS`: one hash pass per run of touching pieces
//!   (run 9 had up to 445,000 passes a frame);
//! - `VICTORY_WIDE_MERGE`: what shaders wrote to copies in the card's
//!   memory merged on several threads (now also on the copy-back thread,
//!   where run 9 spent about 270 ms a frame);
//! - `VICTORY_RECHECK_SOON`: a texture picked by index found changed is
//!   looked at again the next frame, not four frames later (on by default
//!   in the AMD sibling repository).
//!
//! Each still takes its own `=0` (or `=1`); `VICTORY_FAST=0` puts every
//! one of them back to off, as in runs 1-9, in one go.

use std::sync::OnceLock;

/// The switches [`on`] gives a default to, for the start-up line.
const SWITCHES: [&str; 8] = [
    "VICTORY_ASYNC_SUBMIT",
    "VICTORY_QUEUE2",
    "VICTORY_COPY_PLAIN",
    "VICTORY_JOIN_PARTS",
    "VICTORY_BACK_OWN_VIEW",
    "VICTORY_CARD_REHASH_RUNS",
    "VICTORY_WIDE_MERGE",
    "VICTORY_RECHECK_SOON",
];

/// Whether the faster defaults hold (`VICTORY_FAST`, unset or anything but
/// `0`).
pub(crate) fn fast() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let on = std::env::var_os("VICTORY_FAST").is_none_or(|v| v != "0");
        let set: Vec<String> = SWITCHES
            .iter()
            .filter_map(|s| std::env::var(s).ok().map(|v| format!("{s}={v}")))
            .collect();
        tracing::info!(
            "faster defaults (VICTORY_FAST): {}; set by hand: {}",
            if on {
                "ON (=0 puts them all back to off)"
            } else {
                "off, as in runs 1-9"
            },
            if set.is_empty() {
                "none".to_owned()
            } else {
                set.join(" ")
            }
        );
        on
    })
}

/// A plain on/off switch of the set: its own value when set (`0` or empty
/// off, anything else on), else [`fast`].
pub(crate) fn on(var: &str) -> bool {
    decide(
        std::env::var_os(var).as_deref().and_then(|v| v.to_str()),
        fast,
    )
}

fn decide(value: Option<&str>, fast: impl FnOnce() -> bool) -> bool {
    match value {
        Some(v) => !v.is_empty() && v != "0",
        None => fast(),
    }
}

#[cfg(test)]
mod tests {
    use super::decide;

    #[test]
    fn a_switch_set_by_hand_wins_over_the_set() {
        assert!(decide(Some("1"), || false));
        assert!(!decide(Some("0"), || true));
        assert!(!decide(Some(""), || true));
        assert!(decide(None, || true));
        assert!(!decide(None, || false));
    }
}
