//! The clipboard channel between vmmc's agent in a guest (`vmmcagent.exe`, from the
//! vmmc Tools disc) and the VM's worker: frames on the guest's second serial port
//! (COM2), both ways. A frame is [`MAGIC`], the text's length (4 bytes, little-endian)
//! and the text, UTF-8. Anything between frames is skipped.

/// What starts a frame.
pub const MAGIC: [u8; 6] = *b"\x1bVCLIP";
/// The longest text shared: it has to fit a pipe message too.
pub const MAX_TEXT: usize = 512 << 10;
const HEADER: usize = MAGIC.len() + 4;

/// `text` as a frame, cut to [`MAX_TEXT`] bytes (at a character's end).
pub fn frame(text: &str) -> Vec<u8> {
    let mut end = text.len().min(MAX_TEXT);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let text = &text.as_bytes()[..end];
    let mut frame = Vec::with_capacity(HEADER + text.len());
    frame.extend_from_slice(&MAGIC);
    frame.extend_from_slice(&(text.len() as u32).to_le_bytes());
    frame.extend_from_slice(text);
    frame
}

/// Takes frames out of a byte stream.
#[derive(Debug, Default)]
pub struct Parser {
    pending: Vec<u8>,
}

impl Parser {
    /// The texts of the frames `bytes` completes. Bytes come one at a time from a
    /// UART, so a frame that has started is only looked at again once it's whole.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<String> {
        self.pending.extend_from_slice(bytes);
        let mut texts = Vec::new();
        loop {
            if !self.pending.starts_with(&MAGIC) {
                let Some(start) = self
                    .pending
                    .windows(MAGIC.len())
                    .position(|window| window == MAGIC)
                else {
                    // Keep what could be the start of a magic.
                    let keep = self.pending.len().min(MAGIC.len() - 1);
                    self.pending.drain(..self.pending.len() - keep);
                    return texts;
                };
                self.pending.drain(..start);
            }
            if self.pending.len() < HEADER {
                return texts;
            }
            let len =
                u32::from_le_bytes(self.pending[MAGIC.len()..HEADER].try_into().unwrap()) as usize;
            if len > MAX_TEXT {
                // Not a frame after all.
                self.pending.drain(..1);
                continue;
            }
            if self.pending.len() < HEADER + len {
                return texts;
            }
            texts.push(String::from_utf8_lossy(&self.pending[HEADER..HEADER + len]).into_owned());
            self.pending.drain(..HEADER + len);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_come_out_whole_however_the_bytes_arrive() {
        let mut stream = b"noise".to_vec();
        stream.extend(frame("héllo"));
        stream.extend(b"\x1bV");
        stream.extend(frame(""));
        stream.extend(frame("second\r\nline"));
        let mut parser = Parser::default();
        let mut texts = Vec::new();
        for byte in &stream {
            texts.extend(parser.feed(std::slice::from_ref(byte)));
        }
        assert_eq!(texts, ["héllo", "", "second\r\nline"]);
        assert_eq!(Parser::default().feed(&stream).len(), 3);
    }

    #[test]
    fn long_texts_are_cut_at_a_character() {
        let text = "é".repeat(MAX_TEXT);
        let framed = frame(&text);
        let mut parser = Parser::default();
        let texts = parser.feed(&framed);
        assert_eq!(texts.len(), 1);
        assert!(texts[0].len() <= MAX_TEXT && texts[0].chars().all(|c| c == 'é'));
    }

    #[test]
    fn a_bad_length_is_skipped() {
        let mut stream = MAGIC.to_vec();
        stream.extend(u32::MAX.to_le_bytes());
        stream.extend(frame("ok"));
        assert_eq!(Parser::default().feed(&stream), ["ok"]);
    }
}
