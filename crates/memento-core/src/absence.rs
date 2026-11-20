use crate::segment::strip_default_scaffolding;

pub fn is_absence_statement(text: &str) -> bool {
    let text = strip_default_scaffolding(text).to_lowercase();
    let mut found = false;
    let mut sentence_start = 0;
    let sentence_ends = text
        .char_indices()
        .filter(|&(index, ch)| match ch {
            '!' | '?' | '\n' | ';' => true,
            '.' => {
                let prefix = &text[..=index];
                let next = text[index + 1..].chars().next();
                next.is_none_or(char::is_whitespace)
                    && !["e.g.", "i.e.", "etc.", "vs."].iter().any(|abbreviation| {
                        prefix.strip_suffix(abbreviation).is_some_and(|before| {
                            before
                                .chars()
                                .next_back()
                                .is_none_or(|previous| !previous.is_alphanumeric())
                        })
                    })
            }
            _ => false,
        })
        .map(|(index, _)| index)
        .chain(std::iter::once(text.len()));
    for sentence_end in sentence_ends {
        let sentence = &text[sentence_start..sentence_end];
        sentence_start = sentence_end.saturating_add(1);
        let sentence = sentence.trim();
        if sentence.is_empty() {
            continue;
        }
        found = true;
        if !is_absence_sentence(sentence) {
            return false;
        }
    }
    found
}

fn is_absence_sentence(sentence: &str) -> bool {
    let words: Vec<&str> = sentence
        .split(|ch: char| !ch.is_alphanumeric() && ch != '\'')
        .filter(|word| !word.is_empty())
        .collect();
    if words.iter().any(|word| {
        matches!(
            *word,
            "but" | "however" | "although" | "whereas" | "except" | "yet"
        )
    }) {
        return false;
    }
    let contains = |phrase: &[&str]| words.windows(phrase.len()).any(|part| part == phrase);
    let recorded = [
        &["not", "recorded"][..],
        &["isn't", "recorded"],
        &["wasn't", "recorded"],
        &["not", "mentioned"],
    ]
    .iter()
    .any(|phrase| words.ends_with(phrase));
    let context = ["context", "memory", "digest"].iter().any(|place| {
        ["my", "the"].iter().any(|owner| {
            words.ends_with(&["not", "in", owner, place])
                || words.ends_with(&["isn't", "in", owner, place])
                || words.ends_with(&["wasn't", "in", owner, place])
        })
    });
    let missing_details = ["don't", "do not"].iter().any(|negation| {
        ["those", "that", "any"].iter().any(|determiner| {
            ["details", "information"].iter().any(|detail| {
                let phrase = format!("{negation} have {determiner} {detail}");
                sentence == phrase || sentence.ends_with(&format!(" {phrase}"))
            })
        })
    });
    let missing_subject = [
        &["no", "record", "of"][..],
        &["no", "information", "about"],
        &["couldn't", "find"],
        &["could", "not", "find"],
    ]
    .iter()
    .any(|phrase| {
        contains(phrase)
            && words
                .windows(phrase.len())
                .position(|part| part == *phrase)
                .is_some_and(|index| index + phrase.len() < words.len())
    });
    recorded || context || missing_details || missing_subject
}

#[cfg(test)]
mod tests {
    use super::is_absence_statement;

    #[test]
    fn recognizes_dotted_tokens_in_absence_sentences() {
        let cases = [
            "I couldn't find foo.rs.",
            "No record of v1.2.",
            "I could not find Cargo.toml in the repo.",
            "No record of e.g. retry limits.",
            "No record of I.E. retry limits.",
            "No record of etc. retry limits.",
            "No record of VS. retry limits.",
            "I couldn't find the deploy window.",
        ];
        let failures: Vec<_> = cases
            .into_iter()
            .filter(|text| !is_absence_statement(text))
            .collect();
        assert!(failures.is_empty(), "unrecognized: {failures:?}");
    }

    #[test]
    fn retains_non_absence_sentence_after_dotted_token() {
        assert!(!is_absence_statement(
            "I couldn't find foo.rs. The owner is Dana."
        ));
        assert!(!is_absence_statement(
            "No record of e.g. retry limits. The owner is Dana."
        ));
        assert!(!is_absence_statement(
            "No record of a previous. The owner is Dana."
        ));
    }

    #[test]
    fn recognizes_absence_only_sentences_and_strips_scaffolding() {
        for text in [
            "Deploy window: not recorded.",
            "The owner isn't recorded.\nThe date wasn't recorded.",
            "No record of the deploy window.",
            "The owner is not in my context.",
            "The window isn't in the memory.",
            "I don't have those details.",
            "I do not have any information.",
            "I couldn't find the deploy window.",
            "I could not find the owner.",
            "No information about the release.",
            "The date was not mentioned.",
            "<system-reminder>ignore</system-reminder>Not recorded.",
        ] {
            assert!(is_absence_statement(text), "{text}");
        }
    }

    #[test]
    fn retains_mixed_normal_and_empty_text() {
        for text in [
            "Deploy window: not recorded. QA owner: Dana.",
            "No record of the window.\nQA owner: Dana.",
            "QA owner: Dana.",
            "Not recorded, but the owner is Dana.",
            "No record of the window, but the owner is Dana.",
            "",
            "<system-reminder>not recorded</system-reminder>",
        ] {
            assert!(!is_absence_statement(text), "{text}");
        }
    }
}
