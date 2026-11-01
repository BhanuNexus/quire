// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Per-line heights with prefix sums: scroll offset ↔ line in O(log n).

#[derive(Debug, Default, Clone)]
pub(super) struct HeightIndex {
    h: Vec<f32>,
    prefix: Vec<f32>,
    dirty: bool,
}

impl HeightIndex {
    pub(super) fn reset(&mut self, n: usize, estimate: impl Fn(usize) -> f32) {
        self.h = (0..n).map(estimate).collect();
        self.dirty = true;
    }

    pub(super) fn len(&self) -> usize {
        self.h.len()
    }

    /// Replace `old` lines at `start` with `new` lines of height `est`.
    pub(super) fn splice(&mut self, start: usize, old: usize, new: usize, est: f32) {
        let start = start.min(self.h.len());
        let end = (start + old).min(self.h.len());
        self.h.splice(start..end, std::iter::repeat_n(est, new));
        self.dirty = true;
    }

    /// Record a measured height. Returns the change.
    pub(super) fn set(&mut self, i: usize, v: f32) -> f32 {
        match self.h.get_mut(i) {
            Some(x) if (*x - v).abs() > 0.25 => {
                let d = v - *x;
                *x = v;
                self.dirty = true;
                d
            }
            _ => 0.0,
        }
    }

    fn ensure(&mut self) {
        if self.dirty || self.prefix.len() != self.h.len() + 1 {
            self.prefix.clear();
            self.prefix.reserve(self.h.len() + 1);
            let mut acc = 0.0;
            self.prefix.push(0.0);
            for &x in &self.h {
                acc += x;
                self.prefix.push(acc);
            }
            self.dirty = false;
        }
    }

    pub(super) fn total(&mut self) -> f32 {
        self.ensure();
        *self.prefix.last().unwrap_or(&0.0)
    }

    /// Top of line `i`.
    pub(super) fn top(&mut self, i: usize) -> f32 {
        self.ensure();
        self.prefix[i.min(self.h.len())]
    }

    /// Line containing offset `y` (clamped to the last line).
    pub(super) fn line_at(&mut self, y: f32) -> usize {
        self.ensure();
        if self.h.is_empty() {
            return 0;
        }
        let i = self.prefix.partition_point(|&p| p <= y);
        i.saturating_sub(1).min(self.h.len() - 1)
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp, reason = "sums of small integers are exact in f32")]
mod tests {
    use super::*;

    #[test]
    fn maps_offsets_and_splices() {
        let mut h = HeightIndex::default();
        h.reset(4, |i| if i == 1 { 30.0 } else { 10.0 });
        assert_eq!(h.total(), 60.0);
        assert_eq!(h.top(2), 40.0);
        assert_eq!(h.line_at(0.0), 0);
        assert_eq!(h.line_at(39.9), 1);
        assert_eq!(h.line_at(40.0), 2);
        assert_eq!(h.line_at(1e9), 3);
        h.splice(1, 2, 3, 5.0);
        assert_eq!(h.len(), 5);
        assert_eq!(h.total(), 10.0 + 15.0 + 10.0);
        assert_eq!(h.set(0, 12.0), 2.0);
        assert_eq!(h.top(1), 12.0);
    }
}
