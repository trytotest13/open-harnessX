//! Open Harness's own coding agent — an OpenAI tool-calling loop over the
//! local engine with filesystem and command tools confined to a workspace
//! folder. This powers the native Agent panel; no external IDE is involved.
//!
//! The route streams progress to the browser as NDJSON lines:
//! `{"type":"start"|"model"|"tool_call"|"tool_result"|"assistant"|"done"|"error",...}`

use crate::config::Config;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

const MAX_ROUNDS: usize = 15;
const READ_CAP: u64 = 64 * 1024;
const CMD_TIMEOUT: Duration = Duration::from_secs(120);
const OUT_CAP: usize = 32 * 1024;
/// Tool outputs sent back to the model are capped harder than what the UI
/// shows, so a few rounds can't blow the context window.
const MODEL_OUT_CAP: usize = 2000;

/* ── workspace path safety ────────────────────────── */

/// Resolve `rel` inside `ws`, refusing `..` and anything that escapes the
/// workspace after canonicalization. `created` allows a not-yet-existing
/// final component (write_file); its parent must still resolve inside.
pub(crate) fn safe_join(ws: &Path, rel: &str, created: bool) -> Result<PathBuf, String> {
    let rel = rel.trim();
    let rel = if rel.is_empty() || rel == "." { "." } else { rel };
    if Path::new(rel).components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        return Err(format!("'{rel}' contains '..' — outside the workspace"));
    }
    let p = ws.join(rel);
    if p.exists() {
        let canon = p.canonicalize().map_err(|e| format!("{rel}: {e}"))?;
        let ws_canon = ws.canonicalize().map_err(|e| format!("workspace: {e}"))?;
        if canon.starts_with(&ws_canon) {
            return Ok(canon);
        }
        return Err(format!("'{rel}' resolves outside the workspace"));
    }
    if created {
        let parent = p.parent().ok_or_else(|| format!("{rel}: no parent"))?;
        let canon = parent
            .canonicalize()
            .map_err(|e| format!("{rel}: {e}"))?;
        let ws_canon = ws.canonicalize().map_err(|e| format!("workspace: {e}"))?;
        if canon.starts_with(&ws_canon) {
            return Ok(p);
        }
        return Err(format!("'{rel}' resolves outside the workspace"));
    }
    Err(format!("'{rel}' does not exist"))
}

/* ── tools ────────────────────────────────────────── */

fn tool_list_dir(ws: &Path, rel: &str) -> Result<String, String> {
    let dir = safe_join(ws, rel, false)?;
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    for e in std::fs::read_dir(&dir).map_err(|e| format!("{rel}: {e}"))?.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if e.path().is_dir() {
            dirs.push(format!("{name}/"));
        } else {
            let size = e.metadata().map(|m| m.len()).unwrap_or(0);
            files.push(format!("{name} ({} bytes)", size));
        }
    }
    dirs.sort();
    files.sort();
    Ok(format!("dirs: {:?}\nfiles: {:?}", dirs, files))
}

fn tool_read_file(ws: &Path, rel: &str) -> Result<String, String> {
    let f = safe_join(ws, rel, false)?;
    let mut buf = Vec::new();
    std::fs::File::open(&f)
        .map_err(|e| format!("{rel}: {e}"))?
        .take(READ_CAP)
        .read_to_end(&mut buf)
        .map_err(|e| format!("{rel}: {e}"))?;
    let text = String::from_utf8_lossy(&buf);
    let more = if buf.len() as u64 == READ_CAP { " …[truncated]" } else { "" };
    Ok(format!("{}{more}", text.get(..text.len()).unwrap_or("")))
}

fn tool_write_file(ws: &Path, rel: &str, content: &str) -> Result<String, String> {
    let f = safe_join(ws, rel, true)?;
    std::fs::write(&f, content).map_err(|e| format!("{rel}: {e}"))?;
    Ok(format!("wrote {} ({} bytes)", rel, content.len()))
}

/// Drain a child's pipe on its own thread: keep up to `cap` bytes, then keep
/// reading and discarding. Reading only after the wait deadlocks once output
/// outgrows the OS pipe buffer: the child blocks on write and never exits,
/// so every chatty command would run into the timeout.
fn drain_pipe<R: Read + Send + 'static>(
    pipe: Option<R>,
    cap: usize,
) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut kept = Vec::new();
        let Some(mut r) = pipe else { return kept };
        let _ = r.by_ref().take(cap as u64).read_to_end(&mut kept);
        let _ = std::io::copy(&mut r, &mut std::io::sink());
        kept
    })
}

/// Truncate to at most `cap` bytes without splitting a codepoint. A plain
/// byte slice mid-codepoint makes `str::get` return None, silently emptying
/// the output.
fn clip(s: &str, cap: usize) -> &str {
    if s.len() <= cap {
        return s;
    }
    let mut end = cap;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn shell_exec(ws: &Path, prog: &str, args: Vec<String>) -> Result<String, String> {
    let mut child = Command::new(prog)
        .args(&args)
        .current_dir(ws)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| format!("could not start: {e}"))?;
    let out_t = drain_pipe(child.stdout.take(), OUT_CAP);
    let err_t = drain_pipe(child.stderr.take(), OUT_CAP);
    let deadline = Instant::now() + CMD_TIMEOUT;
    let code = loop {
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(status) => break status.code().unwrap_or(-1),
            None if Instant::now() > deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("timed out after {}s", CMD_TIMEOUT.as_secs()));
            }
            None => std::thread::sleep(Duration::from_millis(150)),
        }
    };
    let mut out = out_t.join().unwrap_or_default();
    out.extend_from_slice(&err_t.join().unwrap_or_default());
    let text = String::from_utf8_lossy(&out);
    let clipped = clip(&text, OUT_CAP);
    Ok(format!("exit code {code}\n{clipped}"))
}

fn tool_run_command(ws: &Path, command: &str) -> Result<String, String> {
    shell_exec(ws, "cmd", vec!["/C".into(), command.into()])
}

/// Manual console commands from the Agent panel: cmd or PowerShell, run with
/// the workspace as the current folder. Output is captured, not interactive.
pub(crate) fn console_run(ws: &Path, command: &str, mode: &str) -> Result<String, String> {
    match mode {
        "ps" => shell_exec(ws, "powershell", vec!["-NoProfile".into(), "-Command".into(), command.into()]),
        _ => shell_exec(ws, "cmd", vec!["/C".into(), command.into()]),
    }
}

/* ── skills + web tools (available everywhere, no workspace needed) ── */

fn tool_list_skills() -> Result<String, String> {
    let skills = crate::server::skills_scan();
    if skills.is_empty() {
        Ok("no skills installed — add .md files to the app's skills folder, \
            or <name>/SKILL.md folders under ~/.agents/skills".into())
    } else {
        Ok(skills.iter().map(|(n, d, _)| format!("- {n}: {d}")).collect::<Vec<_>>().join("\n"))
    }
}

fn tool_load_skill(name: &str) -> Result<String, String> {
    crate::server::skill_body(name)
        .map(|b| b.get(..(16 * 1024).min(b.len())).unwrap_or(&b).to_string())
        .map_err(|_| format!("skill '{name}' not found — use list_skills to see what exists"))
}

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn tool_fetch_url(url: &str) -> Result<String, String> {
    let url = url.trim();
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err("only http(s) URLs are allowed".into());
    }
    let safe = url.replace('\'', "''");
    let script = format!(
        "$r = Invoke-WebRequest -UseBasicParsing -Uri '{}' -TimeoutSec 30; [Console]::OutputEncoding=[Text.Encoding]::UTF8; $r.Content",
        safe
    );
    let mut child = Command::new("powershell")
        .args(["-NoProfile", "-Command", &script])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| format!("could not start: {e}"))?;
    let out_t = drain_pipe(child.stdout.take(), OUT_CAP);
    let err_t = drain_pipe(child.stderr.take(), 16 * 1024);
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(_) => break,
            None if Instant::now() > deadline => {
                let _ = child.kill();
                return Err("timed out after 45s".into());
            }
            None => std::thread::sleep(Duration::from_millis(150)),
        }
    }
    let mut out = out_t.join().unwrap_or_default();
    out.extend_from_slice(&err_t.join().unwrap_or_default());
    let text = String::from_utf8_lossy(&out);
    Ok(clip(&text, OUT_CAP).to_string())
}

fn tool_def(name: &str, description: &str, props: &str, required: &str) -> serde_json::Value {
    serde_json::json!({
        "type":"function",
        "function":{
            "name": name,
            "description": description,
            "parameters":{"type":"object","properties": serde_json::from_str::<serde_json::Value>(props).unwrap_or(serde_json::json!({})),"required": serde_json::from_str::<serde_json::Value>(required).unwrap_or(serde_json::json!([]))}
        }
    })
}

/// The tool set for a run: skills + web are always available; the file and
/// command tools appear only when a workspace is set (they are confined to
/// it, so without one they can't run safely).
fn tools_for(ws: Option<&Path>) -> Vec<serde_json::Value> {
    let mut t = vec![
        tool_def("list_skills", "List the installed Open Harness skills (markdown instructions you can load)", "{}", "[]"),
        tool_def("load_skill", "Load a skill's full instructions by name", r#"{"name":{"type":"string","description":"skill name from list_skills"}}"#, r#"["name"]"#),
        tool_def("fetch_url", "Fetch a web page's text over http(s) for research", r#"{"url":{"type":"string","description":"http(s) URL"}}"#, r#"["url"]"#),
        tool_def("remember", "Save one durable fact or preference about the user or this machine to long-term memory, so future sessions know it. Use for lasting facts (stack choices, preferences, project names), not one-off task details.", r#"{"text":{"type":"string","description":"the fact to remember, one line"}}"#, r#"["text"]"#),
    ];
    if ws.is_some() {
        t.push(tool_def("list_dir", "List a folder inside the workspace", r#"{"path":{"type":"string","description":"relative path, use '.' for the workspace root"}}"#, r#"["path"]"#));
        t.push(tool_def("read_file", "Read a text file inside the workspace", r#"{"path":{"type":"string","description":"relative file path"}}"#, r#"["path"]"#));
        t.push(tool_def("write_file", "Create or overwrite a text file inside the workspace", r#"{"path":{"type":"string","description":"relative file path"},"content":{"type":"string","description":"full file content"}}"#, r#"["path","content"]"#));
        t.push(tool_def("run_command", "Run a Windows shell command (cmd /C) with the workspace as the current folder. Use for builds, git, dir listings, running scripts.", r#"{"command":{"type":"string","description":"the command line to run"}}"#, r#"["command"]"#));
    }
    t
}

fn execute_tool(ws: Option<&Path>, name: &str, args: &serde_json::Value) -> Result<String, String> {
    let s = |k: &str| args.get(k).and_then(|v| v.as_str()).unwrap_or("");
    match name {
        "list_skills" => tool_list_skills(),
        "load_skill" => tool_load_skill(s("name")),
        "fetch_url" => tool_fetch_url(s("url")),
        "remember" => tool_remember(s("text")),
        "list_dir" => tool_list_dir(ws.ok_or("no workspace set — this tool needs one")?, s("path")),
        "read_file" => tool_read_file(ws.ok_or("no workspace set — this tool needs one")?, s("path")),
        "write_file" => tool_write_file(ws.ok_or("no workspace set — this tool needs one")?, s("path"), s("content")),
        "run_command" => tool_run_command(ws.ok_or("no workspace set — this tool needs one")?, s("command")),
        other => Err(format!("unknown tool {other}")),
    }
}

/* ── long-term memory (nanobot Dream pattern) ─────── */

/// The agent-visible summary of what it remembers about the user/machine,
/// injected into every system prompt. Newest entries win; the head is cut.
fn memory_prompt() -> String {
    match crate::store::memory_for_prompt(3000) {
        Some(m) => format!(
            "\n\nLong-term memory about the user and this machine (persisted across \
             sessions). Facts and preferences here outrank guesses — when the user \
             states something durable worth keeping, save it with the remember tool:\n{m}"
        ),
        None => String::new(),
    }
}

fn tool_remember(text: &str) -> Result<String, String> {
    crate::store::memory_add(text)?;
    Ok("saved to long-term memory".into())
}

/* ── engine call ──────────────────────────────────── */

fn engine_chat(cfg: &Config, tools: &[serde_json::Value], messages: &serde_json::Value) -> Result<serde_json::Value, String> {
    let body = serde_json::json!({
        "messages": messages,
        "tools": tools,
        "tool_choice": "auto",
        "temperature": 0.2,
        "max_tokens": 2048,
        // small local models degenerate into "Assistant: Done." loops; these
        // penalties are the standard mitigation and cost nothing when unneeded
        "repeat_penalty": 1.1,
        "presence_penalty": 0.4,
        "frequency_penalty": 0.3,
    });
    let body = serde_json::to_vec(&body).map_err(|e| e.to_string())?;
    let mut up = TcpStream::connect((cfg.engine.host.as_str(), cfg.engine.port))
        .map_err(|_| "engine is not reachable — is the model loaded?".to_string())?;
    up.set_read_timeout(Some(Duration::from_secs(300))).ok();
    up.set_write_timeout(Some(Duration::from_secs(30))).ok();
    let req = format!(
        "POST /v1/chat/completions HTTP/1.1\r\nHost: {}:{}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n",
        cfg.engine.host,
        cfg.engine.port,
        body.len()
    );
    up.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
    up.write_all(&body).map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(up);
    let mut line = String::new();
    reader.read_line(&mut line).map_err(|e| e.to_string())?;
    let status = line.split_whitespace().nth(1).unwrap_or("0").to_string();
    loop {
        line.clear();
        if reader.read_line(&mut line).map_err(|e| e.to_string())? == 0 || line.trim().is_empty() {
            break;
        }
    }
    let mut payload = String::new();
    reader.read_to_string(&mut payload).map_err(|e| e.to_string())?;
    if !status.starts_with('2') {
        return Err(format!("engine returned HTTP {status}: {}", payload.get(..300).unwrap_or(&payload)));
    }
    serde_json::from_str(&payload).map_err(|e| format!("bad engine response: {e}"))
}

/* ── the loop ─────────────────────────────────────── */

fn emit(w: &mut TcpStream, value: serde_json::Value) -> std::io::Result<()> {
    let mut line = serde_json::to_string(&value).unwrap_or_else(|_| "{}".into());
    line.push('\n');
    w.write_all(line.as_bytes())?;
    w.flush()
}

/// Run one agent task and stream its steps as NDJSON into `client`.
pub fn run(client: &mut TcpStream, body: &[u8]) -> std::io::Result<()> {
    let head = "HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\n\
                Cache-Control: no-store\r\nConnection: close\r\n\r\n";
    client.write_all(head.as_bytes())?;

    let req: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => return emit(client, serde_json::json!({"type":"error","message":format!("bad body: {e}")})),
    };
    let task = req.get("task").and_then(|t| t.as_str()).unwrap_or("").trim().to_string();
    let ws_raw = req.get("workspace").and_then(|t| t.as_str()).unwrap_or("").trim().to_string();
    if task.is_empty() {
        return emit(client, serde_json::json!({"type":"error","message":"task is empty"}));
    }
    let ws = PathBuf::from(&ws_raw);
    if !ws.is_dir() {
        return emit(client, serde_json::json!({"type":"error","message":format!("not a folder: {ws_raw}")}));
    }
    emit(client, serde_json::json!({"type":"start","workspace":ws_raw}))?;

    let cfg = match crate::load_config() {
        Ok(c) => c,
        Err(e) => return emit(client, serde_json::json!({"type":"error","message":e})),
    };

    let sys = format!(
        "You are Open Harness's coding agent, working inside the workspace folder \"{ws_raw}\" \
         on this Windows PC. You have tools: list_dir, read_file, write_file, run_command, \
         plus list_skills/load_skill for installed skills and fetch_url for web pages. \
         Paths are relative to the workspace. Prefer list_dir/read_file before changing \
         anything, keep edits minimal, and explain briefly what you did. \
         When the task is complete, reply with a short summary and no tool call."
    );
    // A skill picked with "/" before the task joins the system prompt — the
    // chat template raises "System message must be at the beginning." on any
    // system message after the first, so a second one would fail the run.
    let mut sys = match req.get("skill").and_then(|v| v.as_str()).filter(|s| !s.trim().is_empty()) {
        Some(sk) => format!(
            "{sys}\n\nThe user loaded this skill for the task, follow its instructions:\n\n{sk}"
        ),
        None => sys,
    };
    sys.push_str(&memory_prompt());
    if let Some(md) = crate::system_prompt_md() {
        sys.push_str("\n\n");
        sys.push_str(&md);
    }
    let messages = serde_json::json!([
        {"role": "system", "content": sys},
        {"role": "user", "content": task},
    ]);

    let mut events = Vec::new();
    let res = stream_loop(client, &cfg, Some(&ws), messages, MAX_ROUNDS, &mut events);
    // The run transcript lands in the task archive whether it finished or
    // errored mid-way, so results are available for later work either way.
    let skill_name = req
        .get("skillName")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from);
    if let Err(e) = crate::store::task_save(&task, &ws_raw, skill_name.as_deref(), &events) {
        eprintln!("[oh] agent: task_save failed: {e}");
    }
    res
}

/// Normal chat with tools enabled: the browser sends the conversation history
/// (already ending with the user's new message) and an optional workspace.
/// Skills and web fetch always work; file/command tools need a workspace.
pub fn chat_with_tools(client: &mut TcpStream, body: &[u8]) -> std::io::Result<()> {
    let head = "HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\n\
                Cache-Control: no-store\r\nConnection: close\r\n\r\n";
    client.write_all(head.as_bytes())?;

    let req: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => return emit(client, serde_json::json!({"type":"error","message":format!("bad body: {e}")})),
    };
    let ws_raw = req.get("workspace").and_then(|t| t.as_str()).unwrap_or("").trim().to_string();
    let ws = if ws_raw.is_empty() { None } else { Some(PathBuf::from(&ws_raw)) };
    if let Some(w) = &ws {
        if !w.is_dir() {
            return emit(client, serde_json::json!({"type":"error","message":format!("not a folder: {ws_raw}")}));
        }
    }
    let cfg = match crate::load_config() {
        Ok(c) => c,
        Err(e) => return emit(client, serde_json::json!({"type":"error","message":e})),
    };
    // Image attachments with a vision-less engine would be rejected outright
    // ("image input is not supported"), so transcribe them through the OCR
    // fallback sidecar first — the same rewrite the plain chat path does.
    let req = {
        let images = crate::ocr::collect_images(&req);
        if images.is_empty() || crate::server::engine_vision(&cfg) {
            req
        } else {
            match crate::ocr::transcribe_cached(&cfg, &images) {
                Ok(texts) => {
                    let opts: Vec<Option<String>> = texts.into_iter().map(Some).collect();
                    let mut rewritten = req;
                    crate::ocr::rewrite_with_ocr(&mut rewritten, &opts);
                    rewritten
                }
                Err(e) => return emit(
                    client,
                    serde_json::json!({"type":"error","message":format!("OCR fallback failed: {e}")}),
                ),
            }
        }
    };
    // Keep the recent conversation only — the context window is small.
    let hist = req
        .get("messages")
        .and_then(|m| m.as_array())
        .map(|a| a.iter().rev().take(20).rev().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    if hist.is_empty() {
        return emit(client, serde_json::json!({"type":"error","message":"messages is empty"}));
    }
    let ws_note = match &ws {
        Some(w) => format!(
            " The user also opened the workspace folder \"{}\" — the file tools (list_dir, \
             read_file, write_file, run_command) work inside it when a task needs them.",
            w.display()
        ),
        None => String::new(),
    };
    let mut sys = format!(
        "You are Open Harness, a helpful local AI running on the user's Windows PC.{} \
         You also have list_skills / load_skill for installed skills (load one when the user's \
         request matches it), fetch_url for reading web pages, and remember for saving a \
         durable fact the user states so future sessions know it. Use tools only when they \
         clearly help; otherwise just answer.",
        ws_note
    );
    sys.push_str(&memory_prompt());
    if let Some(md) = crate::system_prompt_md() {
        sys.push_str("\n\n");
        sys.push_str(&md);
    }
    // The chat template only accepts a system message at position 0, and the
    // browser may already send one (its system prompt plus any "/" skill).
    // Fold all system content into a single leading message instead of
    // prepending another one next to it.
    let mut extra_sys: Vec<String> = Vec::new();
    let mut rest: Vec<serde_json::Value> = Vec::new();
    for m in hist {
        if m.get("role").and_then(|r| r.as_str()) == Some("system") {
            extra_sys.push(m.get("content").and_then(|c| c.as_str()).unwrap_or("").to_string());
        } else {
            rest.push(m);
        }
    }
    for extra in extra_sys {
        if !extra.trim().is_empty() {
            sys.push_str("\n\n");
            sys.push_str(&extra);
        }
    }
    let mut messages = serde_json::json!([{ "role": "system", "content": sys }]);
    messages.as_array_mut().unwrap().extend(rest);
    // Chats aren't archived as tasks — the browser mirrors them to the
    // session store; only agent runs land in the task archive.
    let mut unused = Vec::new();
    stream_loop(client, &cfg, ws.as_deref(), messages, 10, &mut unused)
}

/// Shared tool-calling loop: streams NDJSON events into `client` until the
/// model answers without tool calls or `max_rounds` is reached. Every emitted
/// event is also pushed into `events` so the caller can archive the run.
fn stream_loop(
    client: &mut TcpStream,
    cfg: &Config,
    ws: Option<&Path>,
    mut messages: serde_json::Value,
    max_rounds: usize,
    events: &mut Vec<serde_json::Value>,
) -> std::io::Result<()> {
    let tools = tools_for(ws);
    for _round in 0..max_rounds {
        record(events, client, serde_json::json!({"type":"model"}))?;
        let resp = match engine_chat(cfg, &tools, &messages) {
            Ok(r) => r,
            Err(e) => return record(events, client, serde_json::json!({"type":"error","message":e})),
        };
        let Some(msg) = resp.pointer("/choices/0/message") else {
            return record(events, client, serde_json::json!({"type":"error","message":"engine returned no message"}));
        };
        let content = msg.get("content").and_then(|c| c.as_str()).unwrap_or("").to_string();
        let calls = msg.get("tool_calls").and_then(|c| c.as_array());

        match calls {
            // No (or empty) tool calls → the model is done; show its reply.
            None => {
                if !content.is_empty() {
                    record(events, client, serde_json::json!({"type":"assistant","text":content}))?;
                }
                break;
            }
            Some(calls) if calls.is_empty() => {
                if !content.is_empty() {
                    record(events, client, serde_json::json!({"type":"assistant","text":content}))?;
                }
                break;
            }
            Some(calls) => {
                // Echo the assistant turn (with its tool_calls) back verbatim.
                messages.as_array_mut().unwrap().push(msg.clone());
                if !content.is_empty() {
                    record(events, client, serde_json::json!({"type":"assistant","text":content}))?;
                }
                for call in calls {
                    let id = call.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let name = call.pointer("/function/name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let raw_args = call.pointer("/function/arguments").cloned().unwrap_or(serde_json::json!({}));
                    let args: serde_json::Value = match &raw_args {
                        serde_json::Value::String(s) => serde_json::from_str(s).unwrap_or(serde_json::json!({})),
                        v @ serde_json::Value::Object(_) => v.clone(),
                        _ => serde_json::json!({}),
                    };
                    record(events, client, serde_json::json!({"type":"tool_call","name":name,"args":args}))?;
                    let result = execute_tool(ws, &name, &args);
                    let (ok, out) = match &result {
                        Ok(o) => (true, o.clone()),
                        Err(e) => (false, e.clone()),
                    };
                    record(events, client, serde_json::json!({"type":"tool_result","name":name,"ok":ok,"output":out}))?;
                    let mut tool_msg = serde_json::json!({"role":"tool","tool_call_id":id,"content": out});
                    if ok && out.len() > MODEL_OUT_CAP {
                        tool_msg["content"] = serde_json::json!(format!(
                            "{}…[truncated]",
                            clip(&out, MODEL_OUT_CAP)
                        ));
                    }
                    if !ok {
                        // A failed tool is still a valid tool message for the
                        // engine; prefix the error so the model can react.
                        tool_msg["content"] = serde_json::json!(format!("ERROR: {out}"));
                    }
                    messages.as_array_mut().unwrap().push(tool_msg);
                }
                // Trim older tool outputs so long tasks stay inside ctx.
                let len = messages.as_array().map(|a| a.len()).unwrap_or(0);
                for i in 0..len.saturating_sub(8) {
                    let m = &messages[i];
                    if m.get("role").and_then(|r| r.as_str()) == Some("tool") {
                        if let Some(c) = m.get("content").and_then(|c| c.as_str()) {
                            if c.len() > 400 {
                                messages[i]["content"] = serde_json::json!(format!(
                                    "{}…[omitted]",
                                    clip(c, 400)
                                ));
                            }
                        }
                    }
                }
            }
        }
    }
    record(events, client, serde_json::json!({"type":"done"}))
}

/// Emit an NDJSON event and keep a copy for the task archive.
fn record(events: &mut Vec<serde_json::Value>, client: &mut TcpStream, value: serde_json::Value) -> std::io::Result<()> {
    events.push(value.clone());
    emit(client, value)
}
