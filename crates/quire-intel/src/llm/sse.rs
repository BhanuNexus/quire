// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Server-sent events, read line by line from any `BufRead`.
//!
//! The providers stream their replies this way. Follows the WHATWG rules that matter here: `event:`
//! and `data:` fields, several `data:` lines joined with `\n`, comments
//! (`:`) skipped, an empty line ends an event, and CR, LF or CRLF endings.

use std::io::BufRead;

/// One event: its `event:` name (empty when not given) and its data.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SseEvent {
    pub event: String,
    pub data: String,
}

pub struct SseReader<R> {
    inner: R,
    line: Vec<u8>,
}

impl<R: BufRead> SseReader<R> {
    pub const fn new(inner: R) -> Self {
        Self { inner, line: Vec::new() }
    }

    /// The next event, or `None` at the end of the stream. An event cut off
    /// by the end of the stream is still returned if it has data.
    pub fn next_event(&mut self) -> std::io::Result<Option<SseEvent>> {
        let mut ev = SseEvent::default();
        let mut has_data = false;
        loop {
            self.line.clear();
            let n = self.inner.read_until(b'\n', &mut self.line)?;
            if n == 0 {
                return Ok(has_data.then_some(ev));
            }
            while matches!(self.line.last(), Some(b'\n' | b'\r')) {
                self.line.pop();
            }
            // A bare CR in the middle ends a line too (rare; old servers).
            let text = String::from_utf8_lossy(&self.line).into_owned();
            for line in text.split('\r') {
                if line.is_empty() {
                    if has_data || !ev.event.is_empty() {
                        if has_data {
                            return Ok(Some(ev));
                        }
                        ev = SseEvent::default();
                    }
                    continue;
                }
                if line.starts_with(':') {
                    continue;
                }
                let (field, value) = line.split_once(':').map_or((line, ""), |(f, v)| (f, v.strip_prefix(' ').unwrap_or(v)));
                match field {
                    "event" => value.clone_into(&mut ev.event),
                    "data" => {
                        if has_data {
                            ev.data.push('\n');
                        }
                        ev.data.push_str(value);
                        has_data = true;
                    }
                    _ => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all(s: &str) -> Vec<SseEvent> {
        let mut r = SseReader::new(s.as_bytes());
        let mut out = Vec::new();
        while let Some(e) = r.next_event().unwrap() {
            out.push(e);
        }
        out
    }

    #[test]
    fn reads_events_with_names_comments_and_multiline_data() {
        let evs = all(": keep-alive\n\nevent: ping\ndata: {}\n\ndata: a\ndata: b\r\n\r\nevent: x\ndata:no-space\n\ndata: tail");
        assert_eq!(
            evs,
            vec![
                SseEvent { event: "ping".into(), data: "{}".into() },
                SseEvent { event: String::new(), data: "a\nb".into() },
                SseEvent { event: "x".into(), data: "no-space".into() },
                SseEvent { event: String::new(), data: "tail".into() },
            ]
        );
        assert!(all("").is_empty());
        assert!(all("event: lonely\n\n").is_empty(), "an event without data is not dispatched");
    }
}
