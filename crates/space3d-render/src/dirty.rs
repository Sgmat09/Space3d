//! Dirty-range tracking used to upload only the parts of a buffer that changed.

use std::ops::Range;

/// Above this many disjoint ranges the two closest neighbours are merged: a bounded number of
/// slightly larger copies is cheaper than hundreds of tiny ones.
const MAX_RANGES: usize = 64;

/// Sorted, non-overlapping set of dirty element ranges with coalescing.
///
/// Adjacent (or overlapping) ranges are merged on insert; ranges separated by at most
/// `merge_gap` clean elements are merged too, trading a few redundant bytes for fewer copies.
/// At most 64 ranges are kept: once the set is saturated, a new range is folded
/// into its nearest neighbour in O(1) when it is no farther from it than the set's average
/// spacing (strided and scattered edits), and only otherwise pays for merging the closest
/// pair of ranges.
#[derive(Clone, Debug, Default)]
pub struct DirtyRanges {
    ranges: Vec<Range<usize>>,
    merge_gap: usize,
    /// Scratch for [`Self::subtract`] (kept to avoid per-frame allocation).
    spare: Vec<Range<usize>>,
}

impl DirtyRanges {
    /// Empty set that merges ranges separated by `merge_gap` or fewer clean elements.
    pub fn with_merge_gap(merge_gap: usize) -> Self {
        Self {
            ranges: Vec::new(),
            merge_gap,
            spare: Vec::new(),
        }
    }

    /// Average distance between range starts once saturated: folding a new range into a
    /// neighbour at most this far away wastes no more than merging a typical pair would.
    fn typical_spacing(&self) -> usize {
        match (self.ranges.first(), self.ranges.last()) {
            (Some(a), Some(b)) => (b.end - a.start) / self.ranges.len(),
            _ => 0,
        }
    }

    /// Mark `range` dirty.
    pub fn mark(&mut self, range: Range<usize>) {
        if range.is_empty() {
            return;
        }
        let gap = self.merge_gap;
        let saturated = self.ranges.len() >= MAX_RANGES;
        let typical = if saturated { self.typical_spacing() } else { 0 };
        // Fast path: appending after (or touching) the last range — the common case for
        // sequential writes and pushes.
        let Some(last) = self.ranges.last_mut() else {
            self.ranges.push(range);
            return;
        };
        if range.start >= last.start {
            if range.start <= last.end + gap {
                last.end = last.end.max(range.end);
                return;
            }
            if saturated && range.start - last.end <= typical {
                last.end = range.end;
                return;
            }
            self.ranges.push(range);
            self.collapse_if_fragmented();
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
        if saturated && last == first {
            // Would add a range: widen the nearer neighbour instead when it is close enough.
            // (`first < len` here: a range past the last one takes the append path above.)
            let right = self.ranges[first].start - new.end;
            let left = first
                .checked_sub(1)
                .map_or(usize::MAX, |l| new.start - self.ranges[l].end);
            if left.min(right) <= typical {
                if left <= right {
                    self.ranges[first - 1].end = new.end;
                } else {
                    self.ranges[first].start = new.start;
                }
                return;
            }
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

    /// Remove every element covered by `remove` (sorted, non-overlapping), keeping the rest
    /// dirty — e.g. after uploading only part of the dirty data.
    pub(crate) fn subtract(&mut self, remove: &[Range<usize>]) {
        if remove.is_empty() || self.ranges.is_empty() {
            return;
        }
        self.spare.clear();
        let mut j = 0;
        for r in &self.ranges {
            // Skip removals that end before this range.
            while j < remove.len() && remove[j].end <= r.start {
                j += 1;
            }
            let mut start = r.start;
            let mut k = j;
            while start < r.end {
                match remove.get(k) {
                    Some(rm) if rm.start < r.end => {
                        if rm.start > start {
                            self.spare.push(start..rm.start);
                        }
                        start = start.max(rm.end);
                        k += 1;
                    }
                    _ => {
                        self.spare.push(start..r.end);
                        break;
                    }
                }
            }
        }
        std::mem::swap(&mut self.ranges, &mut self.spare);
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
        // A far-away range is not folded into its neighbour: the closest pair merges instead.
        d.mark_one(1_000_000);
        assert_eq!(d.ranges().len(), MAX_RANGES);
        assert_eq!(d.total(), MAX_RANGES + 1000);
        assert_eq!(d.ranges().last(), Some(&(1_000_000..1_000_001)));
        assert_eq!(d.ranges()[0], 0..1001);
    }

    fn check_invariants(d: &DirtyRanges, marked: &[usize]) {
        assert!(d.ranges().len() <= MAX_RANGES);
        for w in d.ranges().windows(2) {
            assert!(w[0].end < w[1].start, "sorted and disjoint: {:?}", d.ranges());
        }
        for &m in marked {
            assert!(d.ranges().iter().any(|r| r.contains(&m)), "{m} lost");
        }
    }

    /// Regression: once 64 ranges existed, every further scattered mark scanned all gaps and
    /// shifted the vector (O(MAX_RANGES) per mark). Strided marks now extend the last range in
    /// O(1) and coalesce the same way.
    #[test]
    fn saturated_strided_marks_are_constant_time_and_cover_everything() {
        for descending in [false, true] {
            let mut d = DirtyRanges::with_merge_gap(16);
            let mut marked = Vec::new();
            let idx: Vec<usize> = (0..50_000).map(|k| k * 20).collect();
            let order: Vec<usize> = if descending {
                idx.iter().rev().copied().collect()
            } else {
                idx.clone()
            };
            let t = std::time::Instant::now();
            for &i in &order {
                d.mark_one(i);
                marked.push(i);
            }
            let elapsed = t.elapsed();
            check_invariants(&d, &marked);
            // Nearly everything is dirty either way; the caller then uploads it in one copy.
            assert!(d.total() > 990_000, "{descending}: {}", d.total());
            // Generous bound (debug build, loaded machine): the old O(64) path took ~2.5 ms in
            // release and far longer in debug.
            assert!(elapsed.as_millis() < 250, "{descending}: {elapsed:?}");
        }
    }

    #[test]
    fn saturated_random_marks_stay_valid() {
        let mut d = DirtyRanges::with_merge_gap(16);
        let mut seed = 12345u64;
        let mut marked = Vec::new();
        for _ in 0..5000 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let i = (seed >> 33) as usize % 1_000_000;
            d.mark(i..i + 1 + (seed as usize % 4));
            marked.push(i);
            check_invariants(&d, &marked[marked.len().saturating_sub(64)..]);
        }
        check_invariants(&d, &marked);
    }

    #[test]
    fn subtract_keeps_the_uncovered_parts() {
        let mut d = DirtyRanges::default();
        d.mark(0..10);
        d.mark(20..30);
        d.mark(40..50);
        d.subtract(&[2..4, 8..22, 25..26, 45..100]);
        assert_eq!(d.ranges(), &[0..2, 4..8, 22..25, 26..30, 40..45]);
        d.subtract(&[0..1000]);
        assert!(d.is_empty());
        d.mark(5..6);
        d.subtract(&[]);
        assert_eq!(d.ranges(), &[5..6]);
    }
}
