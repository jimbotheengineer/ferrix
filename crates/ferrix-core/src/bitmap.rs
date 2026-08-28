//! A compact validity bitmap: one bit per cell, `true` == value present.
//!
//! At 10M rows this costs 1.25 MB per column instead of the 10 MB an
//! `Option<f64>` discriminant byte-array would burn, and it lets us answer
//! "is this whole run empty?" with word-at-a-time scans.

/// Bits packed into u64 words, LSB-first within each word.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Bitmap {
    words: Vec<u64>,
    len: usize,
    /// Number of set bits. Maintained incrementally so `count_set` is O(1).
    set_count: usize,
}

impl Bitmap {
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    /// A bitmap of `len` bits, all cleared.
    pub fn zeros(len: usize) -> Self {
        Self {
            words: vec![0; Self::words_for(len)],
            len,
            set_count: 0,
        }
    }

    /// A bitmap of `len` bits, all set.
    pub fn ones(len: usize) -> Self {
        let mut words = vec![u64::MAX; Self::words_for(len)];
        Self::mask_tail(&mut words, len);
        Self {
            words,
            len,
            set_count: len,
        }
    }

    pub fn with_capacity(cap: usize) -> Self {
        Self {
            words: Vec::with_capacity(Self::words_for(cap)),
            len: 0,
            set_count: 0,
        }
    }

    #[inline]
    const fn words_for(bits: usize) -> usize {
        bits.div_ceil(64)
    }

    /// Zero out the unused high bits of the final word so that word-wise
    /// operations (popcount, all-set checks) never see garbage.
    fn mask_tail(words: &mut [u64], len: usize) {
        let rem = len % 64;
        if rem != 0 {
            if let Some(last) = words.last_mut() {
                *last &= (1u64 << rem) - 1;
            }
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Number of bits set. O(1).
    #[inline]
    pub fn count_set(&self) -> usize {
        self.set_count
    }

    #[inline]
    pub fn count_unset(&self) -> usize {
        self.len - self.set_count
    }

    #[inline]
    pub fn get(&self, index: usize) -> bool {
        if index >= self.len {
            return false;
        }
        // SAFETY-free path: index < len implies word index is in bounds.
        (self.words[index >> 6] >> (index & 63)) & 1 == 1
    }

    /// Set or clear a single bit, keeping `set_count` accurate.
    pub fn set(&mut self, index: usize, value: bool) {
        debug_assert!(
            index < self.len,
            "bitmap index {index} out of range {}",
            self.len
        );
        let word = &mut self.words[index >> 6];
        let mask = 1u64 << (index & 63);
        let was_set = *word & mask != 0;
        if value {
            *word |= mask;
            if !was_set {
                self.set_count += 1;
            }
        } else {
            *word &= !mask;
            if was_set {
                self.set_count -= 1;
            }
        }
    }

    /// Append one bit.
    pub fn push(&mut self, value: bool) {
        if self.len % 64 == 0 {
            self.words.push(0);
        }
        let index = self.len;
        self.len += 1;
        if value {
            self.words[index >> 6] |= 1u64 << (index & 63);
            self.set_count += 1;
        }
    }

    /// Grow to `new_len`, filling new bits with `value`.
    pub fn resize(&mut self, new_len: usize, value: bool) {
        if new_len <= self.len {
            self.truncate(new_len);
            return;
        }
        if value {
            let old_len = self.len;
            self.words.resize(Self::words_for(new_len), 0);
            self.len = new_len;
            // Set bits [old_len, new_len): partial word, whole words, partial word.
            let mut i = old_len;
            while i < new_len && i % 64 != 0 {
                self.words[i >> 6] |= 1u64 << (i & 63);
                i += 1;
            }
            while i + 64 <= new_len {
                self.words[i >> 6] = u64::MAX;
                i += 64;
            }
            while i < new_len {
                self.words[i >> 6] |= 1u64 << (i & 63);
                i += 1;
            }
            self.set_count += new_len - old_len;
        } else {
            self.words.resize(Self::words_for(new_len), 0);
            self.len = new_len;
        }
    }

    pub fn truncate(&mut self, new_len: usize) {
        if new_len >= self.len {
            return;
        }
        // Recount only the bits we are dropping.
        let mut dropped = 0usize;
        for i in new_len..self.len {
            if (self.words[i >> 6] >> (i & 63)) & 1 == 1 {
                dropped += 1;
            }
        }
        self.set_count -= dropped;
        self.len = new_len;
        self.words.truncate(Self::words_for(new_len));
        Self::mask_tail(&mut self.words, new_len);
    }

    /// Number of set bits in `[start, end)`.
    ///
    /// Word-at-a-time via `count_ones`, so a filter's rank index over 200M
    /// rows is built at memory bandwidth rather than one bit at a time.
    pub fn count_range(&self, start: usize, end: usize) -> usize {
        let end = end.min(self.len);
        if start >= end {
            return 0;
        }
        let (w0, w1) = (start >> 6, (end - 1) >> 6);
        if w0 == w1 {
            let width = end - start;
            let mask = if width == 64 {
                u64::MAX
            } else {
                ((1u64 << width) - 1) << (start & 63)
            };
            return (self.words[w0] & mask).count_ones() as usize;
        }
        // Leading partial word.
        let mut n = (self.words[w0] & (!0u64 << (start & 63))).count_ones() as usize;
        // Whole words in between.
        for w in &self.words[w0 + 1..w1] {
            n += w.count_ones() as usize;
        }
        // Trailing partial word.
        let rem = end & 63;
        let tail = if rem == 0 {
            self.words[w1]
        } else {
            self.words[w1] & ((1u64 << rem) - 1)
        };
        n + tail.count_ones() as usize
    }

    /// True when every bit in `[start, end)` is clear. Used by the renderer to
    /// skip entirely-blank row bands without touching individual cells.
    pub fn is_range_empty(&self, start: usize, end: usize) -> bool {
        let end = end.min(self.len);
        if start >= end {
            return true;
        }
        let mut i = start;
        // Leading partial word.
        while i < end && i % 64 != 0 {
            if self.get(i) {
                return false;
            }
            i += 1;
        }
        // Whole words.
        while i + 64 <= end {
            if self.words[i >> 6] != 0 {
                return false;
            }
            i += 64;
        }
        // Trailing partial word.
        while i < end {
            if self.get(i) {
                return false;
            }
            i += 1;
        }
        true
    }

    /// Append every bit of `other`, word-aligned when possible.
    pub fn extend_from(&mut self, other: &Bitmap) {
        if other.len == 0 {
            return;
        }
        if self.len % 64 == 0 {
            // Fast path: we are word-aligned, so words concatenate directly.
            self.words.truncate(Self::words_for(self.len));
            self.words.extend_from_slice(&other.words);
            self.len += other.len;
            self.set_count += other.set_count;
            Self::mask_tail(&mut self.words, self.len);
        } else {
            for i in 0..other.len {
                self.push(other.get(i));
            }
        }
    }

    pub fn clear(&mut self) {
        self.words.clear();
        self.len = 0;
        self.set_count = 0;
    }

    /// Iterate the indices of set bits, skipping empty words wholesale.
    pub fn iter_set(&self) -> impl Iterator<Item = usize> + '_ {
        self.words.iter().enumerate().flat_map(move |(w, &word)| {
            let base = w * 64;
            let mut bits = word;
            std::iter::from_fn(move || {
                if bits == 0 {
                    return None;
                }
                let tz = bits.trailing_zeros() as usize;
                bits &= bits - 1; // clear lowest set bit
                Some(base + tz)
            })
        })
    }

    /// Approximate heap footprint in bytes.
    pub fn heap_bytes(&self) -> usize {
        self.words.capacity() * 8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_get_roundtrip() {
        let mut b = Bitmap::new();
        let pattern: Vec<bool> = (0..200).map(|i| i % 3 == 0).collect();
        for &v in &pattern {
            b.push(v);
        }
        assert_eq!(b.len(), 200);
        for (i, &v) in pattern.iter().enumerate() {
            assert_eq!(b.get(i), v, "bit {i}");
        }
        assert_eq!(b.count_set(), pattern.iter().filter(|v| **v).count());
    }

    #[test]
    fn set_maintains_count() {
        let mut b = Bitmap::zeros(100);
        assert_eq!(b.count_set(), 0);
        b.set(5, true);
        b.set(5, true); // idempotent
        assert_eq!(b.count_set(), 1);
        b.set(5, false);
        assert_eq!(b.count_set(), 0);
        assert_eq!(b.count_unset(), 100);
    }

    #[test]
    fn ones_masks_tail() {
        let b = Bitmap::ones(70);
        assert_eq!(b.count_set(), 70);
        assert!(b.get(69));
        assert!(!b.get(70));
    }

    #[test]
    fn range_empty_detection() {
        let mut b = Bitmap::zeros(500);
        assert!(b.is_range_empty(0, 500));
        b.set(300, true);
        assert!(b.is_range_empty(0, 300));
        assert!(!b.is_range_empty(0, 301));
        assert!(!b.is_range_empty(300, 301));
        assert!(b.is_range_empty(301, 500));
    }

    #[test]
    fn extend_aligned_and_unaligned() {
        for prefix in [64usize, 65] {
            let mut a = Bitmap::zeros(prefix);
            a.set(1, true);
            let mut other = Bitmap::new();
            for i in 0..100 {
                other.push(i % 2 == 0);
            }
            let expected_set = a.count_set() + other.count_set();
            a.extend_from(&other);
            assert_eq!(a.len(), prefix + 100);
            assert_eq!(a.count_set(), expected_set, "prefix {prefix}");
            for i in 0..100 {
                assert_eq!(a.get(prefix + i), i % 2 == 0, "prefix {prefix} bit {i}");
            }
        }
    }

    #[test]
    fn resize_grows_and_shrinks() {
        let mut b = Bitmap::zeros(10);
        b.resize(200, true);
        assert_eq!(b.len(), 200);
        assert_eq!(b.count_set(), 190);
        b.truncate(12);
        assert_eq!(b.len(), 12);
        assert_eq!(b.count_set(), 2);
    }

    #[test]
    fn iter_set_matches_get() {
        let mut b = Bitmap::zeros(300);
        for i in [0usize, 63, 64, 130, 299] {
            b.set(i, true);
        }
        let got: Vec<usize> = b.iter_set().collect();
        assert_eq!(got, vec![0, 63, 64, 130, 299]);
    }
}
