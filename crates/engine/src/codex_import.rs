use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde_json::Value;
use zeron_doc::{MessagePart, MessageRole, MessageStatus, SessionMessageEntry};

use crate::doc_host::DocHost;
use crate::workspace_host::WorkspaceHost;
use crate::{EngineError, new_id};

struct ImportedSession {
    id: String,
    cwd: String,
    title: Option<String>,
    entries: Vec<SessionMessageEntry>,
    last_message_at: Option<i64>,
}

pub fn import(
    codex_home: &Path,
    workspace: &WorkspaceHost,
    docs: &DocHost,
    device_id: &str,
) -> Result<usize, EngineError> {
    let mut imported = 0;
    let mut files = Vec::new();
    collect_jsonl(&codex_home.join("sessions"), &mut files);
    files.sort();
    for file in files {
        let Some(session) = parse_session(&file)? else {
            continue;
        };
        if session.entries.is_empty() || workspace.chat(&chat_id(&session.id))?.is_some() {
            continue;
        }
        let space_id = space_id(&session.cwd);
        if workspace.space(&space_id)?.is_none() {
            workspace.create_space(&space_id, &workspace.device_id(), &session.cwd, None, false)?;
        }
        let chat_id = chat_id(&session.id);
        workspace.create_chat(
            &chat_id,
            Some(&space_id),
            None,
            Some(zeron_proto::ChatConfig {
                harness: zeron_proto::HarnessId::Codex,
                model: None,
                reasoning: None,
                model_options: Default::default(),
                sandbox: zeron_proto::SandboxLevel::WorkspaceWrite,
            }),
            Some(session.cwd.clone()),
        )?;
        if let Some(title) = session.title.as_deref() {
            workspace.rename_chat(&chat_id, title)?;
        }
        workspace.set_chat_harness_session(&chat_id, &session.id, &session.cwd);
        if let Some(at) = session.last_message_at {
            workspace.set_chat_activity(&chat_id, Some(at), None)?;
        }
        let handle = docs.open(&chat_id)?;
        for mut entry in session.entries {
            entry.device_id = device_id.to_owned();
            handle.doc().push_message(&entry)?;
        }
        imported += 1;
    }
    Ok(imported)
}

fn collect_jsonl(dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_jsonl(&path, files);
        } else if path.extension().is_some_and(|ext| ext == "jsonl") {
            files.push(path);
        }
    }
}

fn parse_session(path: &Path) -> Result<Option<ImportedSession>, EngineError> {
    let text = std::fs::read_to_string(path)?;
    let mut id = None;
    let mut cwd = None;
    let mut title = None;
    let mut entries = Vec::new();
    let mut last_message_at = None;
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) != Some("event_msg") {
            if value.get("type").and_then(Value::as_str) == Some("session_meta") {
                id = value
                    .pointer("/payload/session_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                cwd = value
                    .pointer("/payload/cwd")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
            continue;
        }
        if value.pointer("/payload/type").and_then(Value::as_str) != Some("item_completed") {
            continue;
        }
        let item = value.pointer("/payload/item").unwrap_or(&Value::Null);
        let role = match item.get("type").and_then(Value::as_str) {
            Some("UserMessage") => MessageRole::User,
            Some("AgentMessage") => MessageRole::Assistant,
            _ => continue,
        };
        let text = item
            .get("content")
            .and_then(Value::as_array)
            .map(|parts| {
                parts
                    .iter()
                    .filter_map(|part| part.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("")
            })
            .unwrap_or_default();
        if text.trim().is_empty() {
            continue;
        }
        let timestamp = value
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_time)
            .unwrap_or_else(crate::now_ms);
        if role == MessageRole::User && title.is_none() {
            title = Some(
                text.lines()
                    .next()
                    .unwrap_or("Codex chat")
                    .chars()
                    .take(80)
                    .collect(),
            );
        }
        last_message_at = Some(last_message_at.unwrap_or(timestamp).max(timestamp));
        entries.push(SessionMessageEntry {
            id: new_id(),
            role,
            parts: vec![MessagePart::Text {
                id: "text".into(),
                text,
            }],
            created_at: timestamp,
            device_id: "codex-import".into(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
            duration_ms: None,
        });
    }
    Ok(Some(ImportedSession {
        id: id.unwrap_or_else(|| {
            path.file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_owned()
        }),
        cwd: cwd.unwrap_or_else(|| ".".into()),
        title,
        entries,
        last_message_at,
    }))
}

fn parse_time(value: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|time| time.with_timezone(&Utc).timestamp_millis())
}

fn chat_id(session_id: &str) -> String {
    format!("codex-{session_id}")
}

fn space_id(cwd: &str) -> String {
    let mut id = String::from("codex-space-");
    for ch in cwd.chars() {
        if id.len() >= 120 {
            break;
        }
        id.push(if ch.is_ascii_alphanumeric() {
            ch.to_ascii_lowercase()
        } else {
            '-'
        });
    }
    id.trim_end_matches('-').to_owned()
}

#[cfg(test)]
mod tests {
    use super::parse_session;

    #[test]
    fn parses_completed_codex_messages() {
        let path = std::env::temp_dir().join(format!("zeron-codex-{}.jsonl", std::process::id()));
        std::fs::write(
            &path,
            concat!(
                "{\"type\":\"session_meta\",\"payload\":{\"session_id\":\"thread-1\",\"cwd\":\"C:\\\\work\"}}\n",
                "{\"timestamp\":\"2026-01-01T00:00:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"item_completed\",\"item\":{\"type\":\"UserMessage\",\"content\":[{\"type\":\"Text\",\"text\":\"hello\"}]}}}\n",
                "{\"timestamp\":\"2026-01-01T00:00:01Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"item_completed\",\"item\":{\"type\":\"AgentMessage\",\"content\":[{\"type\":\"Text\",\"text\":\"world\"}]}}}\n"
            ),
        )
        .unwrap();
        let session = parse_session(&path).unwrap().unwrap();
        std::fs::remove_file(path).unwrap();
        assert_eq!(session.id, "thread-1");
        assert_eq!(session.cwd, "C:\\work");
        assert_eq!(session.entries.len(), 2);
    }
}
