//! Dirty-range tracking used to upload only the parts of a buffer that changed.

use std::ops::Range;

/// Above this many disjoint ranges the two closest neighbours are merged: a bounded number of
/// slightly larger copies is cheaper than hundreds of tiny ones.
const MAX_RANGES: usize = 64;

/// Sorted, non-overlapping set of dirty element ranges with coalescing.
///
/// Adjacent (or overlapping) ranges are merged on insert; ranges separated by at most
/// `merge_gap` clean elements are merged too, trading a few redundant bytes for fewer copies.
#[derive(Clone, Debug, Default)]
pub struct DirtyRanges {
    ranges: Vec<Range<usize>>,
    merge_gap: usize,
}

impl DirtyRanges {
    /// Empty set that merges ranges separated by `merge_gap` or fewer clean elements.
    pub fn with_merge_gap(merge_gap: usize) -> Self {
        Self {
            ranges: Vec::new(),
            merge_gap,
        }
    }

    /// Mark `range` dirty.
    pub fn mark(&mut self, range: Range<usize>) {
        if range.is_empty() {
            return;
        }
        let gap = self.merge_gap;
        // Fast path: appending after (or touching) the last range — the common case for
        // sequential writes and pushes.
        if let Some(last) = self.ranges.last_mut() {
            if range.start >= last.start {
                if range.start <= last.end + gap {
                    last.end = last.end.max(range.end);
                    return;
                }
                self.ranges.push(range);
                self.collapse_if_fragmented();
                return;
            }
        } else {
            self.ranges.push(range);
            return;
        }
        // General path: find first range whose (end + gap) >= start.
        let first = self.ranges.partition_point(|r| r.end + gap < range.start);
        let mut new = range;
        let mut last = first;
        while last < self.ranges.len() && self.ranges[last].start <= new.end + gap {
            new.start = new.start.min(self.ranges[last].start);
            new.end = new.end.max(self.ranges[last].end);
            last += 1;
        }
        self.ranges.splice(first..last, std::iter::once(new));
        self.collapse_if_fragmented();
    }

    /// Mark a single element dirty.
    pub fn mark_one(&mut self, index: usize) {
        self.mark(index..index + 1);
    }

    fn collapse_if_fragmented(&mut self) {
        while self.ranges.len() > MAX_RANGES {
            // Merge the adjacent pair separated by the smallest clean gap (least waste).
            let i = (0..self.ranges.len() - 1)
                .min_by_key(|&i| self.ranges[i + 1].start - self.ranges[i].end)
                .expect("at least two ranges");
            self.ranges[i].end = self.ranges[i + 1].end;
            self.ranges.remove(i + 1);
        }
    }

    /// Drop all ranges that extend past `len` (after a truncate).
    pub fn clamp(&mut self, len: usize) {
        self.ranges.retain_mut(|r| {
            r.end = r.end.min(len);
            r.start < r.end
        });
    }

    /// Forget everything (after an upload).
    pub fn clear(&mut self) {
        self.ranges.clear();
    }

    /// No dirty elements.
    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    /// Total number of dirty elements.
    pub fn total(&self) -> usize {
        self.ranges.iter().map(|r| r.end - r.start).sum()
    }

    /// The coalesced dirty ranges, sorted by start.
    pub fn ranges(&self) -> &[Range<usize>] {
        &self.ranges
    }

    /// Smallest range covering everything dirty.
    pub fn bounds(&self) -> Option<Range<usize>> {
        Some(self.ranges.first()?.start..self.ranges.last()?.end)
    }
}

#[cfg(test)]
#[allow(clippy::single_range_in_vec_init)]
mod tests {
    use super::*;

    #[test]
    fn coalesces_adjacent_and_overlapping() {
        let mut d = DirtyRanges::default();
        d.mark(0..2);
        d.mark(2..4);
        d.mark(10..12);
        d.mark(3..11);
        assert_eq!(d.ranges(), &[0..12]);
        assert_eq!(d.total(), 12);
    }

    #[test]
    fn out_of_order_inserts_stay_sorted() {
        let mut d = DirtyRanges::default();
        d.mark(50..51);
        d.mark(10..11);
        d.mark(30..31);
        d.mark(11..12);
        assert_eq!(d.ranges(), &[10..12, 30..31, 50..51]);
        d.mark(0..100);
        assert_eq!(d.ranges(), &[0..100]);
    }

    #[test]
    fn merge_gap_and_clamp() {
        let mut d = DirtyRanges::with_merge_gap(2);
        d.mark(0..1);
        d.mark(3..4);
        d.mark(10..11);
        assert_eq!(d.ranges(), &[0..4, 10..11]);
        d.clamp(3);
        assert_eq!(d.ranges(), &[0..3]);
        d.clear();
        assert!(d.is_empty());
    }

    #[test]
    fn fragmentation_merges_closest_neighbours() {
        let mut d = DirtyRanges::default();
        for i in 0..MAX_RANGES {
            d.mark_one(i * 1000);
        }
        // One extra range right next to an existing one: only that pair merges.
        d.mark_one(5003);
        assert_eq!(d.ranges().len(), MAX_RANGES);
        assert_eq!(d.total(), MAX_RANGES + 3);
        assert_eq!(d.bounds(), Some(0..(MAX_RANGES - 1) * 1000 + 1));
    }
}
