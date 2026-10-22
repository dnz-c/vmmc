//! The clipboard's frames on the control line, both ways: [`MAGIC`], the text's length
//! (4 bytes, little-endian) and the text, UTF-8. (The first agents knew only these.)
//! [`Parser`](crate::Parser) takes them out of the stream along with messages.

/// What starts a frame.
pub const MAGIC: [u8; 6] = *b"\x1bVCLIP";
/// The longest text shared: it has to fit a pipe message too.
pub const MAX_TEXT: usize = 512 << 10;

/// `text` as a frame, cut to [`MAX_TEXT`] bytes (at a character's end).
pub fn frame(text: &str) -> Vec<u8> {
    let text = &text.as_bytes()[..cut(text, MAX_TEXT)];
    let mut frame = Vec::with_capacity(MAGIC.len() + 4 + text.len());
    frame.extend_from_slice(&MAGIC);
    frame.extend_from_slice(&(text.len() as u32).to_le_bytes());
    frame.extend_from_slice(text);
    frame
}

/// How much of `text` fits in `max` bytes, ending at a character's end.
pub fn cut(text: &str, max: usize) -> usize {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Frame, Parser};

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
        let expected = ["héllo", "", "second\r\nline"].map(|t| Frame::Clipboard(t.into()));
        assert_eq!(texts, expected);
        assert_eq!(Parser::default().feed(&stream), expected);
    }

    #[test]
    fn long_texts_are_cut_at_a_character() {
        let text = "é".repeat(MAX_TEXT);
        let frames = Parser::default().feed(&frame(&text));
        let [Frame::Clipboard(text)] = &frames[..] else {
            panic!("one frame: {frames:?}");
        };
        assert!(text.len() <= MAX_TEXT && text.chars().all(|c| c == 'é'));
    }

    #[test]
    fn a_bad_length_is_skipped() {
        let mut stream = MAGIC.to_vec();
        stream.extend(u32::MAX.to_le_bytes());
        stream.extend(frame("ok"));
        assert_eq!(
            Parser::default().feed(&stream),
            [Frame::Clipboard("ok".into())]
        );
    }
}
