use std::collections::{BTreeMap, HashSet};

use chrono::{DateTime, Utc};

use crate::protocol::{DigestResponse, Kind};
use crate::segment::Segment;

#[derive(Debug, Clone)]
pub struct DigestInput {
    pub harness: String,
    pub project: String,
    pub lineage: Vec<String>,
    pub segments: Vec<Segment>,
}

pub fn merge_chunks(chunks: &[&str], max_overlap_chars: usize) -> String {
    let mut merged = String::new();
    for chunk in chunks {
        let suffix: Vec<char> = merged.chars().rev().take(max_overlap_chars).collect();
        let prefix: Vec<char> = chunk.chars().take(max_overlap_chars).collect();
        let overlap = (1..=suffix.len().min(prefix.len()))
            .rev()
            .find(|&length| suffix[..length].iter().rev().eq(prefix[..length].iter()))
            .unwrap_or(0);
        merged.extend(chunk.chars().skip(overlap));
    }
    merged
}

struct Entry {
    ts: DateTime<Utc>,
    source_id: String,
    chunk_index: u32,
    session: String,
    kind: Kind,
    text: String,
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

fn entries(segments: &[Segment]) -> Vec<Entry> {
    let mut groups: BTreeMap<(&str, &str), Vec<&Segment>> = BTreeMap::new();
    for segment in segments {
        groups
            .entry((&segment.session, &segment.source_id))
            .or_default()
            .push(segment);
    }
    let mut result = Vec::new();
    for ((session, source_id), mut chunks) in groups {
        chunks.sort_by(|left, right| {
            left.chunk_index
                .cmp(&right.chunk_index)
                .then_with(|| left.ts.cmp(&right.ts))
                .then_with(|| left.id.cmp(&right.id))
                .then_with(|| kind_order(left.kind).cmp(&kind_order(right.kind)))
                .then_with(|| left.text.cmp(&right.text))
        });
        let first = chunks[0];
        result.push(Entry {
            ts: first.ts,
            source_id: source_id.to_owned(),
            chunk_index: first.chunk_index,
            session: session.to_owned(),
            kind: first.kind,
            text: merge_chunks(
                &chunks
                    .iter()
                    .map(|chunk| chunk.text.as_str())
                    .collect::<Vec<_>>(),
                300,
            ),
        });
    }
    result.sort_by(|left, right| {
        left.ts
            .cmp(&right.ts)
            .then_with(|| left.source_id.cmp(&right.source_id))
            .then_with(|| left.chunk_index.cmp(&right.chunk_index))
            .then_with(|| left.session.cmp(&right.session))
    });
    result
}

fn list_item(text: &str) -> String {
    format!("- {}\n", text.replace('\n', "\n  "))
}

fn truncate_request(text: &str) -> String {
    let count = text.chars().count();
    if count <= 1500 {
        return text.to_owned();
    }
    let prefix: String = text.chars().take(1500).collect();
    format!("{prefix}...[truncated {} chars]", count - 1500)
}

fn append_list_section(
    output: &mut String,
    heading: &str,
    items: impl IntoIterator<Item = String>,
    budget: usize,
    chronological: bool,
) -> (usize, usize) {
    let heading = format!("\n## {heading}\n");
    let heading_len = heading.chars().count();
    let mut available = budget.saturating_sub(heading_len);
    let mut selected = Vec::new();
    for item in items {
        let length = item.chars().count();
        if length <= available && budget >= heading_len {
            available -= length;
            selected.push(item);
        }
    }
    if selected.is_empty() {
        return (0, 0);
    }
    if chronological {
        selected.reverse();
    }
    output.push_str(&heading);
    for item in &selected {
        output.push_str(item);
    }
    (budget - available, selected.len())
}

pub fn render_digest(input: &DigestInput, budget_chars: usize) -> DigestResponse {
    let mut text = format!(
        "# Session memory (memento)\nharness: {} | project: {} | lineage: {}\n",
        input.harness,
        input.project,
        input.lineage.join(" <- ")
    );
    let remaining = budget_chars.saturating_sub(text.chars().count());
    let shares = [35, 10, 35, 10, 10];
    let mut allocated = 0;
    let mut budgets = [0; 5];
    for (index, share) in shares.into_iter().enumerate() {
        let next = remaining / 100 * share + remaining % 100 * share / 100;
        budgets[index] = next;
        allocated += next;
    }
    budgets[4] += remaining - allocated;
    let entries = entries(&input.segments);
    let mut rendered = 0;
    let mut carry = 0;

    let budget = budgets[0] + carry;
    let (used, count) = append_list_section(
        &mut text,
        "Constraints and decisions",
        entries.iter().rev().filter_map(|entry| {
            let label = match entry.kind {
                Kind::Constraint => "constraint",
                Kind::Decision => "decision",
                _ => return None,
            };
            Some(list_item(&format!("[{label}] {}", entry.text)))
        }),
        budget,
        true,
    );
    rendered += count;
    carry = budget - used;

    let budget = budgets[1] + carry;
    let heading = "\n## Current tasks\n";
    let mut used = 0;
    if let Some(task) = entries.iter().rev().find(|entry| entry.kind == Kind::Task) {
        let heading_len = heading.chars().count();
        let available = budget.saturating_sub(heading_len + 1);
        if available > 0 {
            let content: String = task.text.chars().take(available).collect();
            text.push_str(heading);
            text.push_str(&content);
            text.push('\n');
            used = heading_len + content.chars().count() + 1;
            rendered += 1;
        }
    }
    carry = budget - used;

    let budget = budgets[2] + carry;
    let (used, count) = append_list_section(
        &mut text,
        "User requests",
        entries
            .iter()
            .rev()
            .filter(|entry| entry.kind == Kind::UserRequest)
            .map(|entry| list_item(&truncate_request(&entry.text))),
        budget,
        true,
    );
    rendered += count;
    carry = budget - used;

    let mut files: Vec<&Segment> = input
        .segments
        .iter()
        .filter(|segment| segment.kind == Kind::FileChange)
        .collect();
    files.sort_by(|left, right| {
        right
            .ts
            .cmp(&left.ts)
            .then_with(|| right.source_id.cmp(&left.source_id))
            .then_with(|| right.chunk_index.cmp(&left.chunk_index))
            .then_with(|| left.session.cmp(&right.session))
            .then_with(|| left.path.cmp(&right.path))
            .then_with(|| left.paths.cmp(&right.paths))
    });
    let mut seen = HashSet::new();
    let file_items = files
        .into_iter()
        .flat_map(|segment| segment.paths.iter())
        .filter(|path| seen.insert(*path))
        .map(|path| list_item(path));
    let budget = budgets[3] + carry;
    let (used, count) = append_list_section(&mut text, "Files changed", file_items, budget, false);
    rendered += count;
    carry = budget - used;

    let budget = budgets[4] + carry;
    let (_, count) = append_list_section(
        &mut text,
        "Recent notes",
        entries
            .iter()
            .rev()
            .filter(|entry| entry.kind == Kind::AssistantNote)
            .map(|entry| list_item(&entry.text)),
        budget,
        true,
    );
    rendered += count;
    DigestResponse {
        text,
        segments: rendered,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use uuid::Uuid;

    fn segment(kind: Kind, source: &str, seconds: i64, text: &str) -> Segment {
        Segment {
            id: Uuid::from_u128(source.bytes().map(u128::from).sum()),
            harness: "h".into(),
            project: "p".into(),
            session: "s".into(),
            kind,
            text: text.into(),
            source_id: source.into(),
            chunk_index: 0,
            ts: "2026-01-01T00:00:00Z".parse::<DateTime<Utc>>().unwrap()
                + Duration::seconds(seconds),
            tool: None,
            path: None,
            paths: Vec::new(),
        }
    }

    fn input(segments: Vec<Segment>) -> DigestInput {
        DigestInput {
            harness: "h".into(),
            project: "p".into(),
            lineage: vec!["s".into(), "parent".into()],
            segments,
        }
    }

    #[test]
    fn overlap_is_character_based() {
        assert_eq!(merge_chunks(&["abcde", "defgh"], 3), "abcdefgh");
        assert_eq!(merge_chunks(&["abc", "xyz"], 3), "abcxyz");
        assert_eq!(merge_chunks(&["aé🙂", "é🙂界"], 2), "aé🙂界");
        assert_eq!(merge_chunks(&["aa", "aa"], 0), "aaaa");
    }

    #[test]
    fn renders_all_sections_in_order_and_omits_empty_ones() {
        let mut file = segment(Kind::FileChange, "f", 4, "edit");
        file.path = Some("src/a.rs".into());
        file.paths = vec!["src/a.rs".into()];
        let digest = render_digest(
            &input(vec![
                segment(Kind::Decision, "d", 1, "choose\nthis"),
                segment(Kind::Task, "t", 2, "do work"),
                segment(Kind::UserRequest, "u", 3, "request"),
                file,
                segment(Kind::AssistantNote, "n", 5, "note"),
            ]),
            1000,
        );
        assert_eq!(
            digest.text,
            "# Session memory (memento)\nharness: h | project: p | lineage: s <- parent\n\n## Constraints and decisions\n- [decision] choose\n  this\n\n## Current tasks\ndo work\n\n## User requests\n- request\n\n## Files changed\n- src/a.rs\n\n## Recent notes\n- note\n"
        );
        assert_eq!(digest.segments, 5);
        let empty = render_digest(&input(vec![]), 1000);
        assert!(!empty.text.contains("##"));
        assert_eq!(empty.segments, 0);
    }

    #[test]
    fn file_paths_are_distinct_and_ordered_by_latest_change() {
        let mut old = segment(Kind::FileChange, "old", 0, "edit");
        old.path = Some("src/old.rs".into());
        old.paths = vec!["src/old.rs".into(), "src/shared.rs".into()];
        let mut new = segment(Kind::FileChange, "new", 1, "apply_patch");
        new.path = Some("src/invoice_worker.rs".into());
        new.paths = vec![
            "src/invoice_worker.rs".into(),
            "src/lib.rs".into(),
            "src/shared.rs".into(),
        ];
        let mut second_chunk = new.clone();
        second_chunk.chunk_index = 1;
        let data = input(vec![old, new, second_chunk]);
        let digest = render_digest(&data, 1000);
        assert!(digest.text.contains(
            "## Files changed\n- src/invoice_worker.rs\n- src/lib.rs\n- src/shared.rs\n- src/old.rs\n"
        ));
        assert_eq!(digest.segments, 4);
        let mut reversed = data;
        reversed.segments.reverse();
        assert_eq!(digest, render_digest(&reversed, 1000));
    }

    #[test]
    fn budget_flows_forward_and_newest_selection_renders_chronologically() {
        let data = input(vec![
            segment(Kind::UserRequest, "old", 0, &"o".repeat(60)),
            segment(Kind::UserRequest, "middle", 1, "middle"),
            segment(Kind::UserRequest, "new", 2, "new"),
        ]);
        let header_len = render_digest(&input(vec![]), 1000).text.chars().count();
        let digest = render_digest(&data, header_len + 60);
        assert!(!digest.text.contains(&"o".repeat(60)));
        assert!(digest.text.contains("- middle\n- new\n"));
        assert!(digest.text.chars().count() <= header_len + 60);
    }

    #[test]
    fn truncates_requests_and_tasks() {
        let data = input(vec![
            segment(Kind::UserRequest, "u", 0, &"é".repeat(1504)),
            segment(Kind::Task, "t", 1, &"x".repeat(3000)),
        ]);
        let digest = render_digest(&data, 5000);
        assert!(digest.text.contains("...[truncated 4 chars]"));
        assert!(!digest.text.contains(&"x".repeat(3000)));
        assert!(digest.text.chars().count() <= 5000);
    }

    #[test]
    fn chunks_are_rejoined_and_input_order_does_not_matter() {
        let first = segment(Kind::UserRequest, "u", 0, "hello é🙂");
        let mut second = first.clone();
        second.chunk_index = 1;
        second.text = "é🙂 world".into();
        second.id = Uuid::from_u128(42);
        let note = segment(Kind::AssistantNote, "n", 1, "note");
        let mut data = input(vec![second, note, first]);
        let expected = render_digest(&data, 1000);
        assert!(expected.text.contains("- hello é🙂 world\n"));
        assert_eq!(expected.segments, 2);
        data.segments.reverse();
        assert_eq!(expected, render_digest(&data, 1000));
    }
}
