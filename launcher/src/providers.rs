//! Provider chat proxy — routes a chat request through a cloud provider in
//! one of three wire formats and translates the answer back into the
//! OpenAI-compatible SSE the UI already streams. The upstream request is
//! piped through Windows' bundled curl.exe (std Rust has no TLS; curl also
//! decodes chunked encoding for us, exactly like the remoteApis path).

use crate::config::Provider;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Command, Stdio};

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
#[cfg(windows)]
use std::os::windows::process::CommandExt;

/// Streaming translator: receives each upstream SSE `data:` payload as a
/// parsed JSON event and writes OpenAI-shaped chunks to the browser. Returns
/// false when the upstream conversation is over (usage + [DONE] already sent).
type Translate = fn(&mut TcpStream, &mut Xlate, &serde_json::Value) -> std::io::Result<bool>;

/// Shared counters for a single streamed answer.
#[derive(Default)]
struct Xlate {
    prompt_tokens: u64,
    completion_tokens: u64,
}

/// Non-streaming finalizer: provider JSON → OpenAI completion JSON.
type Finalize = fn(serde_json::Value, &mut Xlate) -> serde_json::Value;

/// Entry point from server.rs proxy_chat: the body is the UI's chat request
/// with the "api" selector already stripped and "model" resolved. Everything
/// the UI sees on the wire is OpenAI chat-completions shape.
pub fn proxy_chat_provider(client: &mut TcpStream, body: &[u8], p: &Provider) -> std::io::Result<()> {
    let req: serde_json::Value = serde_json::from_slice(body).unwrap_or_else(|_| serde_json::json!({}));
    let stream = req.get("stream").and_then(|v| v.as_bool()).unwrap_or(true);
    match p.format.as_str() {
        "anthropic" => {
            let up = match anthropic_request(&req, p) {
                Ok(u) => u,
                Err(e) => return err(client, 400, &e),
            };
            // https://api.anthropic.com → /v1/messages; a base that already
            // ends in /v1 just gets /messages appended.
            let base = p.base_url.trim_end_matches('/');
            let url = if base.ends_with("/v1") {
                format!("{base}/messages")
            } else {
                format!("{base}/v1/messages")
            };
            run_upstream(
                client,
                &url,
                &[
                    ("x-api-key", p.api_key.trim()),
                    ("anthropic-version", "2023-06-01"),
                ],
                &serde_json::to_vec(&up).unwrap_or_default(),
                stream,
                xlate_anthropic,
                final_anthropic,
            )
        }
        "responses" => {
            let up = match responses_request(&req, p) {
                Ok(u) => u,
                Err(e) => return err(client, 400, &e),
            };
            let url = format!("{}/responses", p.base_url.trim_end_matches('/'));
            run_upstream(
                client,
                &url,
                &[("Authorization", format!("Bearer {}", p.api_key.trim()).as_str())],
                &serde_json::to_vec(&up).unwrap_or_default(),
                stream,
                xlate_responses,
                final_responses,
            )
        }
        // "openai" and anything unrecognized: the native OpenAI-compatible
        // shape. Provider base URLs normally carry their own version path
        // (…/v1, Gemini's …/v1beta/openai); a bare host gets /v1 prepended,
        // mirroring the legacy remoteApis rule.
        _ => {
            let base = p.base_url.trim_end_matches('/');
            let has_path = base
                .split_once("://")
                .and_then(|(_, rest)| rest.split_once('/'))
                .is_some();
            let url = if has_path {
                format!("{base}/chat/completions")
            } else {
                format!("{base}/v1/chat/completions")
            };
            // Keep the upstream body's stream flag in sync with the SSE
            // parse mode run_upstream is given: a stream:false client whose
            // upstream answers with a plain JSON body yields no "data:"
            // events, and the SSE-only parse used to surface that as an
            // empty completion (choices:[], usage 0).
            let mut up: serde_json::Value =
                serde_json::from_slice(body).unwrap_or_else(|_| serde_json::json!({}));
            if let Some(o) = up.as_object_mut() {
                o.insert("stream".into(), serde_json::Value::Bool(true));
            }
            run_upstream(
                client,
                &url,
                &[("Authorization", format!("Bearer {}", p.api_key.trim()).as_str())],
                &serde_json::to_vec(&up).unwrap_or_else(|_| body.to_vec()),
                true,
                xlate_passthrough,
                final_passthrough,
            )
        }
    }
}

/// Spawn curl against `url`, forward its status line + headers (minus
/// hop-by-hop framing) to the browser, then pump the body: streaming answers
/// go through `xlate` one SSE data event at a time; non-streaming ones are
/// buffered and converted by `finalize`.
fn run_upstream(
    client: &mut TcpStream,
    url: &str,
    headers: &[(&str, &str)],
    body: &[u8],
    stream: bool,
    xlate: Translate,
    finalize: Finalize,
) -> std::io::Result<()> {
    let tmp = std::env::temp_dir().join(format!(
        "oh-prov-{}-{}.json",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0)
    ));
    std::fs::write(&tmp, body)?;
    let mut cmd = Command::new("curl");
    cmd.args(["-s", "-N", "-i", "--max-time", "600", "-H", "Expect:", "-H"])
        .arg("Content-Type: application/json");
    for (k, v) in headers {
        cmd.arg("-H").arg(format!("{k}: {v}"));
    }
    cmd.arg("--data-binary")
        .arg(format!("@{}", tmp.display()))
        .arg(url)
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
            return err(client, 502, &format!("could not start curl: {e}"));
        }
    };
    let mut reader = BufReader::new(child.stdout.take().unwrap());

    let mut line = String::new();
    reader.read_line(&mut line)?;
    let status = line.split_whitespace().nth(1).unwrap_or("502").to_string();
    let ok = status.starts_with('2');
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
    // Upstream errors pass through untouched so the UI surfaces the
    // provider's own message (bad key, unknown model, quota…).
    if !ok {
        head.push_str("\r\n");
        client.write_all(head.as_bytes())?;
        client.flush()?;
        let _ = std::io::copy(&mut reader, client)?;
        let _ = child.wait();
        let _ = std::fs::remove_file(&tmp);
        return Ok(());
    }

    if !stream {
        let mut raw = String::new();
        reader.read_to_string(&mut raw)?;
        let _ = child.wait();
        let _ = std::fs::remove_file(&tmp);
        let parsed: serde_json::Value = serde_json::from_str(raw.trim()).unwrap_or(serde_json::Value::Null);
        let mut x = Xlate::default();
        let out = finalize(parsed, &mut x);
        let bytes = serde_json::to_vec(&out).unwrap_or_default();
        // Content-Length rides in the header block; writing it after the blank
        // line would put it inside the body and corrupt the JSON.
        head.push_str(&format!("Content-Length: {}\r\n\r\n", bytes.len()));
        client.write_all(head.as_bytes())?;
        client.write_all(&bytes)?;
        return Ok(());
    }

    head.push_str("\r\n");
    client.write_all(head.as_bytes())?;
    client.flush()?;

    let mut x = Xlate::default();
    let mut finished = false;
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let Some(data) = line.trim().strip_prefix("data:") else { continue };
        let data = data.trim();
        if data.is_empty() {
            continue;
        }
        if data == "[DONE]" {
            // OpenAI-format upstreams end their own stream; forward the
            // terminator and stop — no fallback usage chunk after this, or it
            // would clobber the real usage the upstream just reported.
            client.write_all(b"data: [DONE]\n\n")?;
            finished = true;
            break;
        }
        let Ok(ev) = serde_json::from_str::<serde_json::Value>(data) else { continue };
        if !xlate(client, &mut x, &ev)? {
            finished = true;
            break;
        }
    }
    if !finished {
        // Upstream closed without a terminal event — still emit the usage
        // chunk + terminator so the UI's stream reader ends cleanly.
        write_sse(
            client,
            &serde_json::json!({
                "object": "chat.completion.chunk", "choices": [],
                "usage": {"prompt_tokens": x.prompt_tokens, "completion_tokens": x.completion_tokens},
            }),
        )?;
        client.write_all(b"data: [DONE]\n\n")?;
    }
    client.flush()?;
    let _ = child.wait();
    let _ = std::fs::remove_file(&tmp);
    Ok(())
}

fn write_sse(client: &mut TcpStream, v: &serde_json::Value) -> std::io::Result<()> {
    client.write_all(b"data: ")?;
    client.write_all(serde_json::to_vec(v).unwrap_or_default().as_slice())?;
    client.write_all(b"\n\n")
}

fn chunk(delta: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "object": "chat.completion.chunk",
        "choices": [{"index": 0, "delta": delta}],
    })
}

fn err(client: &mut TcpStream, code: u16, msg: &str) -> std::io::Result<()> {
    let body = serde_json::json!({"error": {"message": msg, "type": "open_harness_error"}});
    let bytes = serde_json::to_vec(&body).unwrap_or_default();
    let head = format!(
        "HTTP/1.1 {code} Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\nAccess-Control-Allow-Origin: *\r\n\r\n",
        bytes.len()
    );
    client.write_all(head.as_bytes())?;
    client.write_all(&bytes)
}

// ── anthropic (/v1/messages) ────────────────────────────────────────────

fn parse_data_url(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(',')?;
    let media = meta.split(';').next()?.to_string();
    if media.is_empty() || data.is_empty() {
        return None;
    }
    Some((media, data.to_string()))
}

/// OpenAI messages → Anthropic body. System messages fold into `system`,
/// image data URLs become base64 blocks, and consecutive same-role messages
/// merge (Anthropic requires strictly alternating roles). max_tokens is
/// mandatory upstream, so it defaults when the UI didn't send one.
fn anthropic_request(req: &serde_json::Value, p: &Provider) -> Result<serde_json::Value, String> {
    if p.api_key.trim().is_empty() {
        return Err("this provider has no API key — add one on the Providers page".into());
    }
    let mut out = serde_json::json!({
        "model": req.get("model").cloned().unwrap_or(serde_json::json!("")),
        "max_tokens": req.get("max_tokens").and_then(|v| v.as_u64()).unwrap_or(4096),
        "stream": req.get("stream").and_then(|v| v.as_bool()).unwrap_or(true),
    });
    let mut system: Vec<String> = Vec::new();
    let mut msgs: Vec<serde_json::Value> = Vec::new();
    let empty = Vec::new();
    let arr = req.get("messages").and_then(|m| m.as_array()).unwrap_or(&empty);
    for m in arr {
        let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("user");
        let parts: Vec<serde_json::Value> = match m.get("content") {
            Some(serde_json::Value::String(s)) => vec![serde_json::json!({"type": "text", "text": s})],
            Some(serde_json::Value::Array(a)) => a.clone(),
            _ => vec![],
        };
        if role == "system" || role == "developer" {
            for part in &parts {
                if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
                    system.push(t.to_string());
                }
            }
            continue;
        }
        let mut blocks: Vec<serde_json::Value> = Vec::new();
        for part in &parts {
            match part.get("type").and_then(|t| t.as_str()).unwrap_or("text") {
                "text" => {
                    let t = part.get("text").and_then(|t| t.as_str()).unwrap_or("");
                    if !t.trim().is_empty() {
                        blocks.push(serde_json::json!({"type": "text", "text": t}));
                    }
                }
                "image_url" => {
                    if let Some(url) = part.pointer("/image_url/url").and_then(|u| u.as_str()) {
                        if let Some((media, data)) = parse_data_url(url) {
                            blocks.push(serde_json::json!({
                                "type": "image",
                                "source": {"type": "base64", "media_type": media, "data": data},
                            }));
                        }
                    }
                }
                _ => {}
            }
        }
        if blocks.is_empty() {
            continue;
        }
        let role = if role == "assistant" { "assistant" } else { "user" };
        if let Some(last) = msgs.last_mut() {
            if last.get("role").and_then(|r| r.as_str()) == Some(role) {
                if let Some(serde_json::Value::Array(c)) = last.get_mut("content") {
                    c.extend(blocks);
                    continue;
                }
            }
        }
        msgs.push(serde_json::json!({"role": role, "content": blocks}));
    }
    if msgs.is_empty() {
        return Err("no messages to send".into());
    }
    if !system.is_empty() {
        out["system"] = serde_json::json!(system.join("\n\n"));
    }
    out["messages"] = serde_json::json!(msgs);
    if let Some(t) = req.get("temperature").and_then(|v| v.as_f64()) {
        out["temperature"] = serde_json::json!(t);
    }
    if let Some(t) = req.get("top_p").and_then(|v| v.as_f64()) {
        out["top_p"] = serde_json::json!(t);
    }
    Ok(out)
}

/// Anthropic → OpenAI SSE, one event at a time.
fn xlate_anthropic(client: &mut TcpStream, x: &mut Xlate, ev: &serde_json::Value) -> std::io::Result<bool> {
    match ev.get("type").and_then(|t| t.as_str()).unwrap_or("") {
        "message_start" => {
            x.prompt_tokens = ev
                .pointer("/message/usage/input_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            Ok(true)
        }
        "content_block_delta" => {
            let delta = ev.get("delta");
            match delta.and_then(|d| d.get("type")).and_then(|t| t.as_str()) {
                Some("text_delta") => {
                    let text = delta.and_then(|d| d.get("text")).and_then(|t| t.as_str()).unwrap_or("");
                    write_sse(client, &chunk(serde_json::json!({"content": text})))
                }
                Some("thinking_delta") => {
                    let text = delta.and_then(|d| d.get("thinking")).and_then(|t| t.as_str()).unwrap_or("");
                    write_sse(client, &chunk(serde_json::json!({"reasoning_content": text})))
                }
                _ => Ok(()),
            }?;
            Ok(true)
        }
        "message_delta" => {
            x.completion_tokens = ev
                .pointer("/usage/output_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(x.completion_tokens);
            Ok(true)
        }
        "message_stop" => {
            write_sse(
                client,
                &serde_json::json!({
                    "object": "chat.completion.chunk", "choices": [],
                    "usage": {"prompt_tokens": x.prompt_tokens, "completion_tokens": x.completion_tokens},
                }),
            )?;
            client.write_all(b"data: [DONE]\n\n")?;
            Ok(false)
        }
        "error" => {
            let msg = ev.pointer("/error/message").and_then(|m| m.as_str()).unwrap_or("provider error");
            write_sse(
                client,
                &serde_json::json!({
                    "object": "chat.completion.chunk",
                    "choices": [{"index": 0, "delta": {"content": format!("\n\nError: {msg}")}}],
                }),
            )?;
            client.write_all(b"data: [DONE]\n\n")?;
            Ok(false)
        }
        _ => Ok(true),
    }
}

/// Non-streaming Anthropic message → OpenAI completion shape.
fn final_anthropic(msg: serde_json::Value, x: &mut Xlate) -> serde_json::Value {
    let mut content = String::new();
    let mut reasoning = String::new();
    if let Some(blocks) = msg.get("content").and_then(|c| c.as_array()) {
        for b in blocks {
            match (b.get("type").and_then(|t| t.as_str()), b.get("text").and_then(|t| t.as_str())) {
                (Some("text"), Some(t)) => content.push_str(t),
                (Some("thinking"), Some(t)) => reasoning.push_str(t),
                _ => {}
            }
        }
    }
    x.prompt_tokens = msg.pointer("/usage/input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
    x.completion_tokens = msg.pointer("/usage/output_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
    let mut message = serde_json::json!({"role": "assistant", "content": content});
    if !reasoning.is_empty() {
        message["reasoning_content"] = serde_json::json!(reasoning);
    }
    serde_json::json!({
        "object": "chat.completion",
        "choices": [{"index": 0, "message": message, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": x.prompt_tokens, "completion_tokens": x.completion_tokens},
    })
}

// ── OpenAI Responses (/responses) ───────────────────────────────────────

fn responses_request(req: &serde_json::Value, p: &Provider) -> Result<serde_json::Value, String> {
    if p.api_key.trim().is_empty() {
        return Err("this provider has no API key — add one on the Providers page".into());
    }
    let mut instructions = String::new();
    let mut input: Vec<serde_json::Value> = Vec::new();
    let empty = Vec::new();
    let arr = req.get("messages").and_then(|m| m.as_array()).unwrap_or(&empty);
    for m in arr {
        let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("user");
        let parts: Vec<serde_json::Value> = match m.get("content") {
            Some(serde_json::Value::String(s)) => vec![serde_json::json!({"type": "text", "text": s})],
            Some(serde_json::Value::Array(a)) => a.clone(),
            _ => vec![],
        };
        if role == "system" || role == "developer" {
            for part in &parts {
                if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
                    if !instructions.is_empty() {
                        instructions.push_str("\n\n");
                    }
                    instructions.push_str(t);
                }
            }
            continue;
        }
        let mut content: Vec<serde_json::Value> = Vec::new();
        for part in &parts {
            match part.get("type").and_then(|t| t.as_str()).unwrap_or("text") {
                "text" => {
                    let t = part.get("text").and_then(|t| t.as_str()).unwrap_or("");
                    if !t.is_empty() {
                        content.push(serde_json::json!({"type": "input_text", "text": t}));
                    }
                }
                "image_url" => {
                    if let Some(url) = part.pointer("/image_url/url").and_then(|u| u.as_str()) {
                        if url.starts_with("data:") || url.starts_with("http") {
                            content.push(serde_json::json!({"type": "input_image", "image_url": url}));
                        }
                    }
                }
                _ => {}
            }
        }
        if content.is_empty() {
            continue;
        }
        let role = if role == "assistant" { "assistant" } else { "user" };
        if let Some(last) = input.last_mut() {
            if last.get("role").and_then(|r| r.as_str()) == Some(role) {
                if let Some(serde_json::Value::Array(c)) = last.get_mut("content") {
                    c.extend(content);
                    continue;
                }
            }
        }
        input.push(serde_json::json!({"role": role, "content": content}));
    }
    if input.is_empty() {
        return Err("no messages to send".into());
    }
    let mut out = serde_json::json!({
        "model": req.get("model").cloned().unwrap_or(serde_json::json!("")),
        "input": input,
        "stream": req.get("stream").and_then(|v| v.as_bool()).unwrap_or(true),
    });
    if !instructions.is_empty() {
        out["instructions"] = serde_json::json!(instructions);
    }
    if let Some(mt) = req.get("max_tokens").and_then(|v| v.as_u64()) {
        out["max_output_tokens"] = serde_json::json!(mt);
    }
    if let Some(t) = req.get("temperature").and_then(|v| v.as_f64()) {
        out["temperature"] = serde_json::json!(t);
    }
    if let Some(t) = req.get("top_p").and_then(|v| v.as_f64()) {
        out["top_p"] = serde_json::json!(t);
    }
    Ok(out)
}

fn xlate_responses(client: &mut TcpStream, x: &mut Xlate, ev: &serde_json::Value) -> std::io::Result<bool> {
    match ev.get("type").and_then(|t| t.as_str()).unwrap_or("") {
        "response.output_text.delta" => {
            let text = ev.get("delta").and_then(|t| t.as_str()).unwrap_or("");
            write_sse(client, &chunk(serde_json::json!({"content": text})))?;
            Ok(true)
        }
        "response.reasoning_text.delta" | "response.reasoning_summary_text.delta" => {
            let text = ev.get("delta").and_then(|t| t.as_str()).unwrap_or("");
            write_sse(client, &chunk(serde_json::json!({"reasoning_content": text})))?;
            Ok(true)
        }
        "response.completed" | "response.incomplete" => {
            x.prompt_tokens = ev.pointer("/response/usage/input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
            x.completion_tokens = ev
                .pointer("/response/usage/output_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(x.completion_tokens);
            write_sse(
                client,
                &serde_json::json!({
                    "object": "chat.completion.chunk", "choices": [],
                    "usage": {"prompt_tokens": x.prompt_tokens, "completion_tokens": x.completion_tokens},
                }),
            )?;
            client.write_all(b"data: [DONE]\n\n")?;
            Ok(false)
        }
        "response.failed" | "error" => {
            let msg = ev
                .pointer("/response/error/message")
                .or_else(|| ev.pointer("/error/message"))
                .and_then(|m| m.as_str())
                .unwrap_or("provider error");
            write_sse(
                client,
                &serde_json::json!({
                    "object": "chat.completion.chunk",
                    "choices": [{"index": 0, "delta": {"content": format!("\n\nError: {msg}")}}],
                }),
            )?;
            client.write_all(b"data: [DONE]\n\n")?;
            Ok(false)
        }
        _ => Ok(true),
    }
}

fn final_responses(resp: serde_json::Value, x: &mut Xlate) -> serde_json::Value {
    let mut content = String::new();
    if let Some(items) = resp.get("output").and_then(|o| o.as_array()) {
        for item in items {
            if let Some(parts) = item.get("content").and_then(|c| c.as_array()) {
                for part in parts {
                    if part.get("type").and_then(|t| t.as_str()) == Some("output_text") {
                        if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
                            content.push_str(t);
                        }
                    }
                }
            }
        }
    }
    if content.is_empty() {
        if let Some(t) = resp.get("output_text").and_then(|t| t.as_str()) {
            content = t.to_string();
        }
    }
    x.prompt_tokens = resp.pointer("/usage/input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
    x.completion_tokens = resp.pointer("/usage/output_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
    serde_json::json!({
        "object": "chat.completion",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": content}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": x.prompt_tokens, "completion_tokens": x.completion_tokens},
    })
}

// ── openai passthrough (provider variant of the remoteApis path) ────────

fn xlate_passthrough(client: &mut TcpStream, _x: &mut Xlate, ev: &serde_json::Value) -> std::io::Result<bool> {
    write_sse(client, ev)?;
    Ok(true)
}

fn final_passthrough(v: serde_json::Value, _x: &mut Xlate) -> serde_json::Value {
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prov(key: &str) -> Provider {
        Provider {
            id: "t".into(),
            name: "t".into(),
            enabled: true,
            base_url: "https://x".into(),
            format: "anthropic".into(),
            api_key: key.into(),
            models: vec![],
        }
    }

    #[test]
    fn anthropic_body_folds_system_and_images() {
        let req = serde_json::json!({
            "model": "m1",
            "messages": [
                {"role": "system", "content": "be brief"},
                {"role": "user", "content": "hi"},
                {"role": "user", "content": [{"type": "text", "text": "again"}]},
                {"role": "assistant", "content": "hello"},
                {"role": "user", "content": [
                    {"type": "text", "text": "see this"},
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}}
                ]}
            ]
        });
        let out = anthropic_request(&req, &prov("k")).unwrap();
        assert_eq!(out["system"], "be brief");
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3, "consecutive user messages merge");
        assert_eq!(msgs[0]["content"][0]["text"], "hi");
        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(msgs[2]["content"][1]["source"]["data"], "AAAA");
        assert_eq!(out["max_tokens"], 4096, "mandatory upstream default");
    }

    #[test]
    fn anthropic_body_needs_key() {
        let req = serde_json::json!({"messages": [{"role": "user", "content": "hi"}]});
        assert!(anthropic_request(&req, &prov("")).is_err());
    }

    #[test]
    fn responses_body_builds_input_and_instructions() {
        let req = serde_json::json!({
            "model": "m1",
            "messages": [
                {"role": "system", "content": "be brief"},
                {"role": "user", "content": "hi"}
            ]
        });
        let mut p = prov("k");
        p.format = "responses".into();
        let out = responses_request(&req, &p).unwrap();
        assert_eq!(out["instructions"], "be brief");
        assert_eq!(out["input"][0]["role"], "user");
        assert_eq!(out["input"][0]["content"][0]["type"], "input_text");
    }
}
