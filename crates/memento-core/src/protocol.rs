use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IngestRequest {
    pub harness: String,
    pub project: String,
    pub session: String,
    pub items: Vec<IngestItem>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IngestItem {
    pub source_id: String,
    pub ts: DateTime<Utc>,
    pub role: Role,
    #[serde(default)]
    pub text: String,
    pub kind_hint: Option<Kind>,
    pub tool: Option<ToolCall>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    UserRequest,
    AssistantNote,
    Decision,
    Constraint,
    Task,
    FileChange,
    Command,
    FileRef,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub name: String,
    pub input: Value,
    pub output: Option<String>,
    pub ok: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngestResponse {
    pub segments: usize,
    pub skipped: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRecord {
    pub harness: String,
    pub project: String,
    pub session: String,
    pub parent: Option<String>,
    #[serde(default)]
    pub retention: Retention,
    pub title: Option<String>,
    pub created: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Retention {
    #[default]
    Continuation,
    Keep,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchRequest {
    pub harness: String,
    pub project: String,
    pub session: String,
    pub query: String,
    #[serde(default)]
    pub scope: Scope,
    #[serde(default = "default_search_limit")]
    pub k: usize,
    pub kinds: Option<Vec<Kind>>,
}

fn default_search_limit() -> usize {
    8
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Session,
    #[default]
    Lineage,
    Project,
    Global,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchHit {
    pub id: Uuid,
    pub score: f32,
    pub kind: Kind,
    pub text: String,
    pub session: String,
    pub ts: DateTime<Utc>,
    pub path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchResponse {
    pub hits: Vec<SearchHit>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DigestResponse {
    pub text: String,
    pub segments: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForgetRequest {
    pub harness: String,
    pub project: Option<String>,
    pub session: Option<String>,
    pub scope: ForgetScope,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForgetScope {
    Session,
    Lineage,
    Project,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForgetResponse {
    pub deleted: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub error: ErrorDetail,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorDetail {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Health {
    pub ok: bool,
    pub qdrant: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ingest_example_round_trips() {
        let example = json!({
            "harness": "opencode",
            "project": "proj_123",
            "session": "ses_abc",
            "items": [
                {
                    "source_id": "msg_1",
                    "ts": "2026-11-18T10:00:00Z",
                    "role": "user",
                    "text": "...",
                    "kind_hint": null,
                    "tool": null
                },
                {
                    "source_id": "msg_2:tool:call_7",
                    "ts": "2026-11-18T10:00:05Z",
                    "role": "tool",
                    "text": "",
                    "kind_hint": null,
                    "tool": {
                        "name": "edit",
                        "input": { "filePath": "src/a.rs" },
                        "output": "...",
                        "ok": true
                    }
                }
            ]
        });
        let request: IngestRequest = serde_json::from_value(example.clone()).unwrap();
        assert_eq!(serde_json::to_value(request).unwrap(), example);
    }

    #[test]
    fn defaults_are_applied_when_fields_are_absent() {
        let search: SearchRequest = serde_json::from_value(json!({
            "harness": "h", "project": "p", "session": "s", "query": "q"
        }))
        .unwrap();
        assert_eq!(search.scope, Scope::Lineage);
        assert_eq!(search.k, 8);
        let session: SessionRecord = serde_json::from_value(json!({
            "harness": "h", "project": "p", "session": "s"
        }))
        .unwrap();
        assert_eq!(session.retention, Retention::Continuation);
        let item: IngestItem = serde_json::from_value(json!({
            "source_id": "s", "ts": "2026-11-18T10:00:00Z", "role": "user"
        }))
        .unwrap();
        assert!(item.text.is_empty());
    }

    #[test]
    fn enums_use_snake_case_and_reject_reasoning() {
        assert_eq!(
            serde_json::to_value(Kind::FileChange).unwrap(),
            "file_change"
        );
        assert_eq!(serde_json::to_value(Scope::Global).unwrap(), "global");
        assert_eq!(serde_json::to_value(Retention::Keep).unwrap(), "keep");
        assert!(serde_json::from_str::<Role>("\"reasoning\"").is_err());
    }
}
