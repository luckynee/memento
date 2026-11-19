use std::collections::HashSet;

use chrono::{DateTime, Utc};
use serde_json::Value;
use uuid::Uuid;

use crate::chunk::chunk_text;
use crate::ids::point_id;
use crate::protocol::{IngestItem, IngestRequest, Kind, Role, ToolCall};

#[derive(Debug, Clone)]
pub struct SegmentPolicy {
    pub scaffold_strip_tags: HashSet<String>,
    pub scaffold_unwrap_tags: HashSet<String>,
    pub ignored_tools: HashSet<String>,
    pub self_command_prefixes: Vec<String>,
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
            scaffold_strip_tags: tool_set(&[
                "system-reminder",
                "skill_content",
                "environment_context",
                "user_instructions",
                "command-message",
                "command-name",
                "local-command-stdout",
                "local-command-stderr",
                "local-command-caveat",
                "subagent",
                "memento-digest",
                "memento-memory",
                "memento-continuation",
            ]),
            scaffold_unwrap_tags: tool_set(&["command-args"]),
            ignored_tools: tool_set(&["skill", "memory_search", "question", "execute"]),
            self_command_prefixes: vec!["memento ".into(), "mementod ".into()],
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

struct ScaffoldTag<'a> {
    name: &'a str,
    closing: bool,
    end: usize,
}

fn scaffold_tag(text: &str, start: usize) -> Option<ScaffoldTag<'_>> {
    let rest = text.get(start + 1..)?;
    let (closing, rest) = match rest.strip_prefix('/') {
        Some(rest) => (true, rest),
        None => (false, rest),
    };
    let name_end = rest
        .bytes()
        .position(|byte| !byte.is_ascii_alphanumeric() && byte != b'-' && byte != b'_')
        .unwrap_or(rest.len());
    if name_end == 0 {
        return None;
    }
    let name = &rest[..name_end];
    let after_name = &rest[name_end..];
    if !after_name.starts_with('>') && !after_name.starts_with(char::is_whitespace) {
        return None;
    }
    let mut quote = None;
    for (offset, character) in after_name.char_indices() {
        match (quote, character) {
            (Some(expected), found) if expected == found => quote = None,
            (None, '\'' | '"') => quote = Some(character),
            (None, '>') => {
                let end = text.len() - after_name.len() + offset + 1;
                return Some(ScaffoldTag { name, closing, end });
            }
            _ => {}
        }
    }
    None
}

pub(crate) fn strip_scaffolding(text: &str, policy: &SegmentPolicy) -> String {
    let mut output = String::new();
    let mut position = 0;
    let mut stripped = Vec::new();
    while let Some(relative) = text[position..].find('<') {
        let start = position + relative;
        if stripped.is_empty() {
            output.push_str(&text[position..start]);
        }
        let Some(tag) = scaffold_tag(text, start) else {
            if stripped.is_empty() {
                output.push('<');
            }
            position = start + 1;
            continue;
        };
        let name = tag.name.to_ascii_lowercase();
        if policy.scaffold_strip_tags.contains(&name) {
            if tag.closing {
                if stripped.last() == Some(&name) {
                    stripped.pop();
                } else if stripped.is_empty() {
                    output.push_str(&text[start..tag.end]);
                }
            } else {
                stripped.push(name);
            }
        } else if stripped.is_empty() && !policy.scaffold_unwrap_tags.contains(&name) {
            output.push_str(&text[start..tag.end]);
        }
        position = tag.end;
    }
    if stripped.is_empty() {
        output.push_str(&text[position..]);
    }
    if output == text {
        output
    } else {
        output.trim().to_string()
    }
}

pub fn strip_default_scaffolding(text: &str) -> String {
    strip_scaffolding(text, &SegmentPolicy::default())
}

pub(crate) fn command_exit_header(line: &str) -> Option<&str> {
    let line = line.trim();
    if line == "Exit status: failed" {
        return Some(line);
    }
    let value = line
        .strip_prefix("Process exited with code ")
        .or_else(|| line.strip_prefix("Exit code:"))?;
    value.trim().parse::<i64>().ok().map(|_| line)
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
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SegmentOutcome {
    pub segments: Vec<Segment>,
    pub skipped: usize,
}

pub fn segment_items(request: &IngestRequest, policy: &SegmentPolicy) -> SegmentOutcome {
    let mut outcome = SegmentOutcome::default();
    for item in &request.items {
        if item.injected {
            outcome.skipped += 1;
            continue;
        }
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
                paths: rendered.paths.clone(),
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
    paths: Vec<String>,
}

fn render_item(item: &IngestItem, policy: &SegmentPolicy) -> Option<RenderedItem> {
    match item.role {
        Role::User => Some(RenderedItem {
            kind: match item.kind_hint {
                Some(kind @ (Kind::Decision | Kind::Constraint)) => kind,
                _ => Kind::UserRequest,
            },
            text: strip_scaffolding(&item.text, policy),
            tool: None,
            path: None,
            paths: Vec::new(),
        }),
        Role::Assistant => Some(RenderedItem {
            kind: match item.kind_hint {
                Some(kind @ (Kind::Decision | Kind::Constraint | Kind::Task)) => kind,
                _ => Kind::AssistantNote,
            },
            text: strip_scaffolding(&item.text, policy),
            tool: None,
            path: None,
            paths: Vec::new(),
        }),
        Role::Tool => render_tool(item.tool.as_ref()?, policy),
    }
}

fn render_tool(tool: &ToolCall, policy: &SegmentPolicy) -> Option<RenderedItem> {
    let name = tool.name.to_lowercase();
    if policy.ignored_tools.contains(&name) {
        return None;
    }
    let paths = tool_paths(&name, &tool.input);
    let path = paths.first().cloned();
    let (kind, text) = if policy.edit_tools.contains(&name) {
        let input = truncate_prefix(&render_edit_body(&tool.input), policy.edit_input_max_chars);
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
        if policy
            .self_command_prefixes
            .iter()
            .any(|prefix| command.trim_start().starts_with(prefix))
        {
            return None;
        }
        let output_limit = if tool.ok {
            policy.successful_output_max_chars
        } else {
            policy.failed_output_max_chars
        };
        let original_output = tool.output.as_deref().unwrap_or_default();
        let mut output = tool
            .output
            .as_deref()
            .map(|output| truncate_suffix(output, output_limit))
            .unwrap_or_default();
        if !tool.ok
            && !output
                .lines()
                .any(|line| command_exit_header(line).is_some())
        {
            let header = original_output
                .lines()
                .find_map(command_exit_header)
                .unwrap_or("Exit status: failed");
            output = if output.is_empty() {
                header.to_string()
            } else {
                format!("{header}\n{output}")
            };
        }
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
        paths,
    })
}

fn render_edit_body(input: &Value) -> String {
    if let Some(patch) = ["patchText", "patch", "diff", "input"]
        .iter()
        .find_map(|key| input.get(key).and_then(Value::as_str))
    {
        return patch.to_owned();
    }
    if let (Some(old), Some(new)) = (
        input.get("oldString").and_then(Value::as_str),
        input.get("newString").and_then(Value::as_str),
    ) {
        return format!("--- old\n{old}\n+++ new\n{new}");
    }
    if let Some(content) = input.get("content").and_then(Value::as_str) {
        return content.to_owned();
    }
    input.to_string()
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
            injected: false,
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
        for (hint, expected) in [
            (Some(Kind::Decision), Kind::Decision),
            (Some(Kind::Constraint), Kind::Constraint),
            (Some(Kind::Task), Kind::UserRequest),
            (Some(Kind::Command), Kind::UserRequest),
            (None, Kind::UserRequest),
        ] {
            let mut user = item(Role::User, "user text");
            user.kind_hint = hint;
            assert_eq!(
                run(vec![user], &SegmentPolicy::default()).segments[0].kind,
                expected
            );
        }
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
                    json!({"filePath": "é.rs", "path": "other", "diff": "🙂🙂🙂🙂🙂🙂🙂🙂🙂"}),
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
            assert_eq!(
                failure.segments[0].text,
                "run\nExit status: failed\nabc...[truncated 3 chars]"
            );
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
    fn failed_commands_preserve_or_add_exit_status_after_truncation() {
        let policy = SegmentPolicy {
            failed_output_max_chars: 10,
            ..SegmentPolicy::default()
        };
        for (output, expected) in [
            (None, "run\nExit status: failed".to_string()),
            (
                Some("Exit code: 7\nlong output tail"),
                "run\nExit code: 7\nutput tail...[truncated 19 chars]".to_string(),
            ),
        ] {
            let outcome = run(
                vec![tool_item("bash", json!({"command": "run"}), output, false)],
                &policy,
            );
            assert_eq!(outcome.segments[0].text, expected);
        }
        let outcome = run(
            vec![tool_item(
                "bash",
                json!({"command": "run"}),
                Some("Exit code: 7"),
                false,
            )],
            &SegmentPolicy::default(),
        );
        assert_eq!(outcome.segments[0].text, "run\nExit code: 7");
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
            assert_eq!(
                segment.paths,
                ["src/a.rs", "src/b.rs", "src/c.rs", "src/d.rs"]
            );
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
        assert_eq!(outcome.segments[0].paths, ["explicit", "unified.rs"]);
        assert!(
            outcome.segments[0]
                .text
                .starts_with("patch explicit, unified.rs\n")
        );
    }

    #[test]
    fn file_change_bodies_render_raw_inputs_and_fallback_json() {
        let policy = SegmentPolicy::default();
        for key in ["patchText", "patch", "diff", "input"] {
            let outcome = run(
                vec![tool_item(
                    "patch",
                    json!({key: "line 1\nline 2"}),
                    None,
                    true,
                )],
                &policy,
            );
            assert_eq!(outcome.segments[0].text, "patch \nline 1\nline 2");
        }
        let cases = [
            (
                json!({"path": "a", "oldString": "before\nold", "newString": "after\nnew"}),
                "edit a\n--- old\nbefore\nold\n+++ new\nafter\nnew",
            ),
            (
                json!({"path": "a", "content": "first\nsecond"}),
                "edit a\nfirst\nsecond",
            ),
            (
                json!({"path": "a", "other": 42}),
                "edit a\n{\"other\":42,\"path\":\"a\"}",
            ),
            (
                json!({"path": "a", "patch": "patch", "patchText": "preferred", "content": "ignored"}),
                "edit a\npreferred",
            ),
        ];
        for (input, expected) in cases {
            let outcome = run(vec![tool_item("edit", input, None, true)], &policy);
            assert_eq!(outcome.segments[0].text, expected);
        }
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

    #[test]
    fn scaffold_blocks_are_stripped_from_both_text_roles() {
        let policy = SegmentPolicy::default();
        for tag in &policy.scaffold_strip_tags {
            for role in [Role::User, Role::Assistant] {
                let text = format!("before <{tag} key=\"a > b\">\nsecret\n</{tag}> after");
                let outcome = run(vec![item(role, &text)], &policy);
                assert_eq!(outcome.segments[0].text, "before  after", "{tag}");
            }
        }
    }

    #[test]
    fn scaffold_unwrap_nested_adjacent_case_and_unclosed() {
        let policy = SegmentPolicy::default();
        let cases = [
            ("a<command-args x='1'>keep</command-args>b", "akeepb"),
            ("a<SYSTEM-REMINDER>hide</sYsTeM-rEmInDeR>b", "ab"),
            (
                "a<subagent>outer<subagent>inner</subagent>end</subagent>b",
                "ab",
            ),
            (
                "a<subagent>x</subagent><skill_content>y</skill_content>b",
                "ab",
            ),
            (
                "a<subagent>x<system-reminder>y</system-reminder>z</subagent>b",
                "ab",
            ),
            ("a<system-reminder attr='x'>unfinished", "a"),
            (
                "a<system-reminderish>keep</system-reminderish>b",
                "a<system-reminderish>keep</system-reminderish>b",
            ),
            ("é<COMMAND-ARGS>🙂</COMMAND-ARGS>界", "é🙂界"),
        ];
        for (text, expected) in cases {
            assert_eq!(strip_scaffolding(text, &policy), expected);
        }
    }

    #[test]
    fn injected_empty_ignored_and_self_commands_are_skipped() {
        let mut injected = item(Role::User, "injected");
        injected.injected = true;
        let mut policy = SegmentPolicy::default();
        policy.edit_tools.insert("skill".into());
        let mut items = vec![
            injected,
            item(Role::User, "<system-reminder>hidden</system-reminder>"),
            item(Role::Assistant, "<subagent>unclosed"),
        ];
        for name in ["SKILL", "memory_search", "question", "execute"] {
            items.push(tool_item(name, json!({"path": "x"}), Some("output"), true));
        }
        for command in ["  memento digest", "\nmementod serve"] {
            items.push(tool_item(
                "shell",
                json!({"command": command}),
                Some("output"),
                true,
            ));
        }
        items.push(tool_item(
            "shell",
            json!({"command": "memento-other"}),
            None,
            true,
        ));
        let outcome = run(items, &policy);
        assert_eq!(outcome.skipped, 9);
        assert_eq!(outcome.segments.len(), 1);
        assert_eq!(outcome.segments[0].text, "memento-other");
    }
}
