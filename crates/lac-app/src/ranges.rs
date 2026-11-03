//! Helpers on lists of sample ranges.

use std::ops::Range;

use lac_core::SamplePos;

/// Sorts `ranges` by start and merges overlapping or touching ones.
///
/// Empty ranges are dropped, so the result is a sorted list of disjoint,
/// non-adjacent, non-empty ranges covering exactly the union of the input.
pub fn normalize(ranges: &mut Vec<Range<SamplePos>>) {
    ranges.retain(|r| r.start < r.end);
    ranges.sort_by_key(|r| r.start);
    // `dedup_by` hands us (later, earlier): fold `b` into `a` if they meet.
    ranges.dedup_by(|b, a| {
        if b.start <= a.end {
            a.end = a.end.max(b.end);
            true
        } else {
            false
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_overlapping_and_touching() {
        let mut r = vec![0..10, 5..20, 20..25, 30..40];
        normalize(&mut r);
        assert_eq!(r, vec![0..25, 30..40]);
    }

    #[test]
    fn sorts_and_drops_empty() {
        let mut r = vec![30..40, 7..7, 0..10, 12..15, 3..4];
        normalize(&mut r);
        assert_eq!(r, vec![0..10, 12..15, 30..40]);
    }

    #[test]
    fn contained_range_disappears() {
        let mut r = vec![0..100, 10..20];
        normalize(&mut r);
        assert_eq!(r, vec![0..100]);
    }
}
