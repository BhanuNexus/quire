// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Ink: hand-drawn strokes stored as SVG (`attachments/<canvas>-ink-<n>.svg`).
//!
//! Each stroke is a `<path>` whose outline any SVG viewer draws, with the
//! input points kept in `data-quire-points="x,y,p;…"` and the tool in
//! `data-quire-tool`, so Quire can edit strokes later (docs/formats/canvas.md).
//! The outline itself (pressure-varying width) arrives with WS-G.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Pen,
    Marker,
    Highlighter,
}

impl Tool {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Pen => "pen",
            Self::Marker => "marker",
            Self::Highlighter => "highlighter",
        }
    }

    pub fn from_name(s: &str) -> Option<Self> {
        [Self::Pen, Self::Marker, Self::Highlighter].into_iter().find(|t| t.name() == s)
    }
}

/// An input point: position and pen pressure (0–1).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub x: f32,
    pub y: f32,
    pub pressure: f32,
}

/// `x,y,p;x,y,p…` with two decimals.
pub fn encode_points(points: &[Point]) -> String {
    points.iter().map(|p| format!("{:.2},{:.2},{:.2}", p.x, p.y, p.pressure)).collect::<Vec<_>>().join(";")
}

/// The inverse of [`encode_points`]; malformed points are skipped.
pub fn decode_points(s: &str) -> Vec<Point> {
    s.split(';')
        .filter_map(|t| {
            let mut it = t.split(',').map(|n| n.trim().parse::<f32>());
            Some(Point { x: it.next()?.ok()?, y: it.next()?.ok()?, pressure: it.next().and_then(Result::ok).unwrap_or(0.5) })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn points_round_trip() {
        let pts = [Point { x: 1.0, y: 2.5, pressure: 0.3 }, Point { x: -4.25, y: 0.0, pressure: 1.0 }];
        let s = encode_points(&pts);
        assert_eq!(s, "1.00,2.50,0.30;-4.25,0.00,1.00");
        assert_eq!(decode_points(&s), pts);
        assert_eq!(decode_points("1,2;bad;3,4,0.9").len(), 2, "pressure is optional, junk is skipped");
        assert_eq!(Tool::from_name("marker"), Some(Tool::Marker));
    }
}
