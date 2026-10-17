//! In-memory ring buffer of the most recent log lines, served by
//! `GET /admin/v1/logs` and `glyph-server remote logs`. Lines are also
//! printed to stderr. Nothing secret is ever logged: request lines contain
//! method, path (no query string, no IP) and status only.

use glyph_server_core::admin::types::LogsResponse;
use std::collections::VecDeque;
use std::io::Write;
use std::sync::{Arc, Mutex};
use tracing_subscriber::fmt::MakeWriter;

const CAPACITY: usize = 2000;

#[derive(Default)]
pub struct LogBuffer {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// Sequence number of the next line.
    next: u64,
    lines: VecDeque<(u64, String)>,
}

impl LogBuffer {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn push(&self, line: &str) {
        let line = line.trim_end_matches(['\n', '\r']);
        if line.is_empty() {
            return;
        }
        let mut g = self.inner.lock().unwrap();
        let seq = g.next;
        g.next += 1;
        g.lines.push_back((seq, line.to_string()));
        while g.lines.len() > CAPACITY {
            g.lines.pop_front();
        }
    }

    /// Up to `max` newest lines with sequence number >= `after` (or all).
    pub fn read(&self, max: usize, after: Option<u64>) -> LogsResponse {
        let g = self.inner.lock().unwrap();
        let mut lines: Vec<&(u64, String)> = g.lines.iter().filter(|(s, _)| after.map(|a| *s >= a).unwrap_or(true)).collect();
        if lines.len() > max {
            lines = lines.split_off(lines.len() - max);
        }
        LogsResponse { lines: lines.iter().map(|(_, l)| l.clone()).collect(), next: g.next }
    }
}

/// `tracing` writer: tee to stderr and the ring buffer.
pub struct LineWriter {
    buf: Arc<LogBuffer>,
    pending: Vec<u8>,
}

impl Write for LineWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.pending.extend_from_slice(data);
        let _ = std::io::stderr().write_all(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for LineWriter {
    fn drop(&mut self) {
        let text = String::from_utf8_lossy(&self.pending).into_owned();
        for l in text.lines() {
            self.buf.push(l);
        }
    }
}

#[derive(Clone)]
pub struct TeeMaker(pub Arc<LogBuffer>);

impl<'a> MakeWriter<'a> for TeeMaker {
    type Writer = LineWriter;
    fn make_writer(&'a self) -> LineWriter {
        LineWriter { buf: self.0.clone(), pending: Vec::new() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_buffer_keeps_newest_and_supports_follow() {
        let b = LogBuffer::new();
        for i in 0..(CAPACITY + 10) {
            b.push(&format!("line {i}"));
        }
        let all = b.read(5000, None);
        assert_eq!(all.lines.len(), CAPACITY);
        assert_eq!(all.lines[0], "line 10");
        assert_eq!(all.next, (CAPACITY + 10) as u64);
        let last3 = b.read(3, None);
        assert_eq!(last3.lines, vec!["line 2007", "line 2008", "line 2009"]);
        // follow: only lines after the cursor
        let cursor = b.read(1, None).next;
        b.push("new line\n");
        b.push("");
        let more = b.read(100, Some(cursor));
        assert_eq!(more.lines, vec!["new line"]);
        assert!(b.read(100, Some(more.next)).lines.is_empty());
    }

    #[test]
    fn tracing_writer_feeds_buffer() {
        let b = LogBuffer::new();
        let maker = TeeMaker(b.clone());
        {
            let mut w = maker.make_writer();
            w.write_all(b"hello\nworld\n").unwrap();
        }
        assert_eq!(b.read(10, None).lines, vec!["hello", "world"]);
    }
}
