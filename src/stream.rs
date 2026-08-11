//! Live transcript tailing. The driver uses this to emit Claude's session
//! JSONL transcript to stdout as it grows, giving per-message streaming
//! (the granularity `claude -p --output-format stream-json` produces).
//!
//! The file is being appended to by the child `claude` while we read it. We
//! read from our own cursor with `read_at` (pread) so we never disturb the
//! file's offset, and we hold back an incomplete trailing fragment until its
//! newline arrives so callers never see torn JSON.

use std::fs::File;
use std::io::Write;
use std::os::unix::fs::FileExt;
use std::path::Path;

pub struct Tailer {
    file: File,
    pos: u64,
    partial: Vec<u8>,
}

impl Tailer {
    pub fn open(path: &Path) -> std::io::Result<Self> {
        let file = File::open(path)?;
        Ok(Self {
            file,
            pos: 0,
            partial: Vec::new(),
        })
    }

    /// Read newly-available bytes and write every complete line (including its
    /// trailing `\n`) to `writer`. Returns the number of complete lines
    /// emitted. Non-blocking: returns 0 when there is nothing new.
    pub fn pump(&mut self, writer: &mut dyn Write) -> std::io::Result<usize> {
        let mut buf = [0u8; 4096];
        let mut emitted = 0usize;
        loop {
            let n = self.file.read_at(&mut buf, self.pos)?;
            if n == 0 {
                break;
            }
            self.pos += n as u64;
            self.partial.extend_from_slice(&buf[..n]);

            while let Some(nl) = self.partial.iter().position(|&b| b == b'\n') {
                let line_end = nl + 1;
                writer.write_all(&self.partial[..line_end])?;
                emitted += 1;
                self.partial.drain(..line_end);
            }
            if n < buf.len() {
                break;
            }
        }
        Ok(emitted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_path(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        let uniq = format!(
            "dash-p-stream-test-{}-{}",
            std::process::id(),
            name
        );
        p.push(uniq);
        p
    }

    #[test]
    fn emits_line_written_before_open() {
        let path = tmp_path("before");
        std::fs::write(&path, b"{\"a\":1}\n").unwrap();
        let mut t = Tailer::open(&path).unwrap();
        let mut out = Vec::new();
        let n = t.pump(&mut out).unwrap();
        assert_eq!(n, 1);
        assert_eq!(out, b"{\"a\":1}\n");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn tails_appended_lines_across_pumps() {
        let path = tmp_path("append");
        std::fs::write(&path, b"line1\n").unwrap();
        let mut t = Tailer::open(&path).unwrap();
        let mut out = Vec::new();
        t.pump(&mut out).unwrap();
        assert_eq!(out, b"line1\n");

        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"line2\n").unwrap();
        let n = t.pump(&mut out).unwrap();
        assert_eq!(n, 1);
        assert_eq!(out, b"line1\nline2\n");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn holds_back_partial_until_newline() {
        let path = tmp_path("partial");
        std::fs::write(&path, b"hello, ").unwrap();
        let mut t = Tailer::open(&path).unwrap();
        let mut out = Vec::new();
        assert_eq!(t.pump(&mut out).unwrap(), 0);
        assert!(out.is_empty());

        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"world!\n").unwrap();
        assert_eq!(t.pump(&mut out).unwrap(), 1);
        assert_eq!(out, b"hello, world!\n");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn emits_multiple_lines_in_one_pump() {
        let path = tmp_path("multi");
        std::fs::write(&path, b"a\nb\nc\n").unwrap();
        let mut t = Tailer::open(&path).unwrap();
        let mut out = Vec::new();
        assert_eq!(t.pump(&mut out).unwrap(), 3);
        assert_eq!(out, b"a\nb\nc\n");
        let _ = std::fs::remove_file(&path);
    }
}
