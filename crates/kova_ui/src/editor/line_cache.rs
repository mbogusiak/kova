//! Bounded $O(\text{viewport})$ cache for shaped editor lines.
//!
//! Invalidates in $O(1)$ whenever buffer revision, theme revision, or font size changes,
//! and bounds memory strictly by `capacity` entries regardless of file size.

use std::collections::{HashMap, VecDeque};

/// Default maximum number of cached shaped lines (`O(viewport)` with scroll headroom).
pub const DEFAULT_LINE_CACHE_CAPACITY: usize = 512;

/// Cache key identifying a line and its horizontal byte slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LineCacheKey {
    /// 0-based line index in the buffer.
    pub line_idx: usize,
    /// Inclusive start byte offset within the line.
    pub byte_start: usize,
    /// Exclusive end byte offset within the line.
    pub byte_end: usize,
}

impl LineCacheKey {
    /// Creates a new [`LineCacheKey`].
    #[must_use]
    pub const fn new(line_idx: usize, byte_start: usize, byte_end: usize) -> Self {
        Self {
            line_idx,
            byte_start,
            byte_end,
        }
    }
}

/// Bounded FIFO/LRU-style cache of shaped lines keyed by [`LineCacheKey`].
///
/// Generic over the cached value `V` (in production `gpui::ShapedLine`) so all eviction
/// and invalidation invariants can be verified in headless unit tests without a GPU window.
#[derive(Debug)]
pub struct ShapedLineCache<V> {
    capacity: usize,
    buffer_version: u64,
    theme_revision: u64,
    font_size_bits: u32,
    entries: HashMap<LineCacheKey, V>,
    order: VecDeque<LineCacheKey>,
}

impl<V> Default for ShapedLineCache<V> {
    fn default() -> Self {
        Self::new(DEFAULT_LINE_CACHE_CAPACITY)
    }
}

impl<V> ShapedLineCache<V> {
    /// Creates an empty [`ShapedLineCache`] with the given maximum `capacity`.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            buffer_version: u64::MAX,
            theme_revision: u64::MAX,
            font_size_bits: 0,
            entries: HashMap::with_capacity(capacity.min(1024)),
            order: VecDeque::with_capacity(capacity.min(1024)),
        }
    }

    /// Returns the maximum number of entries retained in this cache.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// Returns the number of currently cached entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns `true` when the cache contains zero entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Synchronizes the cache with the active `(buffer_version, theme_revision, font_size_px)`.
    ///
    /// Clears all entries in $O(1)$ amortized time if any version or font size changed.
    pub fn sync_context(&mut self, buffer_version: u64, theme_revision: u64, font_size_px: f32) {
        let font_bits = font_size_px.to_bits();
        if self.buffer_version != buffer_version
            || self.theme_revision != theme_revision
            || self.font_size_bits != font_bits
        {
            self.entries.clear();
            self.order.clear();
            self.buffer_version = buffer_version;
            self.theme_revision = theme_revision;
            self.font_size_bits = font_bits;
        }
    }

    /// Looks up a cached value by `key`.
    #[must_use]
    pub fn get(&self, key: LineCacheKey) -> Option<&V> {
        self.entries.get(&key)
    }

    /// Inserts `value` under `key`, evicting the oldest entry if at capacity.
    pub fn insert(&mut self, key: LineCacheKey, value: V) {
        if self.capacity == 0 {
            return;
        }

        if !self.entries.contains_key(&key) {
            while self.entries.len() >= self.capacity {
                if let Some(oldest) = self.order.pop_front() {
                    self.entries.remove(&oldest);
                } else {
                    break;
                }
            }
            self.order.push_back(key);
        }

        self.entries.insert(key, value);
    }

    /// Removes all entries from the cache without changing capacity.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enforces_capacity_bound_during_long_scrolls() {
        let mut cache = ShapedLineCache::new(64);
        cache.sync_context(1, 0, 13.0);

        for line in 0..1_000 {
            cache.insert(LineCacheKey::new(line, 0, 42), line);
            assert!(cache.len() <= 64);
        }

        assert_eq!(cache.len(), 64);
        assert_eq!(cache.get(LineCacheKey::new(0, 0, 42)), None);
        assert_eq!(cache.get(LineCacheKey::new(999, 0, 42)), Some(&999));
    }

    #[test]
    fn invalidates_on_buffer_theme_or_font_size_change() {
        let mut cache = ShapedLineCache::new(16);
        cache.sync_context(1, 0, 13.0);
        cache.insert(LineCacheKey::new(0, 0, 10), "hello");
        assert_eq!(cache.len(), 1);

        // Same context retains entry.
        cache.sync_context(1, 0, 13.0);
        assert_eq!(cache.get(LineCacheKey::new(0, 0, 10)), Some(&"hello"));

        // Buffer version bump clears cache.
        cache.sync_context(2, 0, 13.0);
        assert!(cache.is_empty());

        cache.insert(LineCacheKey::new(0, 0, 10), "world");
        // Theme revision bump clears cache.
        cache.sync_context(2, 1, 13.0);
        assert!(cache.is_empty());

        cache.insert(LineCacheKey::new(0, 0, 10), "again");
        // Font size change clears cache.
        cache.sync_context(2, 1, 14.0);
        assert!(cache.is_empty());
    }
}
