use crate::protocol;
use bund_core::Result;

const MAX_EVENT_BYTES: usize = 8 * 1024 * 1024;

/// Incremental SSE framing. HTTP chunks may split UTF-8 characters, CRLF,
/// JSON, or event boundaries; decode only after a complete line is available.
#[derive(Default)]
pub(crate) struct Decoder {
    pending: Vec<u8>,
    event: String,
    data: String,
    has_data: bool,
    first_line: bool,
    skip_lf: bool,
}

impl Decoder {
    pub fn new() -> Self {
        Self {
            first_line: true,
            ..Self::default()
        }
    }

    pub fn push(
        &mut self,
        bytes: &[u8],
        consume: &mut impl FnMut(&str, &str) -> Result<()>,
    ) -> Result<()> {
        for byte in bytes {
            if self.skip_lf {
                self.skip_lf = false;
                if *byte == b'\n' {
                    continue;
                }
            }
            if matches!(*byte, b'\r' | b'\n') {
                self.skip_lf = *byte == b'\r';
                let mut line = String::from_utf8(std::mem::take(&mut self.pending))
                    .map_err(|_| protocol("SSE contains invalid UTF-8"))?;
                if self.first_line {
                    line = line.trim_start_matches('\u{feff}').to_owned();
                    self.first_line = false;
                }
                self.line(&line, consume)?;
            } else {
                self.pending.push(*byte);
                if self.pending.len() + self.data.len() + self.event.len() > MAX_EVENT_BYTES {
                    return Err(protocol("SSE event exceeds the size limit"));
                }
            }
        }
        Ok(())
    }

    fn line(
        &mut self,
        line: &str,
        consume: &mut impl FnMut(&str, &str) -> Result<()>,
    ) -> Result<()> {
        if line.is_empty() {
            if self.has_data {
                consume(&self.event, &self.data)?;
            }
            self.event.clear();
            self.data.clear();
            self.has_data = false;
            return Ok(());
        }
        if line.starts_with(':') {
            return Ok(());
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => self.event = value.to_owned(),
            "data" => {
                if self.has_data {
                    self.data.push('\n');
                }
                self.data.push_str(value);
                self.has_data = true;
            }
            _ => {} // id/retry/extensions do not change model response state.
        }
        if self.data.len() + self.event.len() > MAX_EVENT_BYTES {
            return Err(protocol("SSE event exceeds the size limit"));
        }
        Ok(())
    }

    pub fn finish(&self) -> Result<()> {
        if !self.pending.is_empty() || self.has_data {
            return Err(protocol("truncated SSE event"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_fragmentation_crlf_utf8_and_multiline() {
        let input = "\u{feff}: ping\r\nevent: text\r\ndata: hé\r\ndata: llo\r\n\r\n";
        let mut decoder = Decoder::new();
        let mut values = Vec::new();
        for b in input.as_bytes() {
            decoder
                .push(&[*b], &mut |kind, value| {
                    values.push((kind.to_owned(), value.to_owned()));
                    Ok(())
                })
                .unwrap();
        }
        decoder.finish().unwrap();
        assert_eq!(values, vec![("text".into(), "hé\nllo".into())]);
    }

    #[test]
    fn rejects_unterminated_data() {
        let mut decoder = Decoder::new();
        decoder.push(b"data: {}\n", &mut |_, _| Ok(())).unwrap();
        assert!(decoder.finish().is_err());
    }

    #[test]
    fn cr_only_and_mixed_line_endings() {
        for input in [
            "event: text\rdata: hello\r\r",
            "event: text\r\ndata: hello\n\r",
        ] {
            let mut decoder = Decoder::new();
            let mut values = Vec::new();
            for byte in input.bytes() {
                decoder
                    .push(&[byte], &mut |kind, value| {
                        values.push((kind.to_owned(), value.to_owned()));
                        Ok(())
                    })
                    .unwrap();
            }
            decoder.finish().unwrap();
            assert_eq!(values, vec![("text".into(), "hello".into())]);
        }
    }

    #[test]
    fn invalid_utf8_and_oversize_events_fail_closed() {
        let mut decoder = Decoder::new();
        assert!(decoder.push(b"data: \xff\n\n", &mut |_, _| Ok(())).is_err());
        let mut decoder = Decoder::new();
        assert!(decoder
            .push(&vec![b'x'; MAX_EVENT_BYTES + 1], &mut |_, _| Ok(()))
            .is_err());
    }
}
