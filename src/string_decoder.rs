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

    // These tests specify the behavior we want. Some fail until the decoder
    // matches that behavior. Do not change an assertion to match the current code.
    //
    // `read_in` follows `FramedRead`: a `None` from `decode` stops the read even
    // when bytes are still buffered. The next chunk is appended to that buffer.

    fn read_in(decoder: &mut StringDecoder, buf: &mut BytesMut, chunk: &[u8]) -> String {
        buf.extend_from_slice(chunk);
        let mut out = String::new();
        for _ in 0..32 {
            match decoder.decode(buf).unwrap() {
                Some(text) => out.push_str(&text),
                None => return out,
            }
        }
        panic!("decoder kept returning text");
    }

    fn read_chunks(chunks: &[&[u8]]) -> String {
        let mut decoder = StringDecoder::new();
        let mut buf = BytesMut::new();
        let mut out = String::new();
        for chunk in chunks {
            out.push_str(&read_in(&mut decoder, &mut buf, chunk));
        }
        out
    }

    #[test]
    fn empty_input_returns_none() {
        let mut decoder = StringDecoder::new();
        let mut src = BytesMut::new();
        assert!(decoder.decode(&mut src).unwrap().is_none());
    }

    #[test]
    fn ascii_is_returned_whole() {
        assert_eq!(read_chunks(&[b"hi"]), "hi");
    }

    #[test]
    fn two_byte_char_split_across_chunks() {
        // U+00E9 LATIN SMALL LETTER E WITH ACUTE is the bytes C3 A9.
        assert_eq!(read_chunks(&[b"caf\xc3", b"\xa9"]), "café");
    }

    #[test]
    fn empty_chunk_between_partial_utf8_does_not_drop_it() {
        let mut decoder = StringDecoder::new();
        let mut buf = BytesMut::new();
        assert_eq!(read_in(&mut decoder, &mut buf, b"\xc3"), "");
        assert_eq!(read_in(&mut decoder, &mut buf, b""), "");
        assert_eq!(read_in(&mut decoder, &mut buf, b"\xa9"), "é");
    }

    #[test]
    fn invalid_byte_and_following_ascii_are_emitted_together() {
        assert_eq!(read_chunks(&[b"\xffA"]), "\u{FFFD}A");
    }

    #[test]
    fn three_byte_char_split_across_chunks() {
        // U+20AC EURO SIGN is the bytes E2 82 AC.
        assert_eq!(read_chunks(&[b"\xe2\x82", b"\xac"]), "\u{20AC}");
    }

    #[test]
    fn four_byte_char_split_after_first_byte() {
        // U+1F600 GRINNING FACE is the bytes F0 9F 98 80.
        assert_eq!(read_chunks(&[b"\xf0", b"\x9f\x98\x80"]), "\u{1F600}");
    }

    #[test]
    fn four_byte_char_split_after_third_byte() {
        assert_eq!(read_chunks(&[b"\xf0\x9f\x98", b"\x80"]), "\u{1F600}");
    }

    #[test]
    fn valid_invalid_valid_is_emitted_in_one_read() {
        assert_eq!(read_chunks(&[b"A\xffB"]), "A\u{FFFD}B");
    }

    #[test]
    fn eof_on_empty_input_is_none() {
        let mut decoder = StringDecoder::new();
        let mut buf = BytesMut::new();
        assert!(decoder.decode_eof(&mut buf).unwrap().is_none());
    }

    #[test]
    fn eof_flushes_incomplete_lead_with_valid_prefix() {
        let mut decoder = StringDecoder::new();
        let mut buf = BytesMut::new();
        let mut all = read_in(&mut decoder, &mut buf, b"hi\xc3");
        let end = decoder
            .decode_eof(&mut buf)
            .expect("eof should flush, not error");
        if let Some(rest) = end {
            all.push_str(&rest);
        }
        assert_eq!(all, "hi\u{FFFD}");
    }

    #[test]
    fn complete_multibyte_characters_are_returned_whole() {
        assert_eq!(read_chunks(&["café".as_bytes()]), "café");
        assert_eq!(read_chunks(&[b"\xe2\x82\xac"]), "\u{20AC}");
        assert_eq!(read_chunks(&[b"\xf0\x9f\x98\x80"]), "\u{1F600}");
    }

    #[test]
    fn several_ascii_chunks_are_concatenated() {
        assert_eq!(read_chunks(&[b"ab", b"cd"]), "abcd");
    }

    #[test]
    fn eof_flushes_lone_invalid_byte() {
        let mut decoder = StringDecoder::new();
        let mut buf = BytesMut::new();
        let mut all = read_in(&mut decoder, &mut buf, b"\xff");
        let end = decoder
            .decode_eof(&mut buf)
            .expect("eof should flush, not error");
        if let Some(rest) = end {
            all.push_str(&rest);
        }
        assert_eq!(all, "\u{FFFD}");
    }

    #[tokio::test]
    async fn framed_read_joins_a_character_split_across_two_writes() {
        use futures::StreamExt;
        use tokio::io::AsyncWriteExt;
        use tokio_util::codec::FramedRead;

        let (client, mut server) = tokio::io::duplex(64);
        let mut framed = FramedRead::new(client, StringDecoder::new());
        server.write_all(b"caf\xc3").await.unwrap();
        let first = framed.next().await.unwrap().unwrap();
        assert_eq!(first, "caf");
        server.write_all(b"\xa9").await.unwrap();
        let second = framed.next().await.unwrap().unwrap();
        assert_eq!(second, "é");
    }

    #[tokio::test]
    async fn framed_read_does_not_wait_while_decoded_bytes_remain() {
        use futures::future::FutureExt;
        use futures::StreamExt;
        use tokio::io::AsyncWriteExt;
        use tokio_util::codec::FramedRead;

        let (client, mut server) = tokio::io::duplex(64);
        let mut framed = FramedRead::new(client, StringDecoder::new());
        server.write_all(b"\xffA").await.unwrap();
        // The bytes are already buffered. The read must not wait for the device
        // to send something else before it can show them. `now_or_never` polls
        // once; `Next` is `Unpin`, so it does not need an extra pin.
        let ready = framed.next().now_or_never();
        let text = match ready {
            Some(Some(Ok(text))) => text,
            Some(other) => panic!("unexpected frame: {:?}", other),
            None => panic!("decoder waited for more input while bytes were already buffered"),
        };
        assert_eq!(text, "\u{FFFD}A");
    }
}
