/// Incremental Server-Sent Events parser. Feed it bytes as they arrive and
/// it returns the `data` payload of each completed event. Handles events
/// split across chunks, `\r\n` line endings, comment lines and multi-line
/// `data:` fields. `event:`, `id:` and `retry:` are ignored.
#[derive(Debug, Default)]
pub struct SseParser {
    buf: Vec<u8>,
    data: Vec<String>,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Consume `bytes`; return the payloads of every event completed by them.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line);
            self.handle_line(line.trim_end_matches(['\r', '\n']), &mut out);
        }
        out
    }

    /// Flush at end of stream: a final line without a newline and an event
    /// without its terminating blank line are still delivered.
    pub fn finish(&mut self) -> Option<String> {
        let mut out = Vec::new();
        if !self.buf.is_empty() {
            let line = String::from_utf8_lossy(&std::mem::take(&mut self.buf)).into_owned();
            self.handle_line(line.trim_end_matches('\r'), &mut out);
        }
        self.dispatch(&mut out);
        out.pop()
    }

    fn handle_line(&mut self, line: &str, out: &mut Vec<String>) {
        if line.is_empty() {
            self.dispatch(out);
            return;
        }
        if line.starts_with(':') {
            return;
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        if field == "data" {
            self.data.push(value.to_owned());
        }
    }

    fn dispatch(&mut self, out: &mut Vec<String>) {
        if !self.data.is_empty() {
            out.push(self.data.join("\n"));
            self.data.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_split_across_chunks() {
        let mut p = SseParser::new();
        assert!(p.feed(b"data: {\"a\":").is_empty());
        assert!(p.feed(b" 1}\n").is_empty());
        assert_eq!(p.feed(b"\ndata: two\n\n"), vec!["{\"a\": 1}", "two"]);
        assert_eq!(p.finish(), None);
    }

    #[test]
    fn crlf_comments_and_other_fields_are_handled() {
        let mut p = SseParser::new();
        let got = p.feed(b": keep-alive\r\nevent: message\r\nid: 7\r\ndata: x\r\n\r\n");
        assert_eq!(got, vec!["x"]);
    }

    #[test]
    fn multi_line_data_is_joined_with_newlines() {
        let mut p = SseParser::new();
        assert_eq!(p.feed(b"data: a\ndata: b\n\n"), vec!["a\nb"]);
    }

    #[test]
    fn finish_flushes_a_trailing_event() {
        let mut p = SseParser::new();
        assert!(p.feed(b"data: [DONE]").is_empty());
        assert_eq!(p.finish().as_deref(), Some("[DONE]"));
        assert_eq!(p.finish(), None);
    }

    #[test]
    fn data_without_space_is_accepted() {
        let mut p = SseParser::new();
        assert_eq!(p.feed(b"data:x\n\n"), vec!["x"]);
    }
}
