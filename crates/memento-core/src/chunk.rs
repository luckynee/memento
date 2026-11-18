pub fn chunk_text(text: &str, max_chars: usize, overlap_chars: usize) -> Vec<String> {
    if text.is_empty() || max_chars == 0 {
        return Vec::new();
    }

    let chars: Vec<char> = text.chars().collect();
    let mut chunks = Vec::new();
    let mut start = 0;

    while start < chars.len() {
        let limit = (start + max_chars).min(chars.len());
        let end = if limit == chars.len() {
            limit
        } else {
            preferred_boundary(&chars, start, limit)
        };
        chunks.push(chars[start..end].iter().collect());
        if end == chars.len() {
            break;
        }
        start = end.saturating_sub(overlap_chars).max(start + 1);
    }

    chunks
}

fn preferred_boundary(chars: &[char], start: usize, limit: usize) -> usize {
    let paragraph = (start + 1..limit)
        .rev()
        .find(|&index| chars[index - 1] == '\n' && chars[index] == '\n');
    if let Some(index) = paragraph {
        return index + 1;
    }

    if let Some(index) = (start..limit).rev().find(|&index| chars[index] == '\n') {
        return index + 1;
    }

    (start..limit)
        .rev()
        .find(|&index| chars[index].is_whitespace())
        .map_or(limit, |index| index + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_and_empty_text() {
        assert_eq!(chunk_text("short", 10, 2), vec!["short"]);
        assert!(chunk_text("", 10, 2).is_empty());
        assert!(chunk_text("text", 0, 2).is_empty());
    }

    #[test]
    fn long_text_overlaps() {
        assert_eq!(
            chunk_text("abcdefghij", 5, 2),
            vec!["abcde", "defgh", "ghij"]
        );
    }

    #[test]
    fn prefers_paragraphs_over_later_lines_and_spaces() {
        assert_eq!(chunk_text("one\n\ntwo\nthree four", 14, 0)[0], "one\n\n");
        assert_eq!(chunk_text("one\ntwo three four", 10, 0)[0], "one\n");
        assert_eq!(chunk_text("one two three", 7, 0)[0], "one ");
    }

    #[test]
    fn preserves_multibyte_characters_and_terminates_with_large_overlap() {
        assert_eq!(chunk_text("é界🙂ab", 3, 1), vec!["é界🙂", "🙂ab"]);
        assert_eq!(chunk_text("abcd", 2, 5), vec!["ab", "bc", "cd"]);
    }
}
