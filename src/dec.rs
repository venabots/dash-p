//! Minimal responder for the DEC / XTerm queries Ink (the React-for-terminals
//! runtime Claude Code uses) emits at startup. Without responses the TUI hangs
//! forever waiting for a terminal it thinks is broken.
//!
//! Recognised:
//!   - DA1:  `ESC [ c`  / `ESC [ 0 c`      -> "VT100 with AVO"
//!   - DA2:  `ESC [ > c` / `ESC [ > 0 c`   -> a device-attributes reply
//!   - DSR:  `ESC [ 6 n`                   -> cursor position row 1 col 1
//!   - XTVERSION: `ESC [ > q`              -> a DCS version string
//!   - 18t:  `ESC [ 18 t`                  -> window size "8 ; rows ; cols t"
//!
//! Unlike a pure function over a single chunk, this responder is *stateful*
//! across reads: a query can straddle a PTY read boundary. We hold the bytes
//! of an incomplete trailing escape sequence in `carry` and prepend them to
//! the next chunk. That eliminates the "query split across two reads is
//! silently dropped -> hang" latent bug in the chunk-at-a-time approach.

/// Upper bound on carried bytes. A well-formed CSI/DCS query is short; if we
/// ever accumulate more than this without a final byte the stream is garbage,
/// so we drop the carry rather than grow unbounded.
const MAX_CARRY: usize = 128;

pub struct DecResponder {
    rows: u16,
    cols: u16,
    carry: Vec<u8>,
}

impl DecResponder {
    pub fn new(rows: u16, cols: u16) -> Self {
        Self {
            rows,
            cols,
            carry: Vec::new(),
        }
    }

    /// Feed a chunk of PTY output. Returns the bytes that should be written
    /// back to the PTY master (possibly empty). Any incomplete trailing
    /// escape sequence is retained internally for the next call.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<u8> {
        // Prepend carried bytes from a previously-incomplete sequence.
        let buf: Vec<u8> = if self.carry.is_empty() {
            chunk.to_vec()
        } else {
            let mut b = std::mem::take(&mut self.carry);
            b.extend_from_slice(chunk);
            b
        };

        let mut out = Vec::new();
        let mut i = 0usize;
        while i < buf.len() {
            if buf[i] != 0x1b {
                i += 1;
                continue;
            }
            // Need at least ESC + one more byte to classify.
            if i + 1 >= buf.len() {
                self.stash(&buf[i..]);
                return out;
            }
            if buf[i + 1] != b'[' {
                // Not a CSI we care about; consume the ESC and move on.
                i += 1;
                continue;
            }

            // CSI: ESC [ [>] params intermediates final
            let mut j = i + 2;
            let private_gt = j < buf.len() && buf[j] == b'>';
            if private_gt {
                j += 1;
            }
            while j < buf.len() && (0x30..=0x3f).contains(&buf[j]) {
                j += 1;
            }
            while j < buf.len() && (0x20..=0x2f).contains(&buf[j]) {
                j += 1;
            }
            if j >= buf.len() {
                // Final byte hasn't arrived yet; carry the partial sequence.
                self.stash(&buf[i..]);
                return out;
            }
            let final_byte = buf[j];
            let params_start = i + 2 + if private_gt { 1 } else { 0 };
            let params = &buf[params_start..j];

            self.respond(final_byte, private_gt, params, &mut out);
            i = j + 1;
        }
        out
    }

    fn stash(&mut self, bytes: &[u8]) {
        if bytes.len() > MAX_CARRY {
            // Malformed / not a real query — drop it.
            self.carry.clear();
        } else {
            self.carry.clear();
            self.carry.extend_from_slice(bytes);
        }
    }

    fn respond(&self, final_byte: u8, private_gt: bool, params: &[u8], out: &mut Vec<u8>) {
        match final_byte {
            b'c' => {
                if private_gt {
                    // DA2
                    out.extend_from_slice(b"\x1b[>0;0;0c");
                } else {
                    // DA1: "VT100 with AVO"
                    out.extend_from_slice(b"\x1b[?1;2c");
                }
            }
            b'n' => {
                // DSR cursor-position report.
                if params == b"6" {
                    out.extend_from_slice(b"\x1b[1;1R");
                }
            }
            b'q' => {
                // XTVERSION.
                if private_gt {
                    out.extend_from_slice(b"\x1bP>|dash-p\x1b\\");
                }
            }
            // Window-size report in characters: 8 ; rows ; cols t
            b't' if params == b"18" => {
                let reply = format!("\x1b[8;{};{}t", self.rows, self.cols);
                out.extend_from_slice(reply.as_bytes());
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn da1_query_gets_a_response() {
        let mut d = DecResponder::new(40, 120);
        assert_eq!(d.feed(b"\x1b[c"), b"\x1b[?1;2c");
    }

    #[test]
    fn da2_query_gets_a_response() {
        let mut d = DecResponder::new(40, 120);
        assert_eq!(d.feed(b"\x1b[>c"), b"\x1b[>0;0;0c");
    }

    #[test]
    fn dsr_cursor_position_responds_1_1() {
        let mut d = DecResponder::new(40, 120);
        assert_eq!(d.feed(b"\x1b[6n"), b"\x1b[1;1R");
    }

    #[test]
    fn xtversion_is_dcs_wrapped() {
        let mut d = DecResponder::new(40, 120);
        let r = d.feed(b"\x1b[>q");
        assert!(r.starts_with(b"\x1bP>|dash-p"));
        assert!(r.ends_with(b"\x1b\\"));
    }

    #[test]
    fn window_size_uses_configured_dimensions() {
        let mut d = DecResponder::new(40, 120);
        assert_eq!(d.feed(b"\x1b[18t"), b"\x1b[8;40;120t");
    }

    #[test]
    fn ignores_plain_text() {
        let mut d = DecResponder::new(40, 120);
        assert!(d.feed(b"hello world, no escapes here").is_empty());
    }

    #[test]
    fn multiple_queries_in_one_chunk() {
        let mut d = DecResponder::new(40, 120);
        let r = d.feed(b"hi\x1b[cthere\x1b[>cyo");
        assert!(r.windows(7).any(|w| w == b"\x1b[?1;2c"));
        assert!(r.windows(9).any(|w| w == b"\x1b[>0;0;0c"));
    }

    #[test]
    fn query_split_across_two_feeds_is_answered() {
        // This is the bug the original chunk-at-a-time scanner had: a CSI
        // straddling a read boundary was dropped. The carry buffer fixes it.
        let mut d = DecResponder::new(40, 120);
        assert!(d.feed(b"\x1b[").is_empty()); // incomplete -> carried
        assert_eq!(d.feed(b"c"), b"\x1b[?1;2c"); // completes the DA1 query
    }

    #[test]
    fn esc_split_at_very_end_is_carried() {
        let mut d = DecResponder::new(40, 120);
        assert!(d.feed(b"text\x1b").is_empty());
        assert_eq!(d.feed(b"[6n"), b"\x1b[1;1R");
    }

    #[test]
    fn runaway_carry_is_dropped() {
        let mut d = DecResponder::new(40, 120);
        // ESC [ followed by a flood of parameter bytes and no final byte.
        let mut junk = vec![0x1b, b'['];
        junk.extend(std::iter::repeat_n(b'0', MAX_CARRY + 10));
        assert!(d.feed(&junk).is_empty());
        // The carry was dropped, so a fresh valid query still works.
        assert_eq!(d.feed(b"\x1b[c"), b"\x1b[?1;2c");
    }
}
