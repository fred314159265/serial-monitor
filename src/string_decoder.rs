use crate::ProgramError;
use std::{char, str};
use tokio_util::bytes::BytesMut;
use tokio_util::codec::Decoder;

/// A lossy string decoder that replaces unrecognized characters with [`REPLACEMENT_CHAR`](std::char::REPLACEMENT_CHAR).
pub struct StringDecoder {
    /// An incomplete `char` value being decoded from the stream.
    /// `char`s are always four bytes in length.
    incomplete: (usize, [u8; 4]),
}

impl StringDecoder {
    pub const fn new() -> StringDecoder {
        StringDecoder {
            incomplete: (0, [0; 4]),
        }
    }
}

impl Decoder for StringDecoder {
    type Error = ProgramError;
    type Item = String;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        if src.is_empty() {
            return Ok(None);
        }

        match str::from_utf8(src) {
            Err(err) if err.valid_up_to() > 0 => {
                // Split the bytes that are valid utf8 and turn it into &str.
                let split_bytes = src.split_to(err.valid_up_to());
                let valid_str = unsafe { str::from_utf8_unchecked(&split_bytes) };

                let (ref mut index, _) = self.incomplete;
                if *index > 0 {
                    // We have a partial character stored, but decoded a valid string
                    // after it, this means that this partial character cannot be
                    // completed so we replace it with a `REPLACEMENT_CHARACTER`.
                    let mut result = String::with_capacity(
                        valid_str.len() + char::REPLACEMENT_CHARACTER.len_utf8(),
                    );
                    result.push(char::REPLACEMENT_CHARACTER);
                    result.push_str(valid_str);
                    // Reset the incomplete index
                    *index = 0;

                    Ok(Some(result))
                } else {
                    Ok(Some(valid_str.to_owned()))
                }
            }
            Err(_) => {
                let (ref mut index, ref mut buf) = self.incomplete;

                // Index is always less than 4, because of below.
                // `split_to(1)` panics when `src` is empty, and indexing the
                // result panics if that split is empty. Upstream's
                // `.get(0).unwrap_or_default()` does not compile: `get` yields
                // `&u8`, which has no `Default`.
                if src.is_empty() {
                    return Ok(None);
                }
                buf[*index] = src.split_to(1)[0];
                *index += 1;

                // Check if char is valid
                if let Ok(s) = str::from_utf8(&buf[..*index]) {
                    // If valid turn it into string and return it.
                    let result = s.to_string();
                    // Reset index of `incomplete` because we've taken it.
                    *index = 0;

                    Ok(Some(result))
                } else if *index == 4 {
                    // Char is not valid, but the buffer is full, so return a
                    // replacement char.
                    *index = 0;
                    Ok(Some(char::REPLACEMENT_CHARACTER.to_string()))
                } else {
                    Ok(None)
                }
            }
            Ok(_) => {
                // Split the bytes used for `s`.
                let split_bytes = src.split();
                let s = unsafe { str::from_utf8_unchecked(&split_bytes) };

                let (ref mut index, _) = self.incomplete;
                if *index > 0 {
                    // We have a partial character stored, but decoded a valid string
                    // after it, this means that this partial character cannot be
                    // completed so we replace it with a `REPLACEMENT_CHARACTER`.
                    let mut result =
                        String::with_capacity(s.len() + char::REPLACEMENT_CHARACTER.len_utf8());
                    result.push(char::REPLACEMENT_CHARACTER);
                    result.push_str(s);
                    // Reset the incomplete index
                    *index = 0;

                    Ok(Some(result))
                } else {
                    Ok(Some(s.to_owned()))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::StringDecoder;
    use tokio_util::bytes::BytesMut;
    use tokio_util::codec::Decoder;

    fn decode_chunks(chunks: &[&[u8]]) -> Vec<String> {
        let mut decoder = StringDecoder::new();
        let mut out = Vec::new();
        for chunk in chunks {
            let mut src = BytesMut::from(*chunk);
            let mut spins = 0;
            while !src.is_empty() {
                spins += 1;
                assert!(spins < 8, "decoder stopped consuming input");
                match decoder.decode(&mut src).unwrap() {
                    Some(text) => out.push(text),
                    None => break,
                }
            }
        }
        out
    }

    #[test]
    fn empty_chunk_returns_none() {
        let mut decoder = StringDecoder::new();
        let mut src = BytesMut::new();
        assert!(decoder.decode(&mut src).unwrap().is_none());
    }

    #[test]
    fn ascii_is_returned_whole() {
        assert_eq!(decode_chunks(&[b"hi"]), vec!["hi".to_string()]);
    }

    #[test]
    fn multibyte_char_split_across_chunks() {
        // U+00E9 LATIN SMALL LETTER E WITH ACUTE is the bytes C3 A9.
        let joined: String = decode_chunks(&[b"caf\xc3", b"\xa9"]).concat();
        assert_eq!(joined, "café");
    }

    #[test]
    fn empty_chunk_between_partial_utf8_does_not_panic() {
        let mut decoder = StringDecoder::new();
        let mut lead = BytesMut::from(&b"\xc3"[..]);
        assert!(decoder.decode(&mut lead).unwrap().is_none());

        let mut empty = BytesMut::new();
        assert!(decoder.decode(&mut empty).unwrap().is_none());

        let mut rest = BytesMut::from(&b"\xa9"[..]);
        assert_eq!(decoder.decode(&mut rest).unwrap().as_deref(), Some("é"));
    }

    #[test]
    fn invalid_byte_is_held_until_the_next_decode() {
        // One invalid byte is not enough to emit a replacement. The following
        // ASCII byte stays in the buffer and is emitted, with the replacement,
        // on the next decode call. That matches how FramedRead waits for more
        // input after `decode` returns None.
        let mut decoder = StringDecoder::new();
        let mut src = BytesMut::from(&b"\xffA"[..]);
        assert!(decoder.decode(&mut src).unwrap().is_none());
        assert_eq!(&src[..], b"A");
        assert_eq!(
            decoder.decode(&mut src).unwrap().as_deref(),
            Some("\u{FFFD}A")
        );
    }
}
