//! Open Harness web UI server — std-only HTTP on 127.0.0.1:3080 (first free
//! port upward). Serves the embedded UI and a small JSON API that wraps the
//! supervisor's existing model management, plus an SSE proxy to the engine so
//! the browser never needs CORS.

use crate::agent;
use crate::config::Config;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

const INDEX_HTML: &str = include_str!("../ui/index.html");
const MAX_BODY: usize = 8 * 1024 * 1024;
const READ_TIMEOUT: Duration = Duration::from_secs(600);

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
#[cfg(windows)]
use std::os::windows::process::CommandExt;

/// In-process hot-swap status the UI polls while the engine restarts.
#[derive(Default)]
struct LoadStatus {
    active: bool,
    target: Option<String>,
    error: Option<String>,
}
static LOAD: Mutex<Option<LoadStatus>> = Mutex::new(None);

/// Recursive model discovery walks big HF-cache dirs (~3 s here), so results
/// are cached for the session; /api/scan drops the cache.
static MODEL_CACHE: Mutex<Option<(std::time::Instant, Vec<PathBuf>)>> = Mutex::new(None);

fn discover_cached(cfg: &Config) -> Vec<PathBuf> {
    let mut g = MODEL_CACHE.lock().unwrap();
    if let Some((_, m)) = g.as_ref() {
        return m.clone();
    }
    let models = cfg.discover_models();
    *g = Some((std::time::Instant::now(), models.clone()));
    models
}

/// Scan once at startup so the first Models dialog opens instantly.
fn warm_model_cache() {
    if MODEL_CACHE.lock().unwrap().is_none() {
        if let Ok(cfg) = crate::load_config() {
            std::thread::spawn(move || {
                let _ = discover_cached(&cfg);
            });
        }
    }
}

/// Bind and serve on a background thread. Returns the chosen port.
pub fn spawn() -> Result<u16, String> {
    // 3080 only — never drift upward: 3081 belongs to the dsh agent IDE, and
    // this UI silently stealing it makes dsh's webserver fail to bind, which
    // disables most of the IDE's plugins (file tree, terminal, uploads...).
    let listener = TcpListener::bind(("127.0.0.1", 3080)).map_err(|_| {
        "Open Harness UI port 3080 is busy — another instance may still be \
         running. Close it or run open-harness stop, then start again."
            .to_string()
    })?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    println!("[oh] UI serving http://127.0.0.1:{port}");
    warm_model_cache();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            std::thread::spawn(move || handle(stream));
        }
    });
    Ok(port)
}

fn handle(stream: TcpStream) {
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
    let Ok(mut client) = stream.try_clone() else { return };
    let mut reader = BufReader::new(stream);

    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return;
    }
    let mut parts = line.split_whitespace();
    let (Some(method), Some(raw_path)) = (parts.next(), parts.next()) else { return };
    let (path, query) = match raw_path.split_once('?') {
        Some((p, q)) => (p, q),
        None => (raw_path, ""),
    };

    let mut content_length = 0usize;
    let mut origin = String::new();
    let mut host = String::new();
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).is_err() || h.trim().is_empty() {
            break;
        }
        let lower = h.to_ascii_lowercase();
        if let Some(v) = lower.strip_prefix("content-length:").and_then(|s| s.trim().parse().ok()) {
            content_length = v;
        } else if let Some(v) = lower.strip_prefix("origin:") {
            origin = v.trim().to_string();
        } else if let Some(v) = lower.strip_prefix("host:") {
            host = v.trim().to_string();
        }
    }

    // The API can run real shell commands (/api/console, /api/agent) and hands
    // out chat transcripts and memory, so a page open elsewhere in the browser
    // must not be able to reach it. Refuse requests whose Origin is another
    // site (cross-site fetch/form posts always carry their real Origin) and
    // whose Host is not this server (DNS rebinding). Local origins — this UI
    // window, the dsh IDE on 3081 — and non-browser clients (no Origin) pass.
    let is_local = |v: &str| {
        let no_scheme = v.trim_start_matches("http://").trim_start_matches("https://");
        let name = no_scheme.rsplit_once(':').map(|(h, _)| h).unwrap_or(no_scheme);
        matches!(name.to_lowercase().as_str(), "" | "127.0.0.1" | "localhost" | "[::1]")
    };
    if !is_local(&origin) || !is_local(&host) {
        let _ = text(
            &mut client,
            "403 Forbidden",
            "application/json",
            br#"{"error":"cross-origin requests are not allowed"}"#,
        );
        return;
    }

    if content_length > MAX_BODY {
        // Reject instead of truncating: a clipped body would surface as a
        // confusing "bad JSON" parse error downstream.
        let _ = text(
            &mut client,
            "413 Payload Too Large",
            "application/json",
            br#"{"error":"request body exceeds 8 MB"}"#,
        );
        return;
    }

    let mut body = Vec::new();
    if content_length > 0 {
        (&mut reader)
            .take(content_length.min(MAX_BODY) as u64)
            .read_to_end(&mut body)
            .ok();
    }

    let route = format!("{method} {path}");
    let t0 = std::time::Instant::now();
    let result = match route.as_str() {
        "GET /" => {
            // Prefer launcher/ui/index.html next to the exe so UI tweaks
            // apply with a plain page reload — no rebuild. The embedded copy
            // is the fallback when the file is missing.
            let html = std::fs::read(
                crate::root_dir().join("launcher").join("ui").join("index.html"),
            )
            .unwrap_or_else(|_| INDEX_HTML.as_bytes().to_vec());
            text(&mut client, "200 OK", "text/html; charset=utf-8", &html)
        }
        "GET /favicon.ico" => text(&mut client, "204 No Content", "text/plain", b""),
        "OPTIONS /api/chat" => preflight(&mut client),
        "GET /api/status" => j(&mut client, 200, || status_json()),
        "GET /api/metrics" => j(&mut client, 200, metrics_json),
        "GET /api/models" => j(&mut client, 200, models_json),
        "GET /api/settings" => j(&mut client, 200, settings_json),
        "POST /api/settings" => j(&mut client, 200, || settings_write(&body)),
        "GET /api/providers" => j(&mut client, 200, providers_json),
        "POST /api/providers" => j(&mut client, 200, || providers_write(&body)),
        "POST /api/scan" => j(&mut client, 200, || scan(&body)),
        "POST /api/load" => j(&mut client, 202, || load(&body)),
        "POST /api/dsh" => j(&mut client, 200, dsh_open),
        "GET /api/tree" => j(&mut client, 200, || tree_json(query)),
        "POST /api/browse" => j(&mut client, 200, browse_folder),
        "POST /api/console" => j(&mut client, 200, || console_json(&body)),
        "POST /api/agent" => agent::run(&mut client, &body),
        "POST /api/chat-agent" => agent::chat_with_tools(&mut client, &body),
        "POST /api/chat" => proxy_chat(&mut client, &body),
        "POST /api/attach" => j(&mut client, 200, || attach_json(&body, query)),
        "GET /api/skills" => j(&mut client, 200, skills_list_json),
        r if r.starts_with("GET /api/skills/") => {
            let name = r["GET /api/skills/".len()..].to_string();
            j(&mut client, 200, || skill_by_name_json(&name))
        }
        // durable store: sessions mirror, long-term memory, task results
        "GET /api/sessions" => j(&mut client, 200, crate::store::session_list),
        "POST /api/session" => j(&mut client, 200, || body_handler(&body, crate::store::session_save)),
        "GET /api/memory" => j(&mut client, 200, || {
            serde_json::json!({ "memory": crate::store::memory_read() })
        }),
        "POST /api/memory" => j(&mut client, 200, || memory_write_json(&body)),
        "GET /api/tasks" => j(&mut client, 200, crate::store::task_list),
        r if r.starts_with("GET /api/session/") => {
            let id = r["GET /api/session/".len()..].to_string();
            j(&mut client, 200, || match crate::store::session_load(&id) {
                Some(v) => v,
                None => serde_json::json!({"error": format!("no session: {id}")}),
            })
        }
        r if r.starts_with("DELETE /api/session/") => {
            let id = r["DELETE /api/session/".len()..].to_string();
            j(&mut client, 200, || match crate::store::session_delete(&id) {
                Ok(v) => v,
                Err(e) => serde_json::json!({"error": e}),
            })
        }
        r if r.starts_with("GET /api/task/") => {
            let id = r["GET /api/task/".len()..].to_string();
            j(&mut client, 200, || match crate::store::task_load(&id) {
                Some(v) => v,
                None => serde_json::json!({"error": format!("no task: {id}")}),
            })
        }
        _ => j(&mut client, 404, || {
            serde_json::json!({"error": format!("no route: {route}")})
        }),
    };
    if let Err(e) = result {
        eprintln!("[oh] {route}: {e}");
    } else if t0.elapsed() > Duration::from_millis(500) {
        eprintln!("[oh] slow: {route} took {}ms", t0.elapsed().as_millis());
    }
}

/// Native folder picker for the Agent panel's workspace box. Blocks this
/// request's thread until the user picks a folder or cancels; the server
/// handles each connection on its own thread, so the rest of the UI (chat,
/// status polling) stays live meanwhile.
fn browse_folder() -> serde_json::Value {
    let picked = std::thread::spawn(|| {
        rfd::FileDialog::new()
            .set_title("Pick the workspace folder for the agent")
            .pick_folder()
    })
    .join()
    .ok()
    .flatten();
    match picked {
        Some(p) => serde_json::json!({"path": p.display().to_string()}),
        None => serde_json::json!({"path": null}),
    }
}

/// Run one manual console command (cmd or powershell) with the workspace as
/// the current folder and return its captured output.
fn console_json(body: &[u8]) -> serde_json::Value {
    let v = match body_json(body) {
        Ok(v) => v,
        Err(e) => return serde_json::json!({"error": e}),
    };
    let ws_raw = v["workspace"].as_str().unwrap_or("").trim().to_string();
    let command = v["command"].as_str().unwrap_or("").trim().to_string();
    let mode = v["mode"].as_str().unwrap_or("cmd");
    if command.is_empty() {
        return serde_json::json!({"error": "command is empty"});
    }
    let ws = PathBuf::from(&ws_raw);
    if !ws.is_dir() {
        return serde_json::json!({"error": format!("not a folder: {ws_raw}")});
    }
    match agent::console_run(&ws, &command, mode) {
        Ok(output) => serde_json::json!({"ok": true, "output": output}),
        Err(e) => serde_json::json!({"ok": false, "output": e}),
    }
}

/* ── attachments: text extraction for chat file uploads ── */

/// Per-file cap on extracted text, so one huge PDF can't fill the context
/// window by itself (the UI adds a further per-message budget on top).
const ATTACH_TEXT_CAP: usize = 48 * 1024;

/// Raw bytes arrive as the request body; the filename rides in the query
/// string (`?name=report.pdf`) because the hand-rolled HTTP layer has no
/// multipart parsing.
fn attach_json(body: &[u8], query: &str) -> serde_json::Value {
    let name = query
        .split('&')
        .find_map(|kv| kv.split_once('=').filter(|(k, _)| *k == "name"))
        .map(|(_, v)| v.replace("%20", " "))
        .unwrap_or_else(|| "file".into());
    let name = name.rsplit(['\\', '/']).next().unwrap_or("file").to_string();
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    const IMAGE_EXTS: [&str; 6] = ["png", "jpg", "jpeg", "gif", "webp", "bmp"];
    if IMAGE_EXTS.contains(&ext.as_str()) {
        // Images skip the server: the browser base64-encodes them straight
        // into the message as an image_url part for a vision-capable engine.
        return serde_json::json!({"name": name, "kind": "image"});
    }
    if body.is_empty() {
        return serde_json::json!({"name": name, "kind": "file", "error": "empty upload"});
    }
    match extract_text(&ext, body) {
        Ok(text) => {
            let text = cap_chars(text.trim(), ATTACH_TEXT_CAP);
            serde_json::json!({"name": name, "kind": "file", "chars": text.chars().count(), "text": text})
        }
        Err(e) => serde_json::json!({"name": name, "kind": "file", "error": e}),
    }
}

fn cap_chars(s: &str, cap: usize) -> String {
    if s.chars().count() <= cap {
        return s.to_string();
    }
    format!("{}\n\n…[truncated]", s.chars().take(cap).collect::<String>())
}

fn extract_text(ext: &str, bytes: &[u8]) -> Result<String, String> {
    match ext {
        "pdf" => {
            // pdf-extract reads from a path; stage the upload in %TEMP%.
            let mut tmp = std::env::temp_dir();
            tmp.push(format!(
                "oh-attach-{}.pdf",
                std::process::id() as u128 * 1_000_000 + std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.subsec_millis() as u128)
                    .unwrap_or(0)
            ));
            std::fs::write(&tmp, bytes).map_err(|e| format!("temp write: {e}"))?;
            let r = pdf_extract::extract_text(&tmp).map_err(|e| format!("pdf: {e}"));
            let _ = std::fs::remove_file(&tmp);
            r
        }
        "docx" => {
            let cur = std::io::Cursor::new(bytes);
            let mut ar =
                zip::ZipArchive::new(cur).map_err(|_| "not a valid .docx (zip) file".to_string())?;
            let mut xml = String::new();
            ar.by_name("word/document.xml")
                .map_err(|_| "not a valid .docx (no word/document.xml)".to_string())?
                .read_to_string(&mut xml)
                .map_err(|e| e.to_string())?;
            Ok(docx_text(&xml))
        }
        // Everything else — .txt, .md, .csv, code, configs — is read as text.
        // Binary formats that aren't handled come out lossy; the model just
        // sees noise, which is honest enough.
        _ => Ok(String::from_utf8_lossy(bytes).into_owned()),
    }
}

/// Concatenate the visible text runs of word/document.xml, one line per
/// paragraph (`</w:p>`). A `<w:t` only counts when the next char is '>',
/// ' ' or '/', so `<w:tab` and `<w:tc` don't match.
fn docx_text(xml: &str) -> String {
    let mut out = String::new();
    let b = xml.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i..].starts_with(b"<w:t") && matches!(b.get(i + 4), Some(b'>' | b' ' | b'/')) {
            let Some(gt) = xml[i..].find('>') else { break };
            let start = i + gt + 1;
            if xml[..start].ends_with("/>") {
                i = start;
                continue;
            }
            let Some(end) = xml[start..].find("</w:t>") else { break };
            out.push_str(&xml_unescape(&xml[start..start + end]));
            i = start + end + 6;
        } else if b[i..].starts_with(b"</w:p>") {
            out.push('\n');
            i += 6;
        } else {
            match xml[i + 1..].find('<') {
                Some(d) => i += d + 1,
                None => break,
            }
        }
    }
    out
}

/// Decode the five entities Word actually emits; &amp; last on purpose.
fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/* ── skills: app skills folder (.md files) + global ~/.agents/skills (folders with SKILL.md) ── */

/// Same roots the dsh harness scans: the app's own skills folder first, then
/// the user's global agents folder (where the antislop family lives).
fn skill_roots() -> Vec<(PathBuf, &'static str)> {
    let mut v = vec![(crate::root_dir().join("skills"), "app")];
    if let Some(home) = std::env::var_os("USERPROFILE") {
        v.push((PathBuf::from(home).join(".agents").join("skills"), "global"));
    }
    v
}

/// One-line description: frontmatter `description:`, else the first plain
/// prose line (skipping YAML, headings and blockquotes).
fn skill_desc(text: &str) -> String {
    let (fm, body) = match text.strip_prefix("---") {
        Some(rest) => match rest.splitn(2, "\n---").nth(1) {
            Some(b) => (Some(rest.split("\n---").next().unwrap_or("")), b),
            None => (Some(rest), rest),
        },
        None => (None, text),
    };
    if let Some(block) = fm {
        if let Some(v) = block.lines().find_map(|l| l.trim().strip_prefix("description:")) {
            let v = v.trim().trim_matches(|c| c == '"' || c == '\'');
            // multi-line block scalars (">") fall through to the prose line
            if !v.is_empty() && !v.starts_with('>') {
                return v.to_string();
            }
        }
    }
    body.lines()
        .map(str::trim)
        .find(|l| {
            !l.is_empty()
                && !l.starts_with('#')
                && !l.starts_with('>')
                && !l.starts_with("```")
        })
        .unwrap_or("")
        .to_string()
}

/// (name, description, source) for every skill found, app roots first and
/// deduped by name so a local skill shadows a global one.
pub(crate) fn skills_scan() -> Vec<(String, String, &'static str)> {
    let mut out: Vec<(String, String, &'static str)> = Vec::new();
    for (root, source) in skill_roots() {
        let Ok(rd) = std::fs::read_dir(&root) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            let (name, path) = if p.is_dir() {
                (e.file_name().to_string_lossy().to_string(), p.join("SKILL.md"))
            } else if p.extension().is_some_and(|x| x == "md") {
                (
                    p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default(),
                    p,
                )
            } else {
                continue;
            };
            if name.is_empty() || !path.is_file() || out.iter().any(|(n, _, _)| *n == name) {
                continue;
            }
            let desc = std::fs::read_to_string(&path)
                .map(|t| skill_desc(&t))
                .unwrap_or_default();
            out.push((name, desc, source));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Full instruction body of one skill with the frontmatter stripped, 64 KB cap.
pub(crate) fn skill_body(name: &str) -> Result<String, String> {
    let stem = Path::new(name)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .filter(|s| !s.is_empty() && s != "..")
        .ok_or_else(|| "bad skill name".to_string())?;
    for (root, _) in skill_roots() {
        for cand in [root.join(format!("{stem}.md")), root.join(&stem).join("SKILL.md")] {
            let Ok(text) = std::fs::read_to_string(&cand) else { continue };
            let body = match text.strip_prefix("---").and_then(|rest| rest.splitn(2, "\n---").nth(1)) {
                Some(b) => b.trim_start_matches(['\n', '-']).to_string(),
                None => text,
            };
            return Ok(body.get(..(64 * 1024).min(body.len())).unwrap_or(&body).to_string());
        }
    }
    Err(format!("skill '{name}' not found"))
}

fn skills_list_json() -> serde_json::Value {
    serde_json::json!({ "skills": skills_scan()
        .into_iter()
        .map(|(name, desc, source)| serde_json::json!({"name": name, "desc": desc, "source": source}))
        .collect::<Vec<_>>() })
}

fn skill_by_name_json(name: &str) -> serde_json::Value {
    match skill_body(name) {
        Ok(body) => serde_json::json!({"name": name, "body": body}),
        Err(e) => serde_json::json!({"error": e}),
    }
}

/* ── response helpers ─────────────────────────────── */

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""), 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

/// One level of the workspace file tree for the Agent panel.
/// `query` is `ws=<urlencoded absolute folder>&path=<urlencoded relative path>`.
fn tree_json(query: &str) -> serde_json::Value {
    let get = |key: &str| {
        let prefix = format!("{key}=");
        query
            .split('&')
            .find_map(|kv| kv.strip_prefix(&prefix).map(str::to_string))
            .unwrap_or_default()
    };
    let ws = percent_decode(&get("ws"));
    let rel_raw = percent_decode(&get("path"));
    let rel = if rel_raw.is_empty() { ".".to_string() } else { rel_raw };
    let root = PathBuf::from(&ws);
    if ws.is_empty() || !root.is_dir() {
        return serde_json::json!({"error": format!("not a folder: {ws}")});
    }
    let dir = match agent::safe_join(&root, &rel, false) {
        Ok(d) => d,
        Err(e) => return serde_json::json!({"error": e}),
    };
    let rel_prefix = if rel == "." { String::new() } else { format!("{rel}/") };
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return serde_json::json!({"error": format!("cannot read {rel}")});
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let child = format!("{rel_prefix}{name}");
        if e.path().is_dir() {
            dirs.push(serde_json::json!({"name": name, "path": child}));
        } else {
            let size = e.metadata().map(|m| m.len()).unwrap_or(0);
            files.push(serde_json::json!({"name": name, "path": child, "size": size}));
        }
    }
    dirs.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    files.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    serde_json::json!({"path": rel, "dirs": dirs, "files": files})
}

fn text(w: &mut TcpStream, status: &str, ctype: &str, body: &[u8]) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n\
         Access-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n",
        body.len()
    );
    w.write_all(head.as_bytes())?;
    w.write_all(body)
}

fn j(w: &mut TcpStream, code: u16, f: impl FnOnce() -> serde_json::Value) -> std::io::Result<()> {
    let value = f();
    // Handlers report their own failures as {"error": ...} — surface those as
    // HTTP 400 so the UI's `!r.ok` check throws with the message.
    let status = if code == 404 {
        "404 Not Found"
    } else if value.get("error").is_some() {
        "400 Bad Request"
    } else {
        match code {
            202 => "202 Accepted",
            _ => "200 OK",
        }
    };
    let body = serde_json::to_string(&value).unwrap_or_else(|_| "{}".into());
    text(w, status, "application/json", body.as_bytes())
}

fn err_json(w: &mut TcpStream, code: u16, msg: &str) -> std::io::Result<()> {
    let body = serde_json::json!({"error": msg}).to_string();
    let s = match code { 400 => "400 Bad Request", 409 => "409 Conflict", _ => "502 Bad Gateway" };
    text(w, s, "application/json", body.as_bytes())
}

fn preflight(w: &mut TcpStream) -> std::io::Result<()> {
    w.write_all(
        "HTTP/1.1 204 No Content\r\nAccess-Control-Allow-Origin: *\r\n\
         Access-Control-Allow-Methods: POST, GET, OPTIONS\r\n\
         Access-Control-Allow-Headers: Content-Type\r\nConnection: close\r\n\r\n"
            .as_bytes(),
    )
}

/* ── API handlers (each loads config.json fresh, so edits apply live) ── */

fn cfg_or_panic() -> Config {
    crate::load_config().unwrap_or_else(|e| {
        // The UI can't function without a config; fail the process loudly.
        panic!("config unreadable: {e}")
    })
}

/// Whether the loaded engine has a vision projector (mmproj) and accepts
/// image parts. Probed from /props with a short timeout so a wedged engine
/// can't stall the UI's status polling.
pub(crate) fn engine_vision(cfg: &Config) -> bool {
    let Some(body) = crate::net::http_get(
        cfg.engine.host.as_str(),
        cfg.engine.port,
        "/props",
        std::time::Duration::from_millis(700),
    ) else {
        return false;
    };
    serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.pointer("/modalities/vision").and_then(|b| b.as_bool()))
        .unwrap_or(false)
}

fn status_json() -> serde_json::Value {
    let cfg = cfg_or_panic();
    let healthy = crate::engine_healthy(&cfg);
    let st = crate::state::load(&crate::state_path()).unwrap_or_default();
    let pid_alive = st.engine_pid.is_some_and(crate::pid_alive);
    let load = LOAD.lock().unwrap();
    let vision = healthy && engine_vision(&cfg);
    let ocr = serde_json::json!({
        "available": crate::ocr::available(&cfg),
        "running": crate::ocr::running(&cfg),
    });
    serde_json::json!({
        "engine": { "healthy": healthy, "url": cfg.engine_base_url(), "pidAlive": pid_alive, "ctxSize": cfg.engine.ctx_size, "vision": vision, "ocr": ocr },
        "model": (!st.model.is_empty()).then(|| serde_json::json!({
            "path": st.model, "alias": Config::alias_for(Path::new(&st.model)),
        })).unwrap_or(serde_json::Value::Null),
        "loading": {
            "active": load.as_ref().is_some_and(|l| l.active),
            "target": load.as_ref().and_then(|l| l.target.clone()),
            "error": load.as_ref().and_then(|l| l.error.clone()),
        },
        "remoteApis": cfg.remote_apis.iter()
            .map(|p| serde_json::json!({"name": p.name, "model": p.model}))
            .collect::<Vec<_>>(),
        "providers": cfg.providers.iter()
            .map(|p| serde_json::json!({
                "id": p.id, "name": p.name, "enabled": p.enabled,
                "models": p.models.iter()
                    .map(|m| serde_json::json!({"id": m.id, "enabled": m.enabled, "vision": m.vision}))
                    .collect::<Vec<_>>(),
            }))
            .collect::<Vec<_>>(),
    })
}

/// Live system + engine telemetry for the dashboard. The tier split is the
/// Colibri idea made visible on hardware we can actually measure: VRAM
/// residency comes from nvidia-smi's per-process CUDA accounting, RAM from
/// the engine's working set, and disk is the remainder of the model file.
/// Both memory numbers include KV cache and compute buffers, so the split is
/// an honest estimate, not an exact ledger.
fn metrics_json() -> serde_json::Value {
    let cfg = cfg_or_panic();
    let st = crate::state::load(&crate::state_path()).unwrap_or_default();
    let healthy = crate::engine_healthy(&cfg);
    let pid = st.engine_pid;
    let alive = pid.is_some_and(crate::pid_alive);
    let model_size = if st.model.is_empty() {
        0
    } else {
        std::fs::metadata(&st.model).map(|m| m.len()).unwrap_or(0)
    };
    let ws_bytes = pid.filter(|_| alive).and_then(crate::telemetry::working_set).unwrap_or(0);
    let mut vram_bytes = pid.filter(|_| alive).map(crate::telemetry::gpu_vram_bytes_of).unwrap_or(0);
    let gpus = crate::telemetry::gpus();
    if vram_bytes == 0 && alive {
        // WDDM fallback when even the GPU Engine counter came up empty:
        // attribute the whole GPU's usage (desktop apps hold a few hundred
        // MiB, so this slightly overestimates — the bar is labelled estimate).
        vram_bytes = gpus.first().map(|g| g.used * 1024 * 1024).unwrap_or(0);
    }
    let vram_model = vram_bytes.min(model_size);
    let ram_resident = ws_bytes.saturating_sub(vram_bytes).min(model_size.saturating_sub(vram_model));
    let disk = model_size.saturating_sub(vram_model).saturating_sub(ram_resident);
    let ram = crate::telemetry::sys_ram();
    let profile = if st.model.is_empty() {
        None
    } else {
        cfg.profile_for(std::path::Path::new(&st.model)).map(|p| {
            serde_json::json!({
                "note": p.note, "fitTargetMib": p.fit_target_mib,
                "flashAttn": p.flash_attn, "threads": p.threads, "savedAt": p.saved_at,
            })
        })
    };
    serde_json::json!({
        "healthy": healthy,
        "engine": {
            "pid": pid,
            "alive": alive,
            "uptimeSec": st.started_at.map(|t| crate::patch::unix_now().saturating_sub(t)).unwrap_or(0),
            "ramWorkingSetBytes": ws_bytes,
            "vramUsedBytes": vram_bytes,
        },
        "ram": ram.map(|r| serde_json::json!({"totalBytes": r.total, "availBytes": r.avail})),
        "gpus": gpus.iter().map(|g| serde_json::json!({
            "name": g.name, "totalMib": g.total, "usedMib": g.used,
        })).collect::<Vec<_>>(),
        "model": (!st.model.is_empty()).then(|| serde_json::json!({
            "alias": crate::config::Config::alias_for(std::path::Path::new(&st.model)),
            "path": st.model,
            "sizeBytes": model_size,
        })),
        "tiers": {"vramBytes": vram_model, "ramBytes": ram_resident, "diskBytes": disk},
        "ramLimitGB": cfg.ram_limit_gb,
        "threads": cfg.engine.threads,
        "ctxSize": cfg.engine.ctx_size,
        "profile": profile,
    })
}

fn models_json() -> serde_json::Value {
    let cfg = cfg_or_panic();
    let loaded = crate::loaded_model();
    let models: Vec<_> = discover_cached(&cfg)
        .into_iter()
        .map(|p| {
            serde_json::json!({
                "path": p.display().to_string(),
                "name": Config::alias_for(&p),
                "size": std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0),
                "loaded": loaded.as_ref().is_some_and(|l| crate::same_path(l, &p)),
                "isDefault": crate::same_path(&cfg.default_model, &p),
            })
        })
        .collect();
    serde_json::json!({
        "models": models,
        "scanDirs": cfg.engine.scan_dirs.iter().map(|d| d.display().to_string()).collect::<Vec<_>>(),
        "excludePatterns": cfg.engine.exclude_patterns,
        "defaultModel": cfg.default_model.display().to_string(),
        "ramLimitGB": cfg.ram_limit_gb,
        "remoteApis": cfg.remote_apis.iter()
            .map(|p| serde_json::json!({"name": p.name, "model": p.model}))
            .collect::<Vec<_>>(),
        "providers": cfg.providers.iter()
            .map(|p| serde_json::json!({
                "id": p.id, "name": p.name, "enabled": p.enabled,
                "models": p.models.iter()
                    .map(|m| serde_json::json!({"id": m.id, "enabled": m.enabled, "vision": m.vision}))
                    .collect::<Vec<_>>(),
            }))
            .collect::<Vec<_>>(),
    })
}

fn body_json(body: &[u8]) -> Result<serde_json::Value, String> {
    serde_json::from_slice(body).map_err(|e| format!("bad JSON body: {e}"))
}

/// Run a `&Value -> Result<Value,_>` handler over the parsed request body,
/// mapping parse and handler errors into the standard error envelope.
fn body_handler(
    body: &[u8],
    f: impl FnOnce(&serde_json::Value) -> Result<serde_json::Value, String>,
) -> serde_json::Value {
    match body_json(body) {
        Ok(v) => match f(&v) {
            Ok(r) => r,
            Err(e) => serde_json::json!({"error": e}),
        },
        Err(e) => serde_json::json!({"error": e}),
    }
}

/// POST /api/memory — `{action:"set", text}` replaces the file (UI editor),
/// `{action:"add", text}` appends one bullet (agent / quick note).
fn memory_write_json(body: &[u8]) -> serde_json::Value {
    let v = match body_json(body) {
        Ok(v) => v,
        Err(e) => return serde_json::json!({"error": e}),
    };
    let text = v.get("text").and_then(|t| t.as_str()).unwrap_or("");
    match v.get("action").and_then(|a| a.as_str()).unwrap_or("add") {
        "set" => match crate::store::memory_write(text) {
            Ok(r) => r,
            Err(e) => serde_json::json!({ "error": e }),
        },
        _ => match crate::store::memory_add(text) {
            Ok(_) => serde_json::json!({ "ok": true }),
            Err(e) => serde_json::json!({ "error": e }),
        },
    }
}

/// Add a folder to engine.scanDirs in config.json (deduped) and return the
/// refreshed model list.
fn scan(body: &[u8]) -> serde_json::Value {
    let path = body_json(body).ok().and_then(|v| v["path"].as_str().map(String::from));
    let Some(path) = path else {
        return serde_json::json!({"error": "body must be {\"path\": \"...\"}"});
    };
    let dir = PathBuf::from(&path);
    if !dir.is_dir() {
        return serde_json::json!({"error": format!("not a folder: {path}")});
    }
    match crate::add_scan_dir(&crate::config_path(), &dir) {
        Ok(added) => {
            *MODEL_CACHE.lock().unwrap() = None;
            let mut v = models_json();
            v["added"] = serde_json::json!(added);
            v
        }
        Err(e) => serde_json::json!({"error": e}),
    }
}

/// Kick off a hot-swap in a background thread; the UI polls /api/status.
fn load(body: &[u8]) -> serde_json::Value {
    let needle = body_json(body)
        .ok()
        .and_then(|v| v["path"].as_str().or_else(|| v["name"].as_str()).map(String::from));
    let Some(needle) = needle else {
        return serde_json::json!({"error": "body must be {\"path\": \"...\"} or {\"name\": \"partial\"}"});
    };
    let cfg = cfg_or_panic();
    let model = match cfg.resolve_model(Some(&needle)) {
        Ok(m) => m,
        Err(e) => return serde_json::json!({"error": e}),
    };
    // Refuse before accepting, so the UI gets an immediate error instead of
    // an engine restart that fails a minute later.
    if let Some(err) = cfg.ram_limit_violation(&model) {
        return serde_json::json!({"error": err});
    }
    {
        let mut g = LOAD.lock().unwrap();
        if g.as_ref().is_some_and(|l| l.active) {
            return serde_json::json!({"error": "a model is already loading", "busy": true});
        }
        *g = Some(LoadStatus {
            active: true,
            target: Some(model.display().to_string()),
            error: None,
        });
    }
    let model_for_thread = model.clone();
    std::thread::spawn(move || {
        let result = crate::perform_load(&model_for_thread);
        let mut g = LOAD.lock().unwrap();
        if let Some(l) = g.as_mut() {
            l.active = false;
            l.error = result.err().map(|e| e.lines().last().unwrap_or(&e).to_string());
        }
    });
    serde_json::json!({"loading": true, "target": model.display().to_string()})
}

fn settings_json() -> serde_json::Value {
    let cfg = cfg_or_panic();
    let e = &cfg.engine;
    serde_json::json!({
        "ctxSize": e.ctx_size, "threads": e.threads,
        "gpuLayers": e.gpu_layers, "extraArgs": e.extra_args,
        "host": e.host, "port": e.port,
        "ramLimitGB": cfg.ram_limit_gb,
        "remoteApis": cfg.remote_apis,
    })
}

/// Full provider records (key included — the page has a masked input, and
/// this app is local-only, matching the remoteApis behavior in settings).
fn providers_json() -> serde_json::Value {
    let cfg = cfg_or_panic();
    serde_json::json!({ "providers": cfg.providers })
}

/// Full replace of config.json's providers array. Every entry needs an id
/// and a name; format defaults to openai; model ids must be non-empty.
fn providers_write(body: &[u8]) -> serde_json::Value {
    let v = match body_json(body) {
        Ok(v) => v,
        Err(e) => return serde_json::json!({"error": e}),
    };
    let Some(list) = v.get("providers").and_then(|p| p.as_array()) else {
        return serde_json::json!({"error": "body must be {\"providers\": [...]}"});
    };
    for p in list {
        let s = |k: &str| p.get(k).and_then(|n| n.as_str()).map(str::trim).unwrap_or("");
        if s("id").is_empty() || s("name").is_empty() {
            return serde_json::json!({"error": "every provider needs an id and a name"});
        }
        let fmt = p.get("format").and_then(|f| f.as_str()).unwrap_or("openai");
        if !["openai", "anthropic", "responses"].contains(&fmt) {
            return serde_json::json!({"error": format!("unknown provider format: {fmt}")});
        }
        // Model ids may be empty (the UI autosaves right after "Add model");
        // empty-id models are inert — chat routing skips them.
    }
    let path = crate::config_path();
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return serde_json::json!({"error": "cannot read config.json"});
    };
    let mut cfg: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(c) => c,
        Err(e) => return serde_json::json!({"error": format!("bad config.json: {e}")}),
    };
    cfg.as_object_mut()
        .unwrap()
        .insert("providers".into(), serde_json::Value::Array(list.clone()));
    match serde_json::to_string_pretty(&cfg)
        .map_err(|e| e.to_string())
        .and_then(|s| std::fs::write(&path, s).map_err(|e| e.to_string()))
    {
        Ok(()) => serde_json::json!({"saved": true}),
        Err(e) => serde_json::json!({"error": format!("could not write config.json: {e}")}),
    }
}

fn settings_write(body: &[u8]) -> serde_json::Value {
    let v = match body_json(body) {
        Ok(v) => v,
        Err(e) => return serde_json::json!({"error": e}),
    };
    let path = crate::config_path();
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return serde_json::json!({"error": "cannot read config.json"});
    };
    let mut cfg: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(c) => c,
        Err(e) => return serde_json::json!({"error": format!("bad config.json: {e}")}),
    };
    let eng = match cfg.get_mut("engine") {
        Some(serde_json::Value::Object(m)) => m,
        _ => return serde_json::json!({"error": "config.json has no engine object"}),
    };
    for (key, field) in [
        ("ctxSize", "ctx_size"),
        ("threads", "threads"),
        ("gpuLayers", "gpu_layers"),
        ("extraArgs", "extra_args"),
    ] {
        if let Some(val) = v.get(key) {
            eng.insert(field.into(), val.clone());
        }
    }
    // ramLimitGB lives at the config root; null clears it. Bounded so a bad
    // value can't make config.json unparseable for the supervisor.
    if let Some(val) = v.get("ramLimitGB") {
        let ok = match val {
            serde_json::Value::Null => true,
            serde_json::Value::Number(n) => (1.0..=64.0).contains(&n.as_f64().unwrap_or(0.0)),
            _ => false,
        };
        if !ok {
            return serde_json::json!({"error": "ramLimitGB must be null or a number between 1 and 64"});
        }
        let root = cfg.as_object_mut().unwrap();
        if val.is_null() {
            root.remove("ramLimitGB");
        } else {
            root.insert("ramLimitGB".into(), val.clone());
        }
    }
    // remoteApis: full replace; every entry needs a name and a base URL
    if let Some(val) = v.get("remoteApis") {
        let ok = val.as_array().is_some_and(|a| a.iter().all(|x| {
            let s = |k: &str| x.get(k).and_then(|n| n.as_str()).map(str::trim).unwrap_or("");
            !s("name").is_empty() && !s("baseUrl").is_empty()
        }));
        if !ok {
            return serde_json::json!({"error": "remoteApis entries need a name and a baseUrl"});
        }
        let root = cfg.as_object_mut().unwrap();
        root.insert("remoteApis".into(), val.clone());
    }
    match serde_json::to_string_pretty(&cfg)
        .map_err(|e| e.to_string())
        .and_then(|s| std::fs::write(&path, s).map_err(|e| e.to_string()))
    {
        Ok(()) => serde_json::json!({"saved": true}),
        Err(e) => serde_json::json!({"error": format!("could not write config.json: {e}")}),
    }
}

/// Boot the dsh agent IDE on demand (file tree, terminal, agent editing) and
/// hand the authenticated URL back — the UI embeds it inside this window, so
/// everything stays in the one desktop app. The app also pre-starts the IDE,
/// so this usually answers instantly; ensure_dsh_url single-flights boots.
fn dsh_open() -> serde_json::Value {
    match crate::ensure_dsh_url() {
        Ok(url) => serde_json::json!({"url": url}),
        Err(e) => serde_json::json!({"error": e}),
    }
}

/// Route a chat request: the local llama-server by default, a legacy
/// remoteApis profile by exact name, or a Providers-page entry selected as
/// "prov:<id>" (the UI also sends the chosen "model" explicitly; the
/// provider's first enabled model is the fallback).
fn proxy_chat(client: &mut TcpStream, body: &[u8]) -> std::io::Result<()> {
    let cfg = crate::load_config().map_err(std::io::Error::other)?;
    // strip the UI's "api" selector; it never goes upstream
    let mut req: serde_json::Value =
        serde_json::from_slice(body).unwrap_or_else(|_| serde_json::json!({}));
    let api_name = req
        .as_object_mut()
        .and_then(|o| o.remove("api"))
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default();
    if let Some(prov_id) = api_name.strip_prefix("prov:") {
        let Some(p) = cfg.providers.iter().find(|p| p.id == prov_id) else {
            return match err_json(client, 404, &format!("unknown provider: {prov_id}")) {
                _ => Ok(()),
            };
        };
        if !p.enabled {
            return match err_json(client, 400, &format!("provider {} is disabled", p.name)) {
                _ => Ok(()),
            };
        }
        // Resolve the model: the UI's explicit pick, else the first enabled
        // model, else any model. No model at all = refuse with a clear error.
        let has_model = req.get("model").and_then(|m| m.as_str()).is_some_and(|m| !m.trim().is_empty());
        if !has_model {
            let fallback = p
                .models
                .iter()
                .find(|m| m.enabled && !m.id.trim().is_empty())
                .or_else(|| p.models.iter().find(|m| !m.id.trim().is_empty()));
            match fallback {
                Some(m) => {
                    if let Some(o) = req.as_object_mut() {
                        o.insert("model".into(), serde_json::Value::String(m.id.clone()));
                    }
                }
                None => {
                    return match err_json(client, 400, &format!("provider {} has no models", p.name)) {
                        _ => Ok(()),
                    };
                }
            }
        }
        // Universal OCR fallback: a provider model not marked vision-capable
        // gets its image attachments transcribed first — same as the local
        // engine path. An unknown model id passes through untouched.
        let target_vision = req
            .get("model")
            .and_then(|m| m.as_str())
            .and_then(|mid| p.models.iter().find(|m| m.id == mid))
            .map(|m| m.vision);
        if let Err(e) = ocr_rewrite_request(&cfg, &mut req, target_vision) {
            return match err_json(client, 502, &e) {
                _ => Ok(()),
            };
        }
        let up_body = serde_json::to_vec(&req).unwrap_or_else(|_| body.to_vec());
        return crate::providers::proxy_chat_provider(client, &up_body, p);
    }
    let profile = (!api_name.is_empty())
        .then(|| cfg.remote_apis.iter().find(|p| p.name == api_name))
        .flatten();
    // The UI selects a profile by name and sends no model of its own — the
    // profile's model in config.json is the authoritative choice, and remote
    // APIs reject model-less requests (OpenRouter: "No models provided").
    if let Some(p) = profile {
        // Remote profiles can declare "vision": false in config.json for a
        // text-only model; unknown (None) passes images through as before.
        if let Err(e) = ocr_rewrite_request(&cfg, &mut req, p.vision) {
            return match err_json(client, 502, &e) {
                _ => Ok(()),
            };
        }
        if !p.model.trim().is_empty() {
            if let Some(o) = req.as_object_mut() {
                o.entry("model".to_string())
                    .or_insert_with(|| serde_json::Value::String(p.model.clone()));
            }
        }
    }
    let up_body = serde_json::to_vec(&req).unwrap_or_else(|_| body.to_vec());
    match profile {
        Some(p) => proxy_chat_remote(client, &up_body, p),
        None => proxy_chat_local(client, &up_body, &cfg),
    }
}

/// Route the chat through a remote OpenAI-compatible API by piping the
/// request and its streamed (SSE) response through Windows' bundled curl.exe
/// — std Rust has no TLS, and curl decodes chunked encoding for us.
fn proxy_chat_remote(
    client: &mut TcpStream,
    body: &[u8],
    p: &crate::config::RemoteApi,
) -> std::io::Result<()> {
    use std::process::{Command, Stdio};
    let base = p.base_url.trim_end_matches('/');
    let url = if base.ends_with("/v1") {
        format!("{base}/chat/completions")
    } else {
        format!("{base}/v1/chat/completions")
    };
    let tmp = std::env::temp_dir().join(format!(
        "oh-chat-{}-{}.json",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0)
    ));
    std::fs::write(&tmp, body)?;

    let mut cmd = Command::new("curl");
    cmd.args(["-s", "-N", "-i", "--max-time", "600", "-H", "Expect:"])
        .arg("-H")
        .arg(format!("Authorization: Bearer {}", p.api_key))
        .arg("-H")
        .arg("Content-Type: application/json")
        .arg("--data-binary")
        .arg(format!("@{}", tmp.display()))
        .arg(&url)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            return match err_json(client, 502, &format!("could not start curl: {e}")) {
                _ => Ok(()),
            };
        }
    };
    let mut reader = BufReader::new(child.stdout.take().unwrap());

    let mut line = String::new();
    reader.read_line(&mut line)?;
    let status = line.split_whitespace().nth(1).unwrap_or("502").to_string();
    let mut head = format!("HTTP/1.1 {status}\r\nConnection: close\r\nAccess-Control-Allow-Origin: *\r\n");
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 || line.trim().is_empty() {
            break;
        }
        let lower = line.to_ascii_lowercase();
        if lower.starts_with("transfer-encoding:") || lower.starts_with("connection:") {
            continue;
        }
        head.push_str(&line);
    }
    head.push_str("\r\n");
    client.write_all(head.as_bytes())?;
    let n = std::io::copy(&mut reader, client)?;
    let _ = child.wait();
    let _ = std::fs::remove_file(&tmp);
    if n == 0 {
        return match err_json(
            client,
            502,
            "remote API gave no response — check the base URL, key and model",
        ) {
            _ => Ok(()),
        };
    }
    Ok(())
}

/// Universal OCR fallback for a parsed chat request. `target_vision` says
/// whether the destination model accepts images: Some(true) = vision model,
/// images pass through untouched; Some(false) = transcribe them first; None
/// = unknown target, pass through and let the far end decide. Every chat
/// route (local engine, provider, remote profile) funnels through here so
/// any vision-less model gets the DeepSeek-OCR-2 treatment. Returns whether
/// the request was rewritten (images replaced with transcriptions).
pub(crate) fn ocr_rewrite_request(
    cfg: &Config,
    req: &mut serde_json::Value,
    target_vision: Option<bool>,
) -> Result<bool, String> {
    let images = crate::ocr::collect_images(req);
    if images.is_empty() {
        return Ok(false);
    }
    match target_vision {
        Some(true) | None => return Ok(false),
        Some(false) => {}
    }
    if !crate::ocr::available(cfg) {
        return Err(
            "the target model can't see images and no OCR fallback is configured — \
             add engine.ocrFallback to config.json"
                .into(),
        );
    }
    let texts = crate::ocr::transcribe_cached(cfg, &images)
        .map_err(|e| format!("OCR fallback failed: {e}"))?;
    let opts: Vec<Option<String>> = texts.into_iter().map(Some).collect();
    crate::ocr::rewrite_with_ocr(req, &opts);
    Ok(true)
}

/// Pass the chat body through unchanged unless it needs the OCR fallback:
/// image parts present, local engine without vision, and a usable OCR
/// sidecar configured. Returns the original body when nothing needs doing.
fn ocr_rewrite<'a>(cfg: &Config, body: &'a [u8]) -> Result<std::borrow::Cow<'a, [u8]>, String> {
    let mut req: serde_json::Value = serde_json::from_slice(body)
        .map_err(|e| format!("bad chat JSON: {e}"))?;
    let modified = ocr_rewrite_request(cfg, &mut req, Some(engine_vision(cfg)))?;
    if !modified {
        return Ok(std::borrow::Cow::Borrowed(body));
    }
    serde_json::to_vec(&req)
        .map(std::borrow::Cow::Owned)
        .map_err(|e| format!("chat re-encode: {e}"))
}

/// Local engine path: pipe the browser's chat request straight to
/// llama-server's OpenAI endpoint, passing the upstream response (SSE stream
/// included) through to the browser. Chunked transfer framing, when upstream
/// uses it, is decoded here so the browser only ever sees raw SSE events.
fn proxy_chat_local(client: &mut TcpStream, body: &[u8], cfg: &Config) -> std::io::Result<()> {
    // Image attachments with a vision-less model: transcribe the images
    // through the OCR fallback sidecar first so any loaded model can "see"
    // them. Transcriptions are cached — the UI resends image history with
    // every message, and re-OCRing it all each turn would stall the chat.
    let body: std::borrow::Cow<'_, [u8]> = match ocr_rewrite(cfg, body) {
        Ok(b) => b,
        Err(e) => return match err_json(client, 502, &e) {
            _ => Ok(()),
        },
    };
    let mut up = match TcpStream::connect((cfg.engine.host.as_str(), cfg.engine.port)) {
        Ok(u) => u,
        Err(_) => return match err_json(client, 502, "engine is not reachable — is the model loaded?") {
            _ => Ok(()),
        },
    };
    up.set_read_timeout(Some(READ_TIMEOUT)).ok();
    let req = format!(
        "POST /v1/chat/completions HTTP/1.1\r\nHost: {}:{}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nAccept: text/event-stream\r\nConnection: close\r\n\r\n",
        cfg.engine.host,
        cfg.engine.port,
        body.len()
    );
    up.write_all(req.as_bytes())?;
    up.write_all(body.as_ref())?;

    let mut reader = BufReader::new(up);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    // Rewrite the HTTP version (upstream may answer 1.1 keep-alive style) but
    // keep the status; drop Connection, force close on our side.
    let status = line.split_whitespace().nth(1).unwrap_or("502").to_string();
    let mut chunked = false;
    let mut head = format!("HTTP/1.1 {status}\r\nConnection: close\r\nAccess-Control-Allow-Origin: *\r\n");
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 || line.trim().is_empty() {
            break;
        }
        let lower = line.to_ascii_lowercase();
        if lower.starts_with("transfer-encoding:") {
            if lower.contains("chunked") {
                chunked = true;
            }
            continue;
        }
        if lower.starts_with("connection:") {
            continue;
        }
        head.push_str(&line);
    }
    head.push_str("\r\n");
    client.write_all(head.as_bytes())?;
    if !chunked {
        return std::io::copy(&mut reader, client).map(|_| ());
    }
    // Upstream streams SSE with chunked framing. Dropping the header without
    // decoding it leaks hex chunk sizes into the body, so decode the chunks
    // here; we answer Connection: close, so the body just ends at close.
    let mut size_line = String::new();
    loop {
        size_line.clear();
        if reader.read_line(&mut size_line)? == 0 {
            break;
        }
        let n = usize::from_str_radix(size_line.trim(), 16).unwrap_or(0);
        if n == 0 {
            break;
        }
        let mut chunk = vec![0u8; n];
        reader.read_exact(&mut chunk)?;
        size_line.clear();
        let _ = reader.read_line(&mut size_line); // trailing CRLF of the chunk
        client.write_all(&chunk)?;
        client.flush()?;
    }
    Ok(())
}

/// Compile-time self-check of the route table shape.
#[cfg(test)]
mod tests {
    #[test]
    fn ui_embeds_brand() {
        assert!(super::INDEX_HTML.contains("Open Harness"));
    }
}
