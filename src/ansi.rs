//! ANSI escape handling for text dash-p reads from a harness: the PTY screen,
//! and a harness's stderr when a failure is surfaced to the user.

/// Strip CSI / OSC / DCS escape sequences, leaving literal payload. Used so
/// substring matching (trust-dialog detection, diagnostics) is robust against
/// the cursor-positioning escapes the TUI pads words with.
pub fn strip_csi(bytes: &[u8]) -> String {
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if b != 0x1b {
            out.push(b);
            i += 1;
            continue;
        }
        if i + 1 >= bytes.len() {
            break;
        }
        match bytes[i + 1] {
            b'[' => {
                i += 2;
                while i < bytes.len() && (0x30..=0x3f).contains(&bytes[i]) {
                    i += 1;
                }
                while i < bytes.len() && (0x20..=0x2f).contains(&bytes[i]) {
                    i += 1;
                }
                if i < bytes.len() {
                    i += 1; // final byte
                }
            }
            b']' => {
                i += 2;
                while i < bytes.len() {
                    if bytes[i] == 0x07 {
                        i += 1;
                        break;
                    }
                    if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            }
            b'P' | b'X' | b'^' | b'_' => {
                i += 2;
                while i < bytes.len() {
                    if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            }
            _ => i += 2,
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_csi_removes_cursor_moves() {
        let raw = b"\x1b[1Ctrust\x1b[3Cthis\x1b[2Cfolder\x1b[0m";
        let s = strip_csi(raw);
        assert!(s.contains("trust"));
        assert!(s.contains("folder"));
        assert!(!s.contains('\x1b'));
    }
}
