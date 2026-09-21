/// One complete Server-Sent Event: the optional `event:` type and the
/// joined `data:` payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

/// Incremental Server-Sent Events parser shared by the adapters. Feed it
/// bytes as they arrive and it returns each completed event. Handles events
/// split across chunks, `\r\n` line endings, comment lines and multi-line
/// `data:` fields. `id:` and `retry:` are ignored. OpenAI-style streams
/// carry no `event:` line; the Anthropic Messages API does.
#[derive(Debug, Default)]
pub struct SseParser {
    buf: Vec<u8>,
    event: Option<String>,
    data: Vec<String>,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Consume `bytes`; return every event completed by them.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
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
    pub fn finish(&mut self) -> Option<SseEvent> {
        let mut out = Vec::new();
        if !self.buf.is_empty() {
            let line = String::from_utf8_lossy(&std::mem::take(&mut self.buf)).into_owned();
            self.handle_line(line.trim_end_matches('\r'), &mut out);
        }
        self.dispatch(&mut out);
        out.pop()
    }

    fn handle_line(&mut self, line: &str, out: &mut Vec<SseEvent>) {
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
        match field {
            "data" => self.data.push(value.to_owned()),
            "event" => self.event = Some(value.to_owned()),
            _ => {}
        }
    }

    fn dispatch(&mut self, out: &mut Vec<SseEvent>) {
        if !self.data.is_empty() {
            out.push(SseEvent {
                event: self.event.take(),
                data: self.data.join("\n"),
            });
            self.data.clear();
        }
        self.event = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(events: Vec<SseEvent>) -> Vec<String> {
        events.into_iter().map(|e| e.data).collect()
    }

    #[test]
    fn events_split_across_chunks() {
        let mut p = SseParser::new();
        assert!(p.feed(b"data: {\"a\":").is_empty());
        assert!(p.feed(b" 1}\n").is_empty());
        assert_eq!(data(p.feed(b"\ndata: two\n\n")), vec!["{\"a\": 1}", "two"]);
        assert_eq!(p.finish(), None);
    }

    #[test]
    fn crlf_comments_and_other_fields_are_handled() {
        let mut p = SseParser::new();
        let got = p.feed(b": keep-alive\r\nevent: message\r\nid: 7\r\ndata: x\r\n\r\n");
        assert_eq!(
            got,
            vec![SseEvent {
                event: Some("message".into()),
                data: "x".into()
            }]
        );
    }

    #[test]
    fn multi_line_data_is_joined_with_newlines() {
        let mut p = SseParser::new();
        assert_eq!(data(p.feed(b"data: a\ndata: b\n\n")), vec!["a\nb"]);
    }

    #[test]
    fn finish_flushes_a_trailing_event() {
        let mut p = SseParser::new();
        assert!(p.feed(b"data: [DONE]").is_empty());
        assert_eq!(p.finish().map(|e| e.data).as_deref(), Some("[DONE]"));
        assert_eq!(p.finish(), None);
    }

    #[test]
    fn data_without_space_is_accepted() {
        let mut p = SseParser::new();
        assert_eq!(data(p.feed(b"data:x\n\n")), vec!["x"]);
    }

    #[test]
    fn event_type_does_not_leak_into_the_next_event() {
        let mut p = SseParser::new();
        let got = p.feed(b"event: ping\ndata: {}\n\ndata: {}\n\n");
        assert_eq!(got[0].event.as_deref(), Some("ping"));
        assert_eq!(got[1].event, None);
    }
}
