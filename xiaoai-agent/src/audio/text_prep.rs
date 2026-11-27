//! Spoken-text sanitization and streaming sentence splitting for TTS.

/// Return true if `ch` is an emoji, pictograph, variation selector, or ZWJ
/// that should be stripped before sending text to TTS.
pub fn is_emoji_or_pictograph(ch: char) -> bool {
    matches!(
        ch,
        '\u{1F300}'..='\u{1FAFF}'
            | '\u{1F1E0}'..='\u{1F1FF}'
            | '\u{2600}'..='\u{27BF}'
            | '\u{2300}'..='\u{23FF}'
            | '\u{FE0E}'..='\u{FE0F}'
            | '\u{200D}'
            | '\u{20E3}'
            | '\u{E0020}'..='\u{E007F}'
    )
}

pub fn clean_tts_char(ch: char) -> char {
    match ch {
        '"' | '\\' | '*' | '#' | '`' => ' ',
        '\r' | '\n' => '。',
        _ => ch,
    }
}

pub fn has_speakable_char(text: &str) -> bool {
    text.chars()
        .any(|c| c.is_alphanumeric() || ('\u{4e00}'..='\u{9fff}').contains(&c))
}

fn is_collapsible_punct(ch: char) -> bool {
    matches!(
        ch,
        '。' | '，' | '！' | '？' | '；' | '：' | '、' | ',' | '!' | '?' | ';' | ':'
    )
}

fn is_url_body_char(ch: char) -> bool {
    ch.is_ascii()
        && !ch.is_ascii_whitespace()
        && !matches!(ch, ')' | ']' | '>' | '"' | '\'' | ',' | ';' | '!' | '?')
}

/// Sanitize a complete text string for spoken TTS output:
/// - Markdown links `[title](https://...)` -> `title`
/// - Bare `http://...` / `https://...` URLs stripped
/// - Markdown list prefixes (`- `, `* `, `• `) at line starts stripped
/// - Emojis/pictographs stripped
/// - Duplicate punctuation (`。。` -> `。`) and extra spaces collapsed
#[allow(dead_code)]
pub fn sanitize_spoken_text(text: &str) -> String {
    let mut splitter = StreamingSentenceSplitter::new();
    let mut parts = splitter.push_delta(text);
    if let Some(tail) = splitter.finish() {
        parts.push(tail);
    }
    parts.join(" ")
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum LinkState {
    None,
    /// Saw `[`, collecting title until `]`.
    InTitle(String),
    /// Saw `[title]`, waiting for next char to see if `(` follows.
    AfterBracket(String),
    /// Saw `[title](`, collecting URL until `)`.
    InUrl { title: String, url: String },
}

/// 增量流式切句器：首句尽量短小以便最快出声，后续段落适当放宽长度减少播放器冷启动次数。
pub struct StreamingSentenceSplitter {
    cur: String,
    emitted_count: usize,
    at_line_start: bool,
    pending_bullet: Option<char>,
    in_url: bool,
    link_state: LinkState,
    /// When `cur` ended with `.` (possible English sentence end), wait for the next
    /// char to decide whether it is `. ` (sentence end) vs `3.5` / `1.94` (decimal/version).
    pending_dot: bool,
}

impl StreamingSentenceSplitter {
    pub const FIRST_MAX_CHARS: usize = 50;
    pub const REST_MAX_CHARS: usize = 100;
    /// Maximum extra chars allowed beyond `max_len` to avoid splitting mid-ASCII-word or mid-number.
    const WORD_OVERFLOW_CHARS: usize = 12;

    pub fn new() -> Self {
        Self {
            cur: String::new(),
            emitted_count: 0,
            at_line_start: true,
            pending_bullet: None,
            in_url: false,
            link_state: LinkState::None,
            pending_dot: false,
        }
    }

    pub fn push_delta(&mut self, delta: &str) -> Vec<String> {
        let mut out = Vec::new();
        for ch in delta.chars() {
            self.feed_raw_char(ch, &mut out);
        }
        out
    }

    pub fn finish(&mut self) -> Option<String> {
        let mut out = Vec::new();
        self.flush_pending_states(&mut out);
        if let Some(tail) = self.take_speakable() {
            self.emitted_count += 1;
            out.push(tail);
        }
        if out.is_empty() {
            None
        } else {
            Some(out.join(" "))
        }
    }

    fn flush_pending_states(&mut self, out: &mut Vec<String>) {
        if let Some(bullet) = self.pending_bullet.take() {
            self.push_sanitized_char(bullet, out);
        }
        let link = std::mem::replace(&mut self.link_state, LinkState::None);
        match link {
            LinkState::None => {}
            LinkState::InTitle(title) => {
                self.push_sanitized_str(&format!("[{title}"), out);
            }
            LinkState::AfterBracket(title) => {
                self.push_sanitized_str(&format!("[{title}]"), out);
            }
            LinkState::InUrl { title, url } => {
                let trimmed = url.trim();
                if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
                    self.push_sanitized_str(&title, out);
                } else {
                    self.push_sanitized_str(&format!("[{title}]({url}"), out);
                }
            }
        }
        self.in_url = false;
        self.pending_dot = false;
    }

    fn feed_raw_char(&mut self, ch: char, out: &mut Vec<String>) {
        // 1. If currently inside a bare URL, consume until a non-URL char.
        if self.in_url {
            if is_url_body_char(ch) {
                return;
            }
            self.in_url = false;
            if ch == '\r' || ch == '\n' {
                self.at_line_start = true;
            }
            // Fall through to handle the terminating character normally.
        }

        // 2. Markdown link state machine: `[title](https://...)` -> `title`.
        if self.link_state != LinkState::None {
            let state = std::mem::replace(&mut self.link_state, LinkState::None);
            match state {
                LinkState::InTitle(mut title) => {
                    if ch == ']' {
                        self.link_state = LinkState::AfterBracket(title);
                        return;
                    } else if ch == '\n' || ch == '\r' || title.chars().count() >= 120 {
                        self.push_sanitized_str(&format!("[{title}"), out);
                        // fall through to process `ch`
                    } else {
                        title.push(ch);
                        self.link_state = LinkState::InTitle(title);
                        return;
                    }
                }
                LinkState::AfterBracket(title) => {
                    if ch == '(' {
                        self.link_state = LinkState::InUrl {
                            title,
                            url: String::new(),
                        };
                        return;
                    } else {
                        self.push_sanitized_str(&format!("[{title}]"), out);
                        // fall through to process `ch`
                    }
                }
                LinkState::InUrl { title, mut url } => {
                    if ch == ')' {
                        let trimmed = url.trim();
                        if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
                            self.push_sanitized_str(&title, out);
                        } else {
                            self.push_sanitized_str(&format!("[{title}]({url})"), out);
                        }
                        return;
                    } else if ch == '\n' || ch == '\r' || url.chars().count() >= 512 {
                        self.push_sanitized_str(&format!("[{title}]({url}"), out);
                        // fall through to process `ch`
                    } else {
                        url.push(ch);
                        self.link_state = LinkState::InUrl { title, url };
                        return;
                    }
                }
                LinkState::None => unreachable!(),
            }
        }

        // 3. Markdown list prefix detection at line start: `- `, `* `, `• `.
        if let Some(bullet) = self.pending_bullet.take() {
            if ch == ' ' || ch == '\t' {
                // Confirmed list prefix; drop both bullet and following space.
                self.at_line_start = false;
                return;
            }
            // Not a list prefix (e.g. `**标题**` or `-5`). Emit the held char first.
            self.at_line_start = false;
            self.push_sanitized_char(bullet, out);
        }

        if self.at_line_start {
            if ch == ' ' || ch == '\t' {
                return;
            }
            if matches!(ch, '-' | '*' | '•') {
                self.pending_bullet = Some(ch);
                return;
            }
            if ch != '\r' && ch != '\n' {
                self.at_line_start = false;
            }
        }

        if ch == '\r' || ch == '\n' {
            self.at_line_start = true;
        }

        if ch == '[' {
            self.link_state = LinkState::InTitle(String::new());
            return;
        }

        self.push_sanitized_char(ch, out);
    }

    fn push_sanitized_str(&mut self, s: &str, out: &mut Vec<String>) {
        for ch in s.chars() {
            if ch == '\r' || ch == '\n' {
                self.at_line_start = true;
            }
            self.push_sanitized_char(ch, out);
        }
    }

    fn push_sanitized_char(&mut self, raw_ch: char, out: &mut Vec<String>) {
        if is_emoji_or_pictograph(raw_ch) {
            return;
        }

        let ch = clean_tts_char(raw_ch);

        // If the previous char was `.`, decide now whether it was an English sentence end.
        if self.pending_dot {
            self.pending_dot = false;
            if ch == ' ' || ch == '\t' || raw_ch == '\n' || raw_ch == '\r' {
                if self.should_split_on_dot() {
                    if let Some(chunk) = self.take_speakable() {
                        self.emitted_count += 1;
                        out.push(chunk);
                    }
                    return;
                }
            }
        }

        // Collapse duplicate spaces and duplicate punctuation.
        if ch == ' ' {
            if self.cur.is_empty() || self.cur.ends_with(' ') {
                return;
            }
        } else if is_collapsible_punct(ch) {
            if let Some(last) = self.cur.chars().last() {
                if last == ch {
                    return;
                }
                // Converting newline to `。` right after another major sentence-ender: skip.
                if (raw_ch == '\n' || raw_ch == '\r')
                    && matches!(last, '。' | '！' | '？' | '；' | '!' | '?' | ';' | '.')
                {
                    return;
                }
            } else if raw_ch == '\n' || raw_ch == '\r' {
                return;
            }
        }

        self.cur.push(ch);

        // Detect bare `http://` or `https://` URLs as they stream in.
        if ch == '/' {
            if let Some(prefix_len) = self.trailing_url_scheme_len() {
                let keep_len = self.cur.len() - prefix_len;
                self.cur.truncate(keep_len);
                // Also trim a trailing space left right before the URL if any double-space would form.
                self.in_url = true;
                return;
            }
        }

        // Track potential English sentence-ending `.` (not immediately after another `.`).
        if ch == '.' {
            let prev = self.cur.chars().rev().nth(1);
            if prev.is_some_and(|p| p != '.') {
                self.pending_dot = true;
            }
            return;
        }

        let is_first = self.emitted_count == 0;
        let max_len = if is_first {
            Self::FIRST_MAX_CHARS
        } else {
            Self::REST_MAX_CHARS
        };
        let minor_threshold = if is_first { 18 } else { 38 };
        let is_major_sep = matches!(ch, '。' | '！' | '？' | '；' | '!' | '?' | ';');
        // Do not split on `:` when it could be part of `http:` or `https:`.
        let is_minor_sep = matches!(ch, '，' | ',' | '：' | ':')
            && !(ch == ':' && (self.cur.ends_with("http:") || self.cur.ends_with("https:")));

        let len = self.cur.chars().count();
        if is_major_sep || (is_minor_sep && len >= minor_threshold) {
            if let Some(chunk) = self.take_speakable() {
                self.emitted_count += 1;
                out.push(chunk);
            }
            return;
        }

        if len >= max_len {
            let in_ascii_token = ch.is_ascii_alphanumeric();
            if in_ascii_token && len < max_len + Self::WORD_OVERFLOW_CHARS {
                // Wait up to +12 chars to finish the current ASCII word or number.
                return;
            }
            if let Some(chunk) = self.split_at_safe_boundary(in_ascii_token) {
                self.emitted_count += 1;
                out.push(chunk);
            }
        }
    }

    fn trailing_url_scheme_len(&self) -> Option<usize> {
        if self.cur.ends_with("https://") {
            Some("https://".len())
        } else if self.cur.ends_with("http://") {
            Some("http://".len())
        } else {
            None
        }
    }

    fn should_split_on_dot(&self) -> bool {
        let trimmed = self.cur.trim();
        if !trimmed.ends_with('.') {
            return false;
        }
        let without_dot = &trimmed[..trimmed.len() - 1];
        if without_dot.is_empty() || without_dot.ends_with('.') {
            return false;
        }
        // Avoid splitting numbered list markers like `1.` or `12.` at the start of a chunk.
        if without_dot.chars().all(|c| c.is_ascii_digit()) {
            return false;
        }
        has_speakable_char(without_dot)
    }

    /// Split `self.cur` without cutting mid-ASCII-word/number when possible.
    fn split_at_safe_boundary(&mut self, forced_overflow: bool) -> Option<String> {
        if forced_overflow {
            // Find the last space or punctuation boundary to keep the trailing token intact.
            if let Some(byte_idx) = self.cur.rfind(|c: char| {
                c.is_whitespace()
                    || matches!(
                        c,
                        '。' | '，' | '！' | '？' | '；' | '：' | '、' | ',' | '!' | '?' | ';' | ':'
                    )
            }) {
                let split_at = byte_idx
                    + self.cur[byte_idx..]
                        .chars()
                        .next()
                        .map(char::len_utf8)
                        .unwrap_or(0);
                let head = self.cur[..split_at].trim().to_string();
                let tail = self.cur[split_at..].trim_start().to_string();
                if !head.is_empty() && has_speakable_char(&head) {
                    self.cur = tail;
                    return Some(head);
                }
            }
        }
        self.take_speakable()
    }

    fn take_speakable(&mut self) -> Option<String> {
        let trimmed = self.cur.trim().to_string();
        self.cur.clear();
        if !trimmed.is_empty() && has_speakable_char(&trimmed) {
            Some(trimmed)
        } else {
            None
        }
    }
}

impl Default for StreamingSentenceSplitter {
    fn default() -> Self {
        Self::new()
    }
}

/// 将完整长文本切分为少量语音段并清理 Markdown 符号与双引号。
#[allow(dead_code)]
pub fn split_tts_sentences(text: &str) -> Vec<String> {
    let mut splitter = StreamingSentenceSplitter::new();
    let mut raw_parts = splitter.push_delta(text);
    if let Some(tail) = splitter.finish() {
        raw_parts.push(tail);
    }

    let mut merged: Vec<String> = Vec::new();
    for part in raw_parts {
        let is_first_merged = merged.len() == 1;
        if let Some(last) = merged.last_mut() {
            let combined_len = last.chars().count() + 1 + part.chars().count();
            let should_merge = if is_first_merged {
                (last.chars().count() < 20 || part.chars().count() < 12)
                    && combined_len <= StreamingSentenceSplitter::FIRST_MAX_CHARS
            } else {
                combined_len <= 110
            };
            if should_merge {
                last.push(' ');
                last.push_str(&part);
                continue;
            }
        }
        merged.push(part);
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_prep_strips_markdown_links_urls_lists_emojis_and_collapses_punct() {
        let input = "- 请查看 [官方文档](https://example.com/docs) 😊✨\n• 更多信息见 https://rust-lang.org/learn 。。\n* 测试列表项！！";
        let sanitized = sanitize_spoken_text(input);
        assert!(sanitized.contains("官方文档"), "got: {sanitized}");
        assert!(!sanitized.contains("https://"), "got: {sanitized}");
        assert!(!sanitized.contains("example.com"), "got: {sanitized}");
        assert!(!sanitized.contains("rust-lang.org"), "got: {sanitized}");
        assert!(!sanitized.contains('😊') && !sanitized.contains('✨'), "got: {sanitized}");
        assert!(!sanitized.contains("。。"), "got: {sanitized}");
        assert!(!sanitized.contains("！！"), "got: {sanitized}");
        assert!(!sanitized.contains("- ") && !sanitized.contains("• ") && !sanitized.contains("* "), "got: {sanitized}");
    }

    #[test]
    fn streaming_splitter_handles_english_sentences_without_splitting_decimals_or_versions() {
        let mut splitter = StreamingSentenceSplitter::new();
        let mut chunks = Vec::new();
        chunks.extend(splitter.push_delta("Rust 1.94 is fast. The value is 3.5 meters. "));
        chunks.extend(splitter.push_delta("Done."));
        if let Some(tail) = splitter.finish() {
            chunks.push(tail);
        }
        assert_eq!(
            chunks,
            vec!["Rust 1.94 is fast.", "The value is 3.5 meters.", "Done."]
        );
    }

    #[test]
    fn streaming_splitter_does_not_split_year_or_ascii_word_at_max_len() {
        // 48 CJK chars followed by "2026年" -> hits FIRST_MAX_CHARS (50) right inside "2026".
        let prefix: String = std::iter::repeat_n('天', 48).collect();
        let input = format!("{prefix}2026年的夏天很热");
        let mut splitter = StreamingSentenceSplitter::new();
        let mut chunks = splitter.push_delta(&input);
        if let Some(tail) = splitter.finish() {
            chunks.push(tail);
        }
        assert!(
            chunks.iter().any(|c| c.contains("2026")),
            "2026 must not be split across chunks: {chunks:?}"
        );
    }

    #[test]
    fn streaming_splitter_handles_markdown_link_across_deltas() {
        let mut splitter = StreamingSentenceSplitter::new();
        let mut chunks = Vec::new();
        chunks.extend(splitter.push_delta("点击[小爱"));
        chunks.extend(splitter.push_delta("官网](https://xiaoai.mi.com/"));
        chunks.extend(splitter.push_delta("home)查看。"));
        if let Some(tail) = splitter.finish() {
            chunks.push(tail);
        }
        assert_eq!(chunks, vec!["点击小爱官网查看。"]);
    }
}
