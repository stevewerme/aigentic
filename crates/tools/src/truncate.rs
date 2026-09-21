use std::collections::VecDeque;

/// Default cap on the bytes a tool returns, per stream.
pub const DEFAULT_OUTPUT_CAP: usize = 32 * 1024;

/// Cap `text` at `cap` bytes, keeping the head and the tail and inserting a
/// marker that says how much was omitted. Text within the cap is returned
/// unchanged. Cuts land on UTF-8 character boundaries.
pub fn truncate_output(text: &str, cap: usize) -> String {
    let bytes = text.as_bytes();
    assemble(bytes, bytes, bytes.len(), cap)
}

/// Build the returned text from the first `cap` bytes (`head`), the last
/// `cap` bytes (`tail`) and the true `total` length. `head` and `tail` may
/// be the same slice when the whole output is available.
fn assemble(head: &[u8], tail: &[u8], total: usize, cap: usize) -> String {
    if total <= cap {
        return String::from_utf8_lossy(head).into_owned();
    }
    let head_keep = char_boundary_at_or_before(head, cap / 2);
    let tail_take = (cap - head_keep).min(tail.len());
    let tail_start = char_boundary_at_or_after(tail, tail.len() - tail_take);
    let omitted = total - head_keep - (tail.len() - tail_start);

    let mut out = String::with_capacity(cap + 64);
    out.push_str(&String::from_utf8_lossy(&head[..head_keep]));
    out.push_str(&format!("\n[... {omitted} bytes omitted ...]\n"));
    out.push_str(&String::from_utf8_lossy(&tail[tail_start..]));
    out
}

fn char_boundary_at_or_before(bytes: &[u8], mut idx: usize) -> usize {
    idx = idx.min(bytes.len());
    while idx > 0 && idx < bytes.len() && (bytes[idx] & 0b1100_0000) == 0b1000_0000 {
        idx -= 1;
    }
    idx
}

fn char_boundary_at_or_after(bytes: &[u8], mut idx: usize) -> usize {
    while idx < bytes.len() && (bytes[idx] & 0b1100_0000) == 0b1000_0000 {
        idx += 1;
    }
    idx
}

/// Bounded capture of a byte stream: keeps the first `cap` bytes, the last
/// `cap` bytes and the total count, so memory stays at most `2 * cap`
/// however much a process prints.
#[derive(Debug)]
pub(crate) struct BoundedCapture {
    cap: usize,
    head: Vec<u8>,
    tail: VecDeque<u8>,
    total: usize,
}

impl BoundedCapture {
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            cap,
            head: Vec::new(),
            tail: VecDeque::new(),
            total: 0,
        }
    }

    pub(crate) fn push(&mut self, chunk: &[u8]) {
        self.total += chunk.len();
        let room = self.cap.saturating_sub(self.head.len());
        self.head.extend_from_slice(&chunk[..room.min(chunk.len())]);
        let start = chunk.len().saturating_sub(self.cap);
        self.tail.extend(&chunk[start..]);
        while self.tail.len() > self.cap {
            self.tail.pop_front();
        }
    }

    pub(crate) fn total(&self) -> usize {
        self.total
    }

    pub(crate) fn render(&self) -> String {
        let tail = self.tail.iter().copied().collect::<Vec<u8>>();
        assemble(&self.head, &tail, self.total, self.cap)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn within_cap_is_unchanged() {
        assert_eq!(truncate_output("hello", 5), "hello");
        assert_eq!(truncate_output("", 0), "");
    }

    #[test]
    fn over_cap_keeps_head_and_tail_and_notes_omitted_size() {
        let text: String = (1..=100).map(|i| format!("line {i}\n")).collect();
        let out = truncate_output(&text, 100);
        assert!(out.starts_with("line 1\n"), "{out}");
        assert!(out.ends_with("line 100\n"), "{out}");
        assert!(out.contains("[... "), "{out}");
        assert!(out.contains(" bytes omitted ...]\n"), "{out}");

        let marker = out.lines().find(|l| l.contains("omitted")).unwrap();
        let omitted: usize = marker.split_whitespace().nth(1).unwrap().parse().unwrap();
        let kept = out.len() - marker.len() - 2; // marker plus its surrounding newlines
        assert!(kept <= 100, "kept {kept} bytes, cap 100:\n{out}");
        assert_eq!(omitted + kept, text.len());
    }

    #[test]
    fn cuts_on_char_boundaries() {
        let text = "é".repeat(50); // 100 bytes, 2 each
        let out = truncate_output(&text, 11);
        assert!(!out.contains('\u{FFFD}'), "{out:?}");
        let head_part = out.split('\n').next().unwrap();
        assert_eq!(head_part, "éé"); // 11/2 = 5 bytes, backed off to the boundary at 4
        assert!(out.ends_with("ééé"), "{out:?}");
    }

    #[test]
    fn bounded_capture_matches_whole_string_truncation() {
        let text: String = (1..=500).map(|i| format!("{i}\n")).collect();
        let mut cap = BoundedCapture::new(64);
        for chunk in text.as_bytes().chunks(7) {
            cap.push(chunk);
        }
        assert_eq!(cap.total(), text.len());
        assert_eq!(cap.render(), truncate_output(&text, 64));
        assert!(cap.head.len() <= 64 && cap.tail.len() <= 64);
    }

    #[test]
    fn bounded_capture_within_cap_is_verbatim() {
        let mut cap = BoundedCapture::new(64);
        cap.push(b"abc");
        cap.push(b"def\n");
        assert_eq!(cap.render(), "abcdef\n");
    }
}
