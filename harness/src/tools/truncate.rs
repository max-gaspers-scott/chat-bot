//! Output truncation helper: head N lines + tail N lines with a notice.

/// Cap `text` to at most `max_head + max_tail` lines.
///
/// If `text` fits within the cap, return it unchanged.
/// Otherwise return the first `max_head` lines, a notice saying how many
/// lines were omitted and what offset to use to continue reading, then the
/// last `max_tail` lines.
pub fn truncate_output(text: &str, max_head: usize, max_tail: usize) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let total = lines.len();
    let cap = max_head + max_tail;

    if total <= cap {
        return text.to_string();
    }

    let omitted = total - cap;
    let resume_offset = max_head + 1; // 1-based line for read_file offset

    let head = lines[..max_head].join("\n");
    let tail = lines[total - max_tail..].join("\n");

    format!(
        "{head}\n[... {omitted} lines omitted. \
         Use read_file with offset={resume_offset} to see more ...]\n{tail}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_unchanged() {
        let text = "a\nb\nc";
        assert_eq!(truncate_output(text, 50, 50), text);
    }

    #[test]
    fn exact_cap_unchanged() {
        let lines: Vec<String> = (0..100).map(|i| i.to_string()).collect();
        let text = lines.join("\n");
        let result = truncate_output(&text, 50, 50);
        assert_eq!(result, text);
    }

    #[test]
    fn over_cap_truncated() {
        let lines: Vec<String> = (0..200).map(|i| i.to_string()).collect();
        let text = lines.join("\n");
        let result = truncate_output(&text, 50, 50);
        assert!(result.contains("lines omitted"));
        // Head: first 50 lines (0..49)
        assert!(result.starts_with("0\n"));
        assert!(result.contains("49\n"));
        // Tail: last 50 lines (150..199)
        assert!(result.contains("150"));
        assert!(result.ends_with("199"));
        // Exactly 100 omitted
        assert!(result.contains("100 lines omitted"));
    }

    #[test]
    fn resume_offset_correct() {
        let lines: Vec<String> = (0..120).map(|i| i.to_string()).collect();
        let text = lines.join("\n");
        let result = truncate_output(&text, 10, 10);
        // offset should be 11 (max_head + 1)
        assert!(result.contains("offset=11"));
    }
}
