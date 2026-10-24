//! Durable store for the Open Harness UI — the nanobot-pattern persistence
//! layer: saved conversations, long-term memory, and agent task results.
//!
//! Everything is plain files under `<app root>/data/`:
//!   data/sessions/<id>.json   full chat transcripts (server-side mirror of
//!                             the browser's localStorage, survives a new
//!                             browser/device and a quota-full localStorage)
//!   data/memory/MEMORY.md     long-term memory the agent reads every chat
//!                             and can extend with its `remember` tool
//!   data/tasks/<id>.json      completed agent-run transcripts for later work
//!
//! IDs are sanitized to `[A-Za-z0-9_-]` before they ever touch a path, and
//! writes go through a tmp-file + rename so a crash mid-write can't truncate
//! an existing record.

use serde_json::{json, Value};
use std::io::Write;
use std::path::PathBuf;

pub fn data_dir() -> PathBuf {
    crate::root_dir().join("data")
}

fn sessions_dir() -> PathBuf {
    data_dir().join("sessions")
}

fn tasks_dir() -> PathBuf {
    data_dir().join("tasks")
}

fn memory_file() -> PathBuf {
    data_dir().join("memory").join("MEMORY.md")
}

/// Only `[A-Za-z0-9_-]`, capped at 64 chars — the same charset the UI's
/// `Date.now()+random` ids produce. Anything else becomes `_`, so a hostile
/// id like `../../config.json` collapses to a harmless name.
fn safe_id(raw: &str) -> String {
    let mut id: String = raw
        .chars()
        .take(64)
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect();
    if id.is_empty() {
        id = "untitled".into();
    }
    id
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Atomic write: `.tmp` alongside the target, then rename over it.
fn atomic_write(path: &std::path::Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create_dir_all: {e}"))?;
    }
    let tmp = path.with_extension("tmp");
    {
        let mut f = std::fs::File::create(&tmp).map_err(|e| format!("write {}: {e}", tmp.display()))?;
        f.write_all(bytes).map_err(|e| format!("write {}: {e}", tmp.display()))?;
        f.sync_all().ok();
    }
    std::fs::rename(&tmp, path).map_err(|e| format!("rename {}: {e}", path.display()))
}

/* ── sessions ─────────────────────────────────────── */

pub fn session_list() -> Value {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(sessions_dir()) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("json") {
                continue;
            }
            if let Ok(raw) = std::fs::read_to_string(&p) {
                if let Ok(v) = serde_json::from_str::<Value>(&raw) {
                    out.push(json!({
                        "id": v.get("id").cloned().unwrap_or(json!("")),
                        "title": v.get("title").cloned().unwrap_or(json!("")),
                        "ts": v.get("ts").cloned().unwrap_or(json!(0)),
                        "count": v.pointer("/msgs").and_then(Value::as_array).map(|a| a.len()).unwrap_or(0),
                    }));
                }
            }
        }
    }
    out.sort_by_key(|v| -v.get("ts").and_then(Value::as_i64).unwrap_or(0));
    json!({ "sessions": out })
}

pub fn session_load(id: &str) -> Option<Value> {
    let p = sessions_dir().join(format!("{}.json", safe_id(id)));
    let raw = std::fs::read_to_string(p).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Save a session `{id, title, ts, msgs}` sent by the browser. The transcript
/// is the browser's own object mirrored verbatim — attachments' image parts
/// (data URLs) come along, capped so a handful of images can't fill the disk.
pub fn session_save(body: &Value) -> Result<Value, String> {
    let id = body
        .get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("session id is missing")?;
    let id = safe_id(id);
    if !body.get("msgs").and_then(Value::as_array).is_some_and(|a| !a.is_empty()) {
        return Err("session has no messages — nothing to save".into());
    }
    let mut rec = body.clone();
    rec["id"] = json!(id);
    rec["savedAt"] = json!(now_secs());
    let bytes = serde_json::to_vec(&rec).map_err(|e| e.to_string())?;
    if bytes.len() > 6 * 1024 * 1024 {
        return Err("session too large to persist (>6 MB) — trim image attachments".into());
    }
    atomic_write(&sessions_dir().join(format!("{id}.json")), &bytes)?;
    Ok(json!({ "ok": true, "id": id }))
}

pub fn session_delete(id: &str) -> Result<Value, String> {
    let p = sessions_dir().join(format!("{}.json", safe_id(id)));
    match std::fs::remove_file(&p) {
        Ok(()) => Ok(json!({ "ok": true })),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(json!({ "ok": true })),
        Err(e) => Err(format!("delete {}: {e}", p.display())),
    }
}

/* ── long-term memory (nanobot's Dream → MEMORY.md pattern) ── */

pub fn memory_read() -> String {
    std::fs::read_to_string(memory_file()).unwrap_or_default()
}

/// Replace the whole memory file. Used by the UI editor.
pub fn memory_write(text: &str) -> Result<Value, String> {
    atomic_write(&memory_file(), text.as_bytes())?;
    Ok(json!({ "ok": true }))
}

/// Append one bullet. Keeps the file bounded: entries beyond ~200 lines are
/// dropped from the top so the agent prompt can't grow unbounded.
pub fn memory_add(text: &str) -> Result<String, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("memory entry is empty".into());
    }
    let mut cur = memory_read();
    let line = if text.contains('\n') {
        // multi-line entries collapse into one bullet line
        let one = text.lines().map(|l| l.trim()).collect::<Vec<_>>().join(" ");
        format!("- {} _(from agent, {})_\n", one, now_secs())
    } else {
        format!("- {} _(from agent, {})_\n", text, now_secs())
    };
    if !cur.is_empty() && !cur.ends_with('\n') {
        cur.push('\n');
    }
    cur.push_str(&line);
    // keep only the newest ~200 lines
    let lines: Vec<&str> = cur.lines().collect();
    if lines.len() > 200 {
        cur = lines[lines.len() - 200..].join("\n") + "\n";
    }
    atomic_write(&memory_file(), cur.as_bytes())?;
    Ok(cur)
}

/// Memory trimmed for prompt injection — the head is what gets cut, since
/// `memory_add` appends and the newest entries matter most.
pub fn memory_for_prompt(cap: usize) -> Option<String> {
    let m = memory_read();
    let t = m.trim();
    if t.is_empty() {
        return None;
    }
    if t.len() > cap {
        // Cut at a char boundary; MEMORY.md holds user text, and a plain byte
        // slice mid-codepoint would panic.
        let mut start = t.len() - cap;
        while start > 0 && !t.is_char_boundary(start) {
            start += 1;
        }
        Some(t[start..].to_string())
    } else {
        Some(t.to_string())
    }
}

/* ── agent task results ───────────────────────────── */

pub fn task_list() -> Value {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(tasks_dir()) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("json") {
                continue;
            }
            if let Ok(raw) = std::fs::read_to_string(&p) {
                if let Ok(v) = serde_json::from_str::<Value>(&raw) {
                    out.push(json!({
                        "id": v.get("id").cloned().unwrap_or(json!("")),
                        "task": v.get("task").cloned().unwrap_or(json!("")),
                        "workspace": v.get("workspace").cloned().unwrap_or(json!("")),
                        "ts": v.get("ts").cloned().unwrap_or(json!(0)),
                        "summary": v.get("summary").cloned().unwrap_or(json!("")),
                    }));
                }
            }
        }
    }
    out.sort_by_key(|v| -v.get("ts").and_then(Value::as_i64).unwrap_or(0));
    json!({ "tasks": out })
}

pub fn task_load(id: &str) -> Option<Value> {
    let p = tasks_dir().join(format!("{}.json", safe_id(id)));
    let raw = std::fs::read_to_string(p).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Persist a completed agent run: the task, its NDJSON event stream, and the
/// final assistant text as the summary. Keeps the newest 200 tasks.
pub fn task_save(task: &str, workspace: &str, skill: Option<&str>, events: &[Value]) -> Result<Value, String> {
    let ts = now_secs();
    let id = format!("t{ts}x{}", &format!("{:x}", std::process::id()));
    let summary = events
        .iter()
        .rev()
        .find(|e| e.get("type").and_then(Value::as_str) == Some("assistant"))
        .and_then(|e| e.get("text"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .chars()
        .take(2000)
        .collect::<String>();
    let rec = json!({
        "id": id,
        "ts": ts,
        "task": task,
        "workspace": workspace,
        "skill": skill,
        "summary": summary,
        "events": events,
    });
    atomic_write(&tasks_dir().join(format!("{id}.json")), &serde_json::to_vec(&rec).map_err(|e| e.to_string())?)?;
    prune_tasks(200);
    Ok(json!({ "ok": true, "id": id }))
}

fn prune_tasks(max: usize) {
    let mut files: Vec<(u64, PathBuf)> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(tasks_dir()) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("json") {
                continue;
            }
            let ts = p
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.strip_prefix('t'))
                .and_then(|s| s.split('x').next())
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            files.push((ts, p));
        }
    }
    if files.len() <= max {
        return;
    }
    files.sort_by_key(|(ts, _)| *ts);
    let drop = files.len() - max;
    for (_, p) in files.into_iter().take(drop) {
        let _ = std::fs::remove_file(p);
    }
}
