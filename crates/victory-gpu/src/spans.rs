//! A set of numbers kept as runs (`start..end`), for the write-watch
//! guards' clean blocks: a frame asks about some 150,000 blocks, nearly
//! all clean and in long runs, so whether a range is all clean is one
//! lookup rather than a walk over every block of it (run 18: the guards'
//! lock held 19 ms a frame at that walk, the write-fault handler waiting
//! behind it 66 ms a frame across threads).

use std::collections::BTreeMap;
use std::ops::Range;

/// Disjoint, non-adjacent runs: start -> end (exclusive).
#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub(crate) struct Spans {
    runs: BTreeMap<usize, usize>,
}

impl Spans {
    pub(crate) const fn new() -> Spans {
        Spans { runs: BTreeMap::new() }
    }

    /// The run holding `n`, if any.
    fn run_of(&self, n: usize) -> Option<(usize, usize)> {
        self.runs
            .range(..=n)
            .next_back()
            .filter(|&(_, &end)| n < end)
            .map(|(&start, &end)| (start, end))
    }

    pub(crate) fn contains(&self, n: &usize) -> bool {
        self.run_of(*n).is_some()
    }

    /// Whether every number of `range` is in the set (an empty range is).
    pub(crate) fn covers(&self, range: Range<usize>) -> bool {
        range.is_empty() || self.run_of(range.start).is_some_and(|(_, end)| range.end <= end)
    }

    pub(crate) fn insert(&mut self, n: usize) {
        self.extend(n..n + 1);
    }

    /// Adds every number of `range`, joining the runs it touches.
    pub(crate) fn extend(&mut self, range: Range<usize>) {
        if range.is_empty() {
            return;
        }
        let (mut start, mut end) = (range.start, range.end);
        // A run ending at or after `start` that begins at or before it.
        if let Some((&s, &e)) = self.runs.range(..=start).next_back()
            && e >= start
        {
            start = s;
            end = end.max(e);
        }
        // Runs beginning inside or right after the new one.
        let swallowed: Vec<(usize, usize)> = self
            .runs
            .range(start..=end)
            .map(|(&s, &e)| (s, e))
            .collect();
        for (s, e) in swallowed {
            self.runs.remove(&s);
            end = end.max(e);
        }
        self.runs.insert(start, end);
    }

    /// Takes `n` out; true if it was in.
    pub(crate) fn remove(&mut self, n: &usize) -> bool {
        let n = *n;
        let Some((start, end)) = self.run_of(n) else {
            return false;
        };
        self.runs.remove(&start);
        if start < n {
            self.runs.insert(start, n);
        }
        if n + 1 < end {
            self.runs.insert(n + 1, end);
        }
        true
    }

    /// The runs that meet `range`, cut to it, in order.
    pub(crate) fn runs_in(&self, range: Range<usize>) -> Vec<Range<usize>> {
        if range.is_empty() {
            return Vec::new();
        }
        let first = self.run_of(range.start).map(|(s, _)| s).unwrap_or(range.start);
        self.runs
            .range(first..range.end)
            .map(|(&s, &e)| s.max(range.start)..e.min(range.end))
            .filter(|r| !r.is_empty())
            .collect()
    }

    /// How many numbers of `range` are in the set.
    #[cfg(test)]
    pub(crate) fn count_in(&self, range: Range<usize>) -> usize {
        self.runs_in(range).iter().map(|r| r.len()).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// Random inserts, extends and removes against a plain set: every
    /// answer the same, and the runs always disjoint and apart.
    #[test]
    fn spans_answer_as_a_plain_set_does() {
        let mut seed = 0x2545_f491_4f6c_dd1d_u64;
        let mut next = |m: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % m as u64) as usize
        };
        let mut spans = Spans::new();
        let mut plain = BTreeSet::new();
        for _ in 0..20_000 {
            match next(4) {
                0 => {
                    let n = next(300);
                    spans.insert(n);
                    plain.insert(n);
                }
                1 => {
                    let (a, len) = (next(300), next(40));
                    spans.extend(a..a + len);
                    plain.extend(a..a + len);
                }
                _ => {
                    let n = next(300);
                    assert_eq!(spans.remove(&n), plain.remove(&n));
                }
            }
            let (a, len) = (next(320), next(60));
            let r = a..a + len;
            assert_eq!(spans.covers(r.clone()), plain.range(r.clone()).count() == len);
            assert_eq!(spans.count_in(r.clone()), plain.range(r.clone()).count());
            let listed: Vec<usize> = spans.runs_in(r.clone()).into_iter().flatten().collect();
            let want: Vec<usize> = plain.range(r).copied().collect();
            assert_eq!(listed, want);
            assert_eq!(spans.contains(&a), plain.contains(&a));
            let mut last_end = None;
            for (&s, &e) in &spans.runs {
                assert!(s < e, "no empty run");
                if let Some(prev) = last_end {
                    assert!(prev < s, "runs apart, not touching");
                }
                last_end = Some(e);
            }
        }
    }

    #[test]
    fn a_long_clean_range_is_one_run() {
        let mut spans = Spans::new();
        spans.extend(1_000..151_000);
        assert!(spans.covers(1_000..151_000));
        assert!(spans.covers(5_000..5_000));
        assert!(!spans.covers(999..1_001));
        assert_eq!(spans.runs.len(), 1);
        spans.remove(&70_000);
        assert!(!spans.covers(1_000..151_000));
        assert_eq!(spans.runs_in(69_998..70_003), [69_998..70_000, 70_001..70_003]);
        spans.insert(70_000);
        assert_eq!(spans.runs.len(), 1);
    }
}
