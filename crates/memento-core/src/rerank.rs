use chrono::Duration;

use crate::absence::is_absence_statement;
use crate::protocol::{Kind, SearchHit};
use crate::segment::Segment;

const QUESTION_RRF_FACTOR: f32 = 0.6;
const ABSENCE_RRF_FACTOR: f32 = 0.4;

#[derive(Debug, Clone)]
pub struct Candidate {
    pub segment: Segment,
    pub fused_score: f32,
}

#[derive(Debug, Clone)]
pub struct RerankConfig {
    pub recency_weight: f32,
    pub half_life: Duration,
    pub exact_match_bonus: f32,
}

impl Default for RerankConfig {
    fn default() -> Self {
        Self {
            recency_weight: 0.15,
            half_life: Duration::hours(24),
            exact_match_bonus: 0.5,
        }
    }
}

pub fn rerank(
    query: &str,
    candidates: Vec<Candidate>,
    config: &RerankConfig,
    k: usize,
) -> Vec<SearchHit> {
    let identifiers: Vec<&str> = query
        .split_whitespace()
        .map(|token| token.trim_matches(|ch: char| !ch.is_alphanumeric()))
        .filter(|token| is_identifier(token))
        .collect();
    let Some(newest) = candidates
        .iter()
        .map(|candidate| candidate.segment.ts)
        .max()
    else {
        return Vec::new();
    };
    let max_fused = candidates
        .iter()
        .map(|candidate| candidate.fused_score)
        .fold(f32::NEG_INFINITY, f32::max);
    let half_life_secs = config.half_life.num_seconds() as f64
        + f64::from(config.half_life.subsec_nanos()) / 1_000_000_000.0;
    let mut scored: Vec<_> = candidates
        .into_iter()
        .map(|candidate| {
            let normalized = if max_fused > 0.0 {
                candidate.fused_score / max_fused
            } else {
                0.0
            };
            let normalized = if candidate.segment.kind == Kind::UserRequest
                && is_question(&candidate.segment.text)
            {
                normalized * QUESTION_RRF_FACTOR
            } else {
                normalized
            };
            let normalized = if is_absence_statement(&candidate.segment.text) {
                normalized * ABSENCE_RRF_FACTOR
            } else {
                normalized
            };
            let age = newest.signed_duration_since(candidate.segment.ts);
            let age_secs =
                age.num_seconds() as f64 + f64::from(age.subsec_nanos()) / 1_000_000_000.0;
            let decay = if half_life_secs > 0.0 {
                (-std::f64::consts::LN_2 * age_secs / half_life_secs).exp() as f32
            } else if age_secs == 0.0 {
                1.0
            } else {
                0.0
            };
            let matched_identifiers = identifiers
                .iter()
                .filter(|identifier| {
                    contains_identifier(&candidate.segment.text, identifier)
                        || candidate
                            .segment
                            .path
                            .as_deref()
                            .is_some_and(|path| contains_identifier(path, identifier))
                })
                .count();
            let bonus = if identifiers.is_empty() {
                0.0
            } else {
                config.exact_match_bonus * matched_identifiers as f32 / identifiers.len() as f32
            };
            (
                normalized + config.recency_weight * decay + bonus,
                candidate.segment,
            )
        })
        .collect();
    scored.sort_by(|(left_score, left), (right_score, right)| {
        right_score
            .total_cmp(left_score)
            .then_with(|| right.ts.cmp(&left.ts))
            .then_with(|| left.source_id.cmp(&right.source_id))
            .then_with(|| left.chunk_index.cmp(&right.chunk_index))
            .then_with(|| left.id.cmp(&right.id))
            .then_with(|| left.session.cmp(&right.session))
            .then_with(|| left.text.cmp(&right.text))
            .then_with(|| left.path.cmp(&right.path))
            .then_with(|| kind_order(left.kind).cmp(&kind_order(right.kind)))
    });
    scored
        .into_iter()
        .take(k)
        .map(|(score, segment)| SearchHit {
            id: segment.id,
            score,
            kind: segment.kind,
            text: segment.text,
            harness: segment.harness,
            session: segment.session,
            ts: segment.ts,
            path: segment.path,
        })
        .collect()
}

fn is_question(text: &str) -> bool {
    let text = text.trim();
    text.ends_with('?')
        || text.split_whitespace().next().is_some_and(|word| {
            [
                "who", "what", "which", "when", "where", "why", "how", "is", "are", "does", "do",
                "did", "can", "could", "should",
            ]
            .iter()
            .any(|question_word| word.eq_ignore_ascii_case(question_word))
        })
}

fn contains_identifier(text: &str, identifier: &str) -> bool {
    text.match_indices(identifier).any(|(start, matched)| {
        let before = text[..start].chars().next_back();
        let after = text[start + matched.len()..].chars().next();
        !before.is_some_and(is_identifier_character)
            && (!after.is_some_and(is_identifier_character)
                || after == Some('.')
                    && !text[start + matched.len() + 1..]
                        .chars()
                        .next()
                        .is_some_and(|ch| ch.is_alphanumeric()))
    })
}

fn is_identifier_character(ch: char) -> bool {
    ch.is_alphanumeric() || matches!(ch, '_' | '.' | '-')
}

fn is_identifier(token: &str) -> bool {
    if token.contains('-')
        && token.split('-').all(|part| {
            !part.is_empty()
                && part
                    .chars()
                    .all(|ch| ch.is_alphabetic() && ch.is_lowercase())
        })
    {
        return false;
    }
    let chars: Vec<char> = token.chars().collect();
    let mixed_digits =
        chars.iter().any(|ch| ch.is_alphabetic()) && chars.iter().any(|ch| ch.is_numeric());
    let camel_case = chars
        .windows(2)
        .any(|pair| pair[0].is_lowercase() && pair[1].is_uppercase());
    let separated = chars.windows(3).any(|part| {
        part[0].is_alphanumeric()
            && matches!(part[1], '_' | '.' | '/' | '-')
            && part[2].is_alphanumeric()
    }) || chars.windows(4).any(|part| {
        part[0].is_alphanumeric() && part[1] == ':' && part[2] == ':' && part[3].is_alphanumeric()
    });
    mixed_digits || camel_case || separated
}

fn kind_order(kind: Kind) -> u8 {
    match kind {
        Kind::UserRequest => 0,
        Kind::AssistantNote => 1,
        Kind::Decision => 2,
        Kind::Constraint => 3,
        Kind::Task => 4,
        Kind::FileChange => 5,
        Kind::Command => 6,
        Kind::FileRef => 7,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Kind;
    use uuid::Uuid;

    fn candidate(source: &str, hours: i64, score: f32) -> Candidate {
        Candidate {
            segment: Segment {
                id: Uuid::from_u128(source.bytes().map(u128::from).sum()),
                harness: "h".into(),
                project: "p".into(),
                session: "s".into(),
                kind: Kind::UserRequest,
                text: source.into(),
                source_id: source.into(),
                chunk_index: 0,
                ts: "2026-01-01T00:00:00Z"
                    .parse::<chrono::DateTime<chrono::Utc>>()
                    .unwrap()
                    + Duration::hours(hours),
                tool: None,
                path: None,
                paths: Vec::new(),
            },
            fused_score: score,
        }
    }

    #[test]
    fn normalization_and_recency_use_newest_candidate() {
        let hits = rerank(
            "search",
            vec![candidate("old", 0, 2.0), candidate("new", 24, 1.0)],
            &RerankConfig::default(),
            10,
        );
        assert_eq!(hits[0].text, "old");
        assert_eq!(hits[0].harness, "h");
        assert!((hits[0].score - 1.075).abs() < 0.00001);
        assert!((hits[1].score - 0.65).abs() < 0.00001);
    }

    #[test]
    fn ties_are_deterministic_and_k_is_respected() {
        let config = RerankConfig {
            recency_weight: 0.0,
            ..RerankConfig::default()
        };
        let candidates = vec![
            candidate("b", 1, 1.0),
            candidate("a", 1, 1.0),
            candidate("old", 0, 1.0),
        ];
        let hits = rerank("search", candidates.clone(), &config, 2);
        assert_eq!(
            hits.iter().map(|hit| hit.text.as_str()).collect::<Vec<_>>(),
            ["a", "b"]
        );
        let mut reversed = candidates;
        reversed.reverse();
        assert_eq!(hits, rerank("search", reversed, &config, 2));
        assert!(rerank("search", Vec::new(), &config, 3).is_empty());
        assert!(rerank("search", vec![candidate("a", 0, 1.0)], &config, 0).is_empty());
    }

    #[test]
    fn zero_scores_only_get_recency() {
        let hits = rerank(
            "search",
            vec![candidate("old", 0, 0.0), candidate("new", 24, 0.0)],
            &RerankConfig::default(),
            2,
        );
        assert_eq!(hits[0].text, "new");
        assert!((hits[0].score - 0.15).abs() < 0.00001);
        assert!((hits[1].score - 0.075).abs() < 0.00001);
    }

    #[test]
    fn answering_notes_rank_above_questions_with_higher_fused_scores() {
        for kind in [Kind::AssistantNote, Kind::Decision] {
            let question = candidate("Who is the QA sign-off owner?", 0, 1.0);
            let mut answer = candidate("QA sign-off owner is Dana Reyes.", 0, 0.8);
            answer.segment.kind = kind;
            let hits = rerank(
                "QA sign-off owner",
                vec![question, answer],
                &RerankConfig::default(),
                2,
            );
            assert_eq!(hits[0].kind, kind);
        }
    }

    #[test]
    fn non_question_user_requests_are_not_penalized() {
        let hits = rerank(
            "QA owner",
            vec![candidate("Record the QA owner.", 0, 1.0)],
            &RerankConfig::default(),
            1,
        );
        assert!((hits[0].score - 1.15).abs() < 0.00001);
        assert!(is_question("  WHO owns this  "));
        assert!(is_question("Tell me the owner?  "));
        assert!(!is_question("Record the owner."));
    }

    #[test]
    fn absence_only_hits_rank_below_facts() {
        let mut absence = candidate("absence", 0, 1.0);
        absence.segment.kind = Kind::AssistantNote;
        absence.segment.text = "Deploy window: not recorded.".into();
        let mut fact = candidate("fact", 0, 0.7);
        fact.segment.kind = Kind::AssistantNote;
        fact.segment.text = "Deploy window: Friday.".into();
        let hits = rerank(
            "deploy window",
            vec![absence, fact],
            &RerankConfig::default(),
            2,
        );
        assert_eq!(hits[0].text, "Deploy window: Friday.");
        assert!((hits[1].score - 0.55).abs() < 0.00001);
    }

    #[test]
    fn exact_identifier_beats_newer_semantic_neighbour() {
        let mut task = candidate("task", 24, 1.0);
        task.segment.kind = Kind::Task;
        task.segment.text = "fix retry deadline".into();
        let mut command = candidate("command", 0, 1.0);
        command.segment.kind = Kind::Command;
        command.segment.text = "deadline_exceeded_in_retry_loop".into();
        let hits = rerank(
            "deadline_exceeded_in_retry_loop",
            vec![task, command],
            &RerankConfig::default(),
            2,
        );
        assert_eq!(hits[0].kind, Kind::Command);
        assert!((hits[0].score - 1.575).abs() < 0.00001);
    }

    #[test]
    fn identifier_boundary_match_ranks_above_substrings() {
        let mut false_match = candidate("false", 0, 1.0);
        false_match.segment.kind = Kind::AssistantNote;
        false_match.segment.text = "sha1_v100_legacy and my_route_handler_old".into();
        let mut true_match = candidate("true", 0, 0.8);
        true_match.segment.kind = Kind::AssistantNote;
        true_match.segment.text = "Implemented route_handler for payment ledger.".into();
        let hits = rerank(
            "v1 route_handler",
            vec![false_match, true_match],
            &RerankConfig::default(),
            2,
        );
        assert_eq!(
            hits[0].text,
            "Implemented route_handler for payment ledger."
        );
        assert!((hits[0].score - 1.2).abs() < 0.00001);
        assert!((hits[1].score - 1.15).abs() < 0.00001);
    }

    #[test]
    fn partial_identifier_matches_receive_proportional_bonus() {
        let hits = rerank(
            "v1 route_handler",
            vec![candidate("route_handler", 0, 1.0)],
            &RerankConfig::default(),
            1,
        );
        assert!((hits[0].score - 1.4).abs() < 0.00001, "{}", hits[0].score);
    }

    #[test]
    fn identifiers_score_proportional_case_sensitive_matches_in_text_or_path() {
        let mut matched = candidate("match", 0, 1.0);
        matched.segment.text = "run camelCase with item2 and mod::name".into();
        matched.segment.path = Some("src/invoice-worker.rs".into());
        let mut partial = matched.clone();
        partial.segment.text = "run camelcase with item2 and mod::name".into();
        let query = r#""camelCase", (src/invoice-worker.rs) item2 mod::name"#;
        let hits = rerank(query, vec![partial, matched], &RerankConfig::default(), 2);
        assert!((hits[0].score - 1.65).abs() < 0.00001);
        assert!((hits[1].score - 1.525).abs() < 0.00001);
        assert!(hits[0].text.contains("camelCase"));
    }

    #[test]
    fn lowercase_hyphenated_words_are_not_identifiers() {
        for token in ["sign-off", "half-even", "follow-up", "invoice-worker"] {
            assert!(!is_identifier(token), "{token}");
        }
    }

    #[test]
    fn technical_identifiers_remain_identifiers() {
        for token in [
            "X-Tide-Sig-v2",
            "tw_recovery_v3",
            "src/invoice-worker.rs",
            "mod::name",
            "item2",
            "camelCase",
            "invoice-Worker",
        ] {
            assert!(is_identifier(token), "{token}");
        }
    }

    #[test]
    fn lowercase_hyphenated_prose_does_not_boost_file_changes() {
        let mut decision = candidate("decision", 0, 1.0);
        decision.segment.kind = Kind::Decision;
        decision.segment.text = "QA approval owner is Dana.".into();
        let mut file_change = candidate("file_change", 0, 0.8);
        file_change.segment.kind = Kind::FileChange;
        file_change.segment.text = "Update sign-off documentation.".into();
        let hits = rerank(
            "QA sign-off owner",
            vec![decision, file_change],
            &RerankConfig::default(),
            2,
        );
        assert_eq!(hits[0].kind, Kind::Decision);
        assert_eq!(hits[1].kind, Kind::FileChange);
    }

    #[test]
    fn identifier_boundaries_cover_all_identifier_characters() {
        for ch in ['a', '2', 'é', '_', '-'] {
            assert!(!contains_identifier(&format!("{ch}item2"), "item2"));
            assert!(!contains_identifier(&format!("item2{ch}"), "item2"));
        }
        assert!(contains_identifier("(item2),", "item2"));
        assert!(contains_identifier("item2.", "item2"));
        assert!(!contains_identifier("item2.rs", "item2"));
        assert!(contains_identifier("xitem2 item2", "item2"));
        assert!(!contains_identifier("Item2", "item2"));
        assert!(contains_identifier("edited src/foo.rs today", "foo.rs"));
        assert!(contains_identifier("mod::item2", "item2"));
    }

    #[test]
    fn plain_queries_do_not_get_identifier_bonus() {
        let hits = rerank(
            "fix retry deadline 123",
            vec![candidate("fix retry deadline 123", 0, 1.0)],
            &RerankConfig::default(),
            1,
        );
        assert!((hits[0].score - 1.15).abs() < 0.00001);
        for token in ["hello", "123", "foo-", "-foo", "FOO", "foo:bar"] {
            assert!(!is_identifier(token), "{token}");
        }
        for token in [
            "foo_bar",
            "foo.bar",
            "foo/bar",
            "foo::bar",
            "foo-Bar",
            "foo2",
            "camelCase",
        ] {
            assert!(is_identifier(token), "{token}");
        }
    }
}
