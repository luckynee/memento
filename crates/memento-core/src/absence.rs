use crate::segment::strip_default_scaffolding;

pub fn is_absence_statement(text: &str) -> bool {
    let text = strip_default_scaffolding(text).to_lowercase();
    let mut found = false;
    for sentence in text.split(['.', '!', '?', '\n', ';']) {
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
