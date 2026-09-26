//! First-fit range allocator with coalescing over the particle pool (in slots), the same
//! algorithm as the renderer's mesh arena.

/// Free ranges `(start, len)` sorted by start, over `0..total`.
#[derive(Debug, Default)]
pub(crate) struct RangeAllocator {
    free: Vec<(u32, u32)>,
    total: u32,
    reserved: u32,
    high_water: u32,
}

impl RangeAllocator {
    /// Slots in use.
    pub fn reserved(&self) -> u32 {
        self.reserved
    }

    /// Size of the pool.
    pub fn total(&self) -> u32 {
        self.total
    }

    /// One past the highest slot ever handed out (the simulated part of the pool).
    pub fn high_water(&self) -> u32 {
        self.high_water
    }

    /// Grow the pool to `total` slots (never shrinks: live ranges keep their slots).
    pub fn grow_to(&mut self, total: u32) {
        if total <= self.total {
            return;
        }
        let added = (self.total, total - self.total);
        self.total = total;
        // Room for a fragmented pool from the start (1 KiB): the free list rarely grows later,
        // where a steady state would count the growth as an allocation.
        if self.free.capacity() == 0 {
            self.free.reserve(128);
        }
        self.release(added.0, added.1, false);
    }

    /// Shrink the pool toward `total` slots by cutting its free tail (live ranges keep their
    /// slots: a pool whose tail is still reserved shrinks as those ranges are freed, on a later
    /// call). The high water follows (the cut slots hold no live particles).
    pub fn shrink_to(&mut self, total: u32) {
        if total >= self.total {
            return;
        }
        let Some(&(start, len)) = self.free.last() else {
            return;
        };
        if start + len != self.total {
            return;
        }
        let new_total = total.max(start);
        if new_total == start {
            self.free.pop();
        } else {
            let last = self.free.len() - 1;
            self.free[last].1 = new_total - start;
        }
        self.total = new_total;
        self.high_water = self.high_water.min(new_total);
    }

    /// First fit of `len` slots.
    pub fn alloc(&mut self, len: u32) -> Option<u32> {
        if len == 0 {
            return None;
        }
        let i = self.free.iter().position(|&(_, l)| l >= len)?;
        let (start, l) = self.free[i];
        if l == len {
            self.free.remove(i);
        } else {
            self.free[i] = (start + len, l - len);
        }
        self.reserved += len;
        self.high_water = self.high_water.max(start + len);
        Some(start)
    }

    /// Return `start..start + len`.
    pub fn free(&mut self, start: u32, len: u32) {
        self.release(start, len, true);
    }

    fn release(&mut self, start: u32, len: u32, reserved: bool) {
        if len == 0 {
            return;
        }
        if reserved {
            self.reserved -= len;
        }
        let i = self.free.partition_point(|&(s, _)| s < start);
        let merge_prev = i > 0 && self.free[i - 1].0 + self.free[i - 1].1 == start;
        let merge_next = i < self.free.len() && start + len == self.free[i].0;
        match (merge_prev, merge_next) {
            (true, true) => {
                self.free[i - 1].1 += len + self.free[i].1;
                self.free.remove(i);
            }
            (true, false) => self.free[i - 1].1 += len,
            (false, true) => {
                self.free[i].0 = start;
                self.free[i].1 += len;
            }
            (false, false) => self.free.insert(i, (start, len)),
        }
    }

    /// Room for `ranges` free ranges (a pool of `n` live ranges has at most `n + 1`).
    pub fn reserve(&mut self, ranges: usize) {
        super::reserve_to(&mut self.free, ranges);
    }

    /// Capacity of the free list (reallocation tracking).
    pub fn capacity(&self) -> usize {
        self.free.capacity()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn first_fit_and_coalescing() {
        let mut a = RangeAllocator::default();
        assert_eq!(a.alloc(4), None, "empty pool");
        a.grow_to(100);
        let x = a.alloc(10).unwrap();
        let y = a.alloc(20).unwrap();
        let z = a.alloc(30).unwrap();
        assert_eq!((x, y, z), (0, 10, 30));
        assert_eq!((a.reserved(), a.high_water()), (60, 60));
        a.free(y, 20);
        assert_eq!(a.alloc(15), Some(10), "first fit reuses the hole");
        a.free(x, 10);
        a.free(10, 15);
        a.free(z, 30);
        assert_eq!(a.reserved(), 0);
        assert_eq!(a.free, vec![(0, 100)], "fully coalesced");
        assert_eq!(a.alloc(101), None);
        a.grow_to(150);
        assert_eq!(a.alloc(150), Some(0));
    }

    /// game#14: a lowered budget gives the free tail back; a reserved tail waits.
    #[test]
    fn shrink_cuts_the_free_tail() {
        let mut a = RangeAllocator::default();
        a.grow_to(1000);
        let x = a.alloc(100).unwrap();
        let y = a.alloc(300).unwrap();
        assert_eq!((a.total(), a.high_water()), (1000, 400));
        a.shrink_to(200);
        assert_eq!((a.total(), a.high_water()), (400, 400), "y is live up to 400");
        a.free(y, 300);
        a.shrink_to(200);
        assert_eq!((a.total(), a.high_water(), a.reserved()), (200, 200, 100));
        assert_eq!(a.alloc(100), Some(100));
        assert_eq!(a.alloc(1), None, "full at 200");
        a.free(x, 100);
        a.free(100, 100);
        a.shrink_to(0);
        assert_eq!((a.total(), a.free.len()), (0, 0));
        a.grow_to(64);
        assert_eq!(a.alloc(64), Some(0));
    }

    proptest! {
        /// Live ranges never overlap and stay inside the pool; freeing everything restores one
        /// free range.
        #[test]
        fn ranges_never_overlap(ops in proptest::collection::vec((1u32..40, any::<bool>()), 1..200)) {
            let mut a = RangeAllocator::default();
            a.grow_to(500);
            let mut live: Vec<(u32, u32)> = Vec::new();
            for (len, free) in ops {
                if free && !live.is_empty() {
                    let (s, l) = live.remove(len as usize % live.len());
                    a.free(s, l);
                } else if let Some(s) = a.alloc(len) {
                    prop_assert!(s + len <= 500);
                    for &(t, m) in &live {
                        prop_assert!(s + len <= t || t + m <= s, "overlap");
                    }
                    live.push((s, len));
                }
                prop_assert_eq!(a.reserved(), live.iter().map(|r| r.1).sum::<u32>());
            }
            for (s, l) in live {
                a.free(s, l);
            }
            prop_assert_eq!(a.free.clone(), vec![(0, 500)]);
        }
    }
}
