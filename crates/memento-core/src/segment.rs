use std::collections::HashSet;

use chrono::{DateTime, Utc};
use serde_json::Value;
use uuid::Uuid;

use crate::chunk::chunk_text;
use crate::ids::point_id;
use crate::protocol::{IngestItem, IngestRequest, Kind, Role, ToolCall};

#[derive(Debug, Clone)]
pub struct SegmentPolicy {
    pub edit_tools: HashSet<String>,
    pub shell_tools: HashSet<String>,
    pub read_tools: HashSet<String>,
    pub todo_tools: HashSet<String>,
    pub subagent_tools: HashSet<String>,
    pub edit_input_max_chars: usize,
    pub failed_output_max_chars: usize,
    pub successful_output_max_chars: usize,
    pub subagent_output_max_chars: usize,
    pub chunk_max_chars: usize,
    pub chunk_overlap_chars: usize,
}

impl Default for SegmentPolicy {
    fn default() -> Self {
        Self {
            edit_tools: tool_set(&["edit", "write", "patch", "apply_patch", "multiedit"]),
            shell_tools: tool_set(&["bash", "shell"]),
            read_tools: tool_set(&["read", "grep", "glob", "list", "webfetch", "websearch"]),
            todo_tools: tool_set(&["todowrite"]),
            subagent_tools: tool_set(&["task", "subagent"]),
            edit_input_max_chars: 2000,
            failed_output_max_chars: 1000,
            successful_output_max_chars: 400,
            subagent_output_max_chars: 2000,
            chunk_max_chars: 1000,
            chunk_overlap_chars: 150,
        }
    }
}

fn tool_set(names: &[&str]) -> HashSet<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub id: Uuid,
    pub harness: String,
    pub project: String,
    pub session: String,
    pub kind: Kind,
    pub text: String,
    pub source_id: String,
    pub chunk_index: u32,
    pub ts: DateTime<Utc>,
    pub tool: Option<String>,
    pub path: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SegmentOutcome {
    pub segments: Vec<Segment>,
    pub skipped: usize,
}

pub fn segment_items(request: &IngestRequest, policy: &SegmentPolicy) -> SegmentOutcome {
    let mut outcome = SegmentOutcome::default();
    for item in &request.items {
        let Some(rendered) = render_item(item, policy) else {
            outcome.skipped += 1;
            continue;
        };
        if rendered.text.trim().is_empty() {
            outcome.skipped += 1;
            continue;
        }
        let chunks = chunk_text(
            &rendered.text,
            policy.chunk_max_chars,
            policy.chunk_overlap_chars,
        );
        if chunks.is_empty() {
            outcome.skipped += 1;
            continue;
        }
        for (index, text) in chunks.into_iter().enumerate() {
            let chunk_index = u32::try_from(index).expect("segment chunk index exceeds u32");
            outcome.segments.push(Segment {
                id: point_id(
                    &request.harness,
                    &request.session,
                    &item.source_id,
                    chunk_index,
                ),
                harness: request.harness.clone(),
                project: request.project.clone(),
                session: request.session.clone(),
                kind: rendered.kind,
                text,
                source_id: item.source_id.clone(),
                chunk_index,
                ts: item.ts,
                tool: rendered.tool.clone(),
                path: rendered.path.clone(),
            });
        }
    }
    outcome
}

struct RenderedItem {
    kind: Kind,
    text: String,
    tool: Option<String>,
    path: Option<String>,
}

fn render_item(item: &IngestItem, policy: &SegmentPolicy) -> Option<RenderedItem> {
    match item.role {
        Role::User => Some(RenderedItem {
            kind: Kind::UserRequest,
            text: item.text.clone(),
            tool: None,
            path: None,
        }),
        Role::Assistant => Some(RenderedItem {
            kind: match item.kind_hint {
                Some(kind @ (Kind::Decision | Kind::Constraint | Kind::Task)) => kind,
                _ => Kind::AssistantNote,
            },
            text: item.text.clone(),
            tool: None,
            path: None,
        }),
        Role::Tool => render_tool(item.tool.as_ref()?, policy),
    }
}

fn render_tool(tool: &ToolCall, policy: &SegmentPolicy) -> Option<RenderedItem> {
    let name = tool.name.to_lowercase();
    let paths = tool_paths(&name, &tool.input);
    let path = paths.first().cloned();
    let (kind, text) = if policy.edit_tools.contains(&name) {
        let input = truncate_prefix(&tool.input.to_string(), policy.edit_input_max_chars);
        (
            Kind::FileChange,
            format!("{} {}\n{}", tool.name, paths.join(", "), input),
        )
    } else if policy.shell_tools.contains(&name) {
        let command = tool
            .input
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or("");
        let output_limit = if tool.ok {
            policy.successful_output_max_chars
        } else {
            policy.failed_output_max_chars
        };
        let output = tool
            .output
            .as_deref()
            .map(|output| truncate_suffix(output, output_limit))
            .unwrap_or_default();
        let text = if output.is_empty() {
            command.to_owned()
        } else if command.is_empty() {
            output
        } else {
            format!("{command}\n{output}")
        };
        (Kind::Command, text)
    } else if policy.read_tools.contains(&name) {
        (
            Kind::FileRef,
            format!("{} {}", tool.name, path.as_deref().unwrap_or("")),
        )
    } else if policy.todo_tools.contains(&name) {
        let text = tool
            .input
            .get("todos")
            .and_then(Value::as_array)
            .map(|todos| {
                todos
                    .iter()
                    .filter_map(|todo| {
                        let content = todo.get("content")?.as_str()?;
                        let status = todo.get("status")?.as_str()?;
                        Some(format!("- [{status}] {content}"))
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        (Kind::Task, text)
    } else if policy.subagent_tools.contains(&name) {
        let output = tool
            .output
            .as_deref()
            .filter(|output| !output.trim().is_empty())?;
        (
            Kind::AssistantNote,
            format!(
                "{}\n{}",
                tool.name,
                truncate_prefix(output, policy.subagent_output_max_chars)
            ),
        )
    } else {
        return None;
    };
    Some(RenderedItem {
        kind,
        text,
        tool: Some(tool.name.clone()),
        path,
    })
}

fn tool_paths(name: &str, input: &Value) -> Vec<String> {
    let keys: &[&str] = match name {
        "webfetch" => &["url"],
        "websearch" => &["query"],
        _ => &["filePath", "path", "file", "pattern"],
    };
    let mut paths: Vec<String> = keys
        .iter()
        .find_map(|key| input.get(key).and_then(Value::as_str))
        .map(str::to_owned)
        .into_iter()
        .collect();
    if name == "webfetch" || name == "websearch" {
        return paths;
    }
    for key in ["patch", "patchText", "diff", "input"] {
        if let Some(patch) = input.get(key).and_then(Value::as_str) {
            for line in patch.lines() {
                let path = [
                    "*** Add File: ",
                    "*** Update File: ",
                    "*** Delete File: ",
                    "*** Move to: ",
                    "+++ b/",
                ]
                .iter()
                .find_map(|prefix| line.strip_prefix(prefix));
                if let Some(path) = path.filter(|path| !path.is_empty())
                    && !paths.iter().any(|existing| existing == path)
                {
                    paths.push(path.to_owned());
                }
            }
        }
    }
    paths
}

fn truncate_prefix(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_owned();
    }
    let prefix: String = text.chars().take(max_chars).collect();
    format!("{prefix}...[truncated {} chars]", count - max_chars)
}

fn truncate_suffix(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_owned();
    }
    let suffix: String = text.chars().skip(count - max_chars).collect();
    format!("{suffix}...[truncated {} chars]", count - max_chars)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn item(role: Role, text: &str) -> IngestItem {
        IngestItem {
            source_id: "source".into(),
            ts: "2026-11-18T10:00:00Z".parse().unwrap(),
            role,
            text: text.into(),
            kind_hint: None,
            tool: None,
        }
    }

    fn tool_item(name: &str, input: Value, output: Option<&str>, ok: bool) -> IngestItem {
        let mut item = item(Role::Tool, "ignored");
        item.tool = Some(ToolCall {
            name: name.into(),
            input,
            output: output.map(str::to_owned),
            ok,
        });
        item
    }

    fn run(items: Vec<IngestItem>, policy: &SegmentPolicy) -> SegmentOutcome {
        segment_items(
            &IngestRequest {
                harness: "h".into(),
                project: "p".into(),
                session: "s".into(),
                items,
            },
            policy,
        )
    }

    #[test]
    fn user_and_assistant_hints() {
        let mut items = vec![item(Role::User, "request")];
        for hint in [
            None,
            Some(Kind::Decision),
            Some(Kind::Constraint),
            Some(Kind::Task),
            Some(Kind::Command),
        ] {
            let mut assistant = item(Role::Assistant, "note");
            assistant.kind_hint = hint;
            items.push(assistant);
        }
        let outcome = run(items, &SegmentPolicy::default());
        assert_eq!(
            outcome.segments.iter().map(|s| s.kind).collect::<Vec<_>>(),
            vec![
                Kind::UserRequest,
                Kind::AssistantNote,
                Kind::Decision,
                Kind::Constraint,
                Kind::Task,
                Kind::AssistantNote
            ]
        );
        assert_eq!(outcome.segments[0].text, "request");
        assert_eq!(outcome.skipped, 0);
    }

    #[test]
    fn edit_tools_capture_path_and_truncate_input() {
        let policy = SegmentPolicy {
            edit_input_max_chars: 8,
            ..SegmentPolicy::default()
        };
        for name in ["EDIT", "write", "patch", "apply_patch", "multiedit"] {
            let outcome = run(
                vec![tool_item(
                    name,
                    json!({"filePath": "é.rs", "path": "other", "diff": "🙂🙂🙂"}),
                    Some("ignored"),
                    true,
                )],
                &policy,
            );
            let segment = &outcome.segments[0];
            assert_eq!(segment.kind, Kind::FileChange);
            assert_eq!(segment.path.as_deref(), Some("é.rs"));
            assert!(segment.text.starts_with(&format!("{name} é.rs\n")));
            assert!(segment.text.contains("...[truncated "));
            assert!(!segment.text.contains("ignored"));
        }
        assert_eq!(
            truncate_prefix("é界🙂abc", 3),
            "é界🙂...[truncated 3 chars]"
        );
    }

    #[test]
    fn shell_tools_include_output_tail_for_both_statuses() {
        let policy = SegmentPolicy {
            failed_output_max_chars: 3,
            successful_output_max_chars: 4,
            ..SegmentPolicy::default()
        };
        for name in ["bash", "SHELL"] {
            let success = run(
                vec![tool_item(
                    name,
                    json!({"command": "run"}),
                    Some("secret output"),
                    true,
                )],
                &policy,
            );
            assert_eq!(success.segments[0].kind, Kind::Command);
            assert_eq!(success.segments[0].text, "run\ntput...[truncated 9 chars]");
            let failure = run(
                vec![tool_item(
                    name,
                    json!({"command": "run"}),
                    Some("é界🙂abc"),
                    false,
                )],
                &policy,
            );
            assert_eq!(failure.segments[0].text, "run\nabc...[truncated 3 chars]");
            for output in [None, Some("")] {
                let outcome = run(
                    vec![tool_item(name, json!({"command": "run"}), output, true)],
                    &policy,
                );
                assert_eq!(outcome.segments[0].text, "run");
            }
        }
    }

    #[test]
    fn default_limits_and_subagent_reports() {
        let policy = SegmentPolicy::default();
        assert_eq!(
            (policy.chunk_max_chars, policy.chunk_overlap_chars),
            (1000, 150)
        );
        assert_eq!(
            (
                policy.successful_output_max_chars,
                policy.failed_output_max_chars
            ),
            (400, 1000)
        );
        assert_eq!(
            (
                policy.edit_input_max_chars,
                policy.subagent_output_max_chars
            ),
            (2000, 2000)
        );
        for name in ["task", "SUBAGENT"] {
            let output = "é".repeat(2001);
            let outcome = run(
                vec![tool_item(name, json!({}), Some(&output), true)],
                &policy,
            );
            assert!(outcome.segments.len() > 1);
            assert!(
                outcome
                    .segments
                    .iter()
                    .all(|segment| segment.kind == Kind::AssistantNote)
            );
            assert!(outcome.segments[0].text.starts_with(&format!("{name}\n")));
            assert!(
                outcome
                    .segments
                    .last()
                    .unwrap()
                    .text
                    .ends_with("...[truncated 1 chars]")
            );
            for empty in [None, Some(""), Some(" \n")] {
                let outcome = run(vec![tool_item(name, json!({}), empty, true)], &policy);
                assert_eq!(outcome.skipped, 1);
                assert!(outcome.segments.is_empty());
            }
        }
    }

    #[test]
    fn patch_paths_are_ordered_deduplicated_and_recorded() {
        let patch = "*** Begin Patch\n*** Update File: src/a.rs\n*** Add File: src/b.rs\n*** Move to: src/c.rs\n*** Delete File: src/a.rs\n+++ b/src/d.rs\n*** End Patch";
        for key in ["patch", "patchText", "diff", "input"] {
            let outcome = run(
                vec![tool_item("apply_patch", json!({key: patch}), None, true)],
                &SegmentPolicy::default(),
            );
            let segment = &outcome.segments[0];
            assert_eq!(segment.path.as_deref(), Some("src/a.rs"));
            assert!(
                segment
                    .text
                    .starts_with("apply_patch src/a.rs, src/b.rs, src/c.rs, src/d.rs\n")
            );
        }
        let outcome = run(
            vec![tool_item(
                "patch",
                json!({"path": "explicit", "diff": "+++ b/unified.rs\n+change"}),
                None,
                true,
            )],
            &SegmentPolicy::default(),
        );
        assert_eq!(outcome.segments[0].path.as_deref(), Some("explicit"));
        assert!(
            outcome.segments[0]
                .text
                .starts_with("patch explicit, unified.rs\n")
        );
    }

    #[test]
    fn file_changes_and_commands_are_truncated_before_chunking() {
        let policy = SegmentPolicy::default();
        let edit = run(
            vec![tool_item(
                "edit",
                json!({"path": "file", "diff": "x".repeat(2100)}),
                None,
                true,
            )],
            &policy,
        );
        assert!(edit.segments.len() > 1);
        assert!(edit.segments.iter().all(
            |segment| segment.kind == Kind::FileChange && segment.text.chars().count() <= 1000
        ));
        assert!(
            edit.segments
                .last()
                .unwrap()
                .text
                .contains("...[truncated ")
        );
        let output = "é".repeat(1200);
        let command = run(
            vec![tool_item(
                "bash",
                json!({"command": "x".repeat(1100)}),
                Some(&output),
                false,
            )],
            &policy,
        );
        assert!(command.segments.len() > 1);
        assert!(
            command.segments.iter().all(
                |segment| segment.kind == Kind::Command && segment.text.chars().count() <= 1000
            )
        );
        assert!(
            command
                .segments
                .last()
                .unwrap()
                .text
                .contains("...[truncated 200 chars]")
        );
    }

    #[test]
    fn configurable_tool_sets_and_unknown_tools() {
        let mut policy = SegmentPolicy::default();
        policy.subagent_tools.insert("custom".into());
        let outcome = run(
            vec![
                tool_item("CUSTOM", json!({}), Some("report"), true),
                tool_item("mcp", json!({}), Some("ignored"), true),
                tool_item("lsp", json!({}), Some("ignored"), true),
            ],
            &policy,
        );
        assert_eq!(outcome.segments.len(), 1);
        assert_eq!(outcome.segments[0].text, "CUSTOM\nreport");
        assert_eq!(outcome.skipped, 2);
    }

    #[test]
    fn read_tools_record_only_target_and_use_path_precedence() {
        for (name, input, target) in [
            (
                "read",
                json!({"filePath": "first", "path": "second"}),
                "first",
            ),
            ("grep", json!({"path": "dir", "pattern": "needle"}), "dir"),
            ("glob", json!({"pattern": "*.rs"}), "*.rs"),
            ("list", json!({"file": "dir"}), "dir"),
            (
                "webfetch",
                json!({"url": "https://example.test", "path": "ignored"}),
                "https://example.test",
            ),
            (
                "websearch",
                json!({"query": "search", "pattern": "ignored"}),
                "search",
            ),
        ] {
            let outcome = run(
                vec![tool_item(name, input, Some("never stored"), true)],
                &SegmentPolicy::default(),
            );
            let segment = &outcome.segments[0];
            assert_eq!(segment.kind, Kind::FileRef);
            assert_eq!(segment.path.as_deref(), Some(target));
            assert_eq!(segment.text, format!("{name} {target}"));
        }
    }

    #[test]
    fn todos_render_and_unrecognized_or_empty_items_are_skipped() {
        let todo = tool_item(
            "TODOWRITE",
            json!({"todos": [
                {"content": "first", "status": "pending"},
                {"content": "second", "status": "completed"}
            ]}),
            None,
            true,
        );
        let outcome = run(
            vec![
                todo,
                tool_item("unknown", json!({}), None, true),
                item(Role::User, " \n "),
                item(Role::Tool, ""),
                tool_item("todowrite", json!({"todos": []}), None, true),
            ],
            &SegmentPolicy::default(),
        );
        assert_eq!(outcome.segments.len(), 1);
        assert_eq!(outcome.segments[0].kind, Kind::Task);
        assert_eq!(
            outcome.segments[0].text,
            "- [pending] first\n- [completed] second"
        );
        assert_eq!(outcome.skipped, 4);
    }

    #[test]
    fn chunks_have_stable_ids_and_metadata() {
        let policy = SegmentPolicy {
            chunk_max_chars: 5,
            chunk_overlap_chars: 2,
            ..SegmentPolicy::default()
        };
        let outcome = run(vec![item(Role::User, "abcdefghij")], &policy);
        assert_eq!(outcome.segments.len(), 3);
        for (index, segment) in outcome.segments.iter().enumerate() {
            assert_eq!(segment.chunk_index, index as u32);
            assert_eq!(segment.id, point_id("h", "s", "source", index as u32));
            assert_eq!(
                (&*segment.harness, &*segment.project, &*segment.session),
                ("h", "p", "s")
            );
            assert_eq!(segment.source_id, "source");
        }
    }
}
