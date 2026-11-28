//! 1:1 port of `xiaoai-doubao` `conversation_policy.hpp` Smart Turn endpointing.
//!
//! Dynamically extends quiet wait time when the partial ASR transcript ends on
//! an incomplete English/Chinese filler, conjunction, pronoun, or clause opener
//! (`unfinished_quiet_ms` vs `complete_quiet_ms`).

const EN_UNFINISHED: &[&str] = &[
    "a", "about", "after", "am", "an", "and", "any", "are", "at", "be", "because", "been",
    "before", "being", "but", "by", "can", "could", "did", "do", "does", "er", "erm", "every",
    "for", "from", "gonna", "gotta", "had", "has", "have", "he", "he's", "her", "his", "hmm", "i",
    "i'm", "if", "im", "in", "into", "is", "it's", "let", "let's", "like", "love", "may", "might",
    "mm", "must", "my", "need", "of", "on", "or", "our", "over", "really", "shall", "she",
    "she's", "should", "so", "some", "than", "that's", "the", "their", "then", "there's", "these",
    "they", "they're", "think", "this", "those", "to", "uh", "um", "under", "until", "very",
    "wanna", "want", "was", "we", "we're", "were", "when", "while", "will", "with", "would",
    "you're", "your",
];

const ZH_UNFINISHED: &[&str] = &[
    "我想",
    "我要",
    "然后",
    "就是",
    "因为",
    "所以",
    "那个",
    "这个",
    "嗯",
    "呃",
    "额",
    "和",
    "跟",
    "还有",
    "但是",
    "可是",
    "如果",
    "想问",
    "想说",
    "要问",
    "要说",
    "的是",
    "把",
    "给",
    "叫",
    "的时候",
    "最喜欢的",
];

/// Extract lowercased English words (with ASCII `'`) and U+4E00..=U+9FFF Han runs.
pub fn conversation_words(text: &str) -> Vec<String> {
    let normalized = text.replace('’', "'");
    let mut out = Vec::new();
    let mut word = String::new();
    let mut kind: u8 = 0;

    for ch in normalized.chars() {
        let (next, pushed): (u8, char) = if ch.is_ascii() {
            let lower = ch.to_ascii_lowercase();
            if lower.is_ascii_lowercase() || lower == '\'' {
                (1, lower)
            } else {
                (0, ch)
            }
        } else if ('\u{4e00}'..='\u{9fff}').contains(&ch) {
            (2, ch)
        } else {
            (0, ch)
        };

        if next != kind || next == 0 {
            if !word.is_empty() {
                out.push(std::mem::take(&mut word));
            }
        }
        if next != 0 {
            word.push(pushed);
        }
        kind = next;
    }

    if !word.is_empty() {
        out.push(word);
    }
    out
}

/// Return `true` if `text` is empty or ends on a word/suffix that typically
/// indicates the speaker is mid-thought.
pub fn looks_unfinished(text: &str) -> bool {
    let words = conversation_words(text);
    let Some(last) = words.last() else {
        return true;
    };
    if last.as_bytes().first().copied().unwrap_or(0) >= 128 {
        return ZH_UNFINISHED.iter().any(|suffix| last.ends_with(suffix));
    }
    EN_UNFINISHED.binary_search(&last.as_str()).is_ok()
}

/// Return `true` when `text` has at least one spoken word and `quiet_ms`
/// has reached the open or complete endpointing threshold.
pub fn should_commit_turn(text: &str, quiet_ms: u64, complete_ms: u64, open_ms: u64) -> bool {
    if conversation_words(text).is_empty() {
        return false;
    }
    let threshold = if looks_unfinished(text) {
        open_ms
    } else {
        complete_ms
    };
    quiet_ms >= threshold
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversation_words_splits_english_apostrophes_and_cjk_runs() {
        assert_eq!(
            conversation_words("I’m thinking，然后 say hello!"),
            vec!["i'm", "thinking", "然后", "say", "hello"]
        );
        assert_eq!(
            conversation_words("123... ？？？"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn looks_unfinished_detects_english_and_chinese_trailing_cues() {
        for unfinished in ["I want", "because", "我想", "然后", "其实我想问", "tell me about"] {
            assert!(
                looks_unfinished(unfinished),
                "expected '{unfinished}' to look unfinished"
            );
        }

        for complete in [
            "今天天气怎么样",
            "Tell me a joke",
            "明天的日程安排",
            "What time is it",
        ] {
            assert!(
                !looks_unfinished(complete),
                "expected '{complete}' to look complete"
            );
        }
    }

    #[test]
    fn should_commit_turn_waits_longer_for_unfinished_utterances() {
        assert!(!should_commit_turn("", 10_000, 1500, 4500));
        assert!(!should_commit_turn("。。。", 10_000, 1500, 4500));

        // Unfinished: stays open at 1500 ms, commits at 4500 ms.
        assert!(!should_commit_turn("我想", 1500, 1500, 4500));
        assert!(!should_commit_turn("because", 4499, 1500, 4500));
        assert!(should_commit_turn("because", 4500, 1500, 4500));
        assert!(should_commit_turn("然后", 4500, 1500, 4500));

        // Complete sentence: commits at 1500 ms.
        assert!(!should_commit_turn("今天天气不错", 1499, 1500, 4500));
        assert!(should_commit_turn("今天天气不错", 1500, 1500, 4500));
    }
}
