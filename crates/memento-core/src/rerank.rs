use chrono::Duration;

use crate::protocol::{Kind, SearchHit};
use crate::segment::Segment;

#[derive(Debug, Clone)]
pub struct Candidate {
    pub segment: Segment,
    pub fused_score: f32,
}

#[derive(Debug, Clone)]
pub struct RerankConfig {
    pub recency_weight: f32,
    pub half_life: Duration,
}

impl Default for RerankConfig {
    fn default() -> Self {
        Self {
            recency_weight: 0.15,
            half_life: Duration::hours(24),
        }
    }
}

pub fn rerank(candidates: Vec<Candidate>, config: &RerankConfig, k: usize) -> Vec<SearchHit> {
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
            (
                normalized + config.recency_weight * decay,
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
            session: segment.session,
            ts: segment.ts,
            path: segment.path,
        })
        .collect()
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
            },
            fused_score: score,
        }
    }

    #[test]
    fn normalization_and_recency_use_newest_candidate() {
        let hits = rerank(
            vec![candidate("old", 0, 2.0), candidate("new", 24, 1.0)],
            &RerankConfig::default(),
            10,
        );
        assert_eq!(hits[0].text, "old");
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
        let hits = rerank(candidates.clone(), &config, 2);
        assert_eq!(
            hits.iter().map(|hit| hit.text.as_str()).collect::<Vec<_>>(),
            ["a", "b"]
        );
        let mut reversed = candidates;
        reversed.reverse();
        assert_eq!(hits, rerank(reversed, &config, 2));
        assert!(rerank(Vec::new(), &config, 3).is_empty());
        assert!(rerank(vec![candidate("a", 0, 1.0)], &config, 0).is_empty());
    }

    #[test]
    fn zero_scores_only_get_recency() {
        let hits = rerank(
            vec![candidate("old", 0, 0.0), candidate("new", 24, 0.0)],
            &RerankConfig::default(),
            2,
        );
        assert_eq!(hits[0].text, "new");
        assert!((hits[0].score - 0.15).abs() < 0.00001);
        assert!((hits[1].score - 0.075).abs() < 0.00001);
    }
}
