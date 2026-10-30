//! OCR fallback engine: a second llama-server instance running a dedicated
//! vision/OCR model (e.g. DeepSeek-OCR-2) on its own port. Chat models
//! without a vision projector get image attachments transcribed into text
//! before their request is proxied, so any local model can "see" images.
//!
//! The sidecar spawns on demand (first image attachment), stays resident
//! afterwards, and is a plain llama-server like the main engine, so all the
//! same tooling (spawn_logged, /health probes) applies.

use crate::config::{Config, OcrFallback};
use crate::proc::spawn_logged;
use crate::root_dir;
use crate::{net, state, state_path};
use serde_json::json;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// DeepSeek-OCR-2 answers its *native* task prompts well and paraphrases
/// arbitrary instructions ("Extract all text...") instead of transcribing,
/// so the transcribe call uses the model's own "Free OCR." task. When that
/// yields nothing (a photo with no text), the describe prompt runs instead
/// so a text-only chat model can still answer questions about the image.
const OCR_PROMPT: &str = "Free OCR.";
const OCR_DESCRIBE_PROMPT: &str = "Describe this image in detail.";

/// Health-probe timeout for the sidecar.
const OCR_PROBE: Duration = Duration::from_millis(700);
/// Model load can take a while on first spawn (mmap + GPU offload).
const OCR_START_TIMEOUT: Duration = Duration::from_secs(180);
/// One OCR completion (image encode + decode of the transcription).
const OCR_COMPLETION_TIMEOUT: Duration = Duration::from_secs(300);

/// Serializes sidecar startup: two image requests arriving together must not
/// race two llama-server spawns onto the same port.
static OCR_SPAWN_LOCK: Mutex<()> = Mutex::new(());

/// data-URL → transcription. The UI resends image history with every message,
/// so without this every follow-up turn would re-OCR the whole history.
/// Cleared wholesale when it grows past the cap (entries are big; a simple
/// reset is plenty).
static OCR_CACHE: Mutex<Option<std::collections::HashMap<String, String>>> = Mutex::new(None);
const OCR_CACHE_CAP: usize = 64;

/// The configured fallback, but only when it is actually usable: both files
/// must exist on disk. Half-configured entries must not silently swallow
/// image attachments, so they read as "not available".
pub fn usable(cfg: &Config) -> Option<&OcrFallback> {
    cfg.engine
        .ocr_fallback
        .as_ref()
        .filter(|o| o.enabled && o.model.is_file() && o.mmproj.is_file())
}

/// Configured and files present: the UI may offer image attachments even
/// when the loaded model has no vision.
pub fn available(cfg: &Config) -> bool {
    usable(cfg).is_some()
}

/// Sidecar currently answering on its port.
pub fn running(cfg: &Config) -> bool {
    usable(cfg).is_some_and(|o| {
        net::http_status(cfg.engine.host.as_str(), o.port, "/health", OCR_PROBE, None) == Some(200)
    })
}

fn ocr_args(cfg: &Config, o: &OcrFallback) -> Vec<String> {
    vec![
        "-m".into(),
        o.model.display().to_string(),
        "--host".into(),
        cfg.engine.host.clone(),
        "--port".into(),
        o.port.to_string(),
        "-c".into(),
        "8192".into(),
        "-t".into(),
        cfg.engine.threads.to_string(),
        "--alias".into(),
        "ocr-fallback".into(),
        // CPU-only, deliberately. The sidecar must coexist with whatever main
        // model is loaded, and that model's auto-fit grabs VRAM dynamically:
        // weights may fit on GPU while the vision encoder's ~1.6 GB compute
        // graph doesn't, which kills the request with cudaMalloc OOM
        // ("failed to process mtmd chunk"). DeepSeek-OCR-2 is a 3B-MoE with
        // ~0.6B active params: full-CPU OCR runs in ~5 s, and never touches
        // the main model's VRAM.
        "-ngl".into(),
        "0".into(),
        // -ngl 0 alone is not enough: this build offloads the mmproj (vision
        // encoder) to GPU by default regardless of -ngl, and its ~1.6 GB
        // compute graph OOMs whenever the main model's auto-fit has claimed
        // the VRAM in the meantime. Pin the whole sidecar to CPU.
        "--no-mmproj-offload".into(),
        "--mmproj".into(),
        o.mmproj.display().to_string(),
    ]
}

/// Make sure the sidecar is up, spawning it if needed; blocks until healthy.
/// The returned borrow shares `cfg`'s lifetime: callers pass a config that
/// lives for the whole request, so that is all we need.
fn ensure_running<'a>(cfg: &'a Config) -> Result<&'a OcrFallback, String> {
    let lock: MutexGuard<'_, ()> = OCR_SPAWN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _held = lock;
    let Some(o) = usable(cfg) else {
        return Err("OCR fallback is not configured (or its model files are missing)".into());
    };
    if running(cfg) {
        return Ok(o);
    }
    // An orphaned sidecar from a previous run may still hold the port; take
    // it down first so the fresh spawn doesn't die with "address in use".
    if let Some(pid) = net::pid_listening_on(o.port) {
        let _ = crate::proc::kill_tree(pid);
        std::thread::sleep(Duration::from_millis(800));
    }
    let args = ocr_args(cfg, o);
    println!("[oh] starting OCR fallback engine: {}", cfg.engine.exe.display());
    println!("[oh]   model: {}", o.model.display());
    let child = spawn_logged(
        std::process::Command::new(&cfg.engine.exe)
            .args(&args)
            .current_dir(cfg.engine.exe.parent().map(Path::to_path_buf).unwrap_or_else(|| ".".into())),
        &root_dir().join("ocr-engine.log"),
    )
    .map_err(|e| format!("failed to start OCR engine: {e}"))?;
    let pid = child.id();
    let deadline = Instant::now() + OCR_START_TIMEOUT;
    while Instant::now() < deadline {
        if net::http_status(cfg.engine.host.as_str(), o.port, "/health", OCR_PROBE, None) == Some(200) {
            // Remember the sidecar pid for stop/restart hygiene.
            let st = state::load(&state_path()).unwrap_or_default();
            let _ = state::save(
                &state_path(),
                &state::State { ocr_pid: Some(pid), ..st },
            );
            return Ok(o);
        }
        if !crate::pid_alive(pid) {
            return Err(
                "OCR engine exited while starting: see ocr-engine.log (wrong mmproj pairing, \
                 out of VRAM, or unsupported architecture)"
                    .into(),
            );
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Err("OCR engine did not become healthy within 180 s: see ocr-engine.log".into())
}

/// POST one OpenAI-shaped chat request and return the response body, through
/// the same bundled-curl idiom proxy_chat_remote uses (no TLS client in this
/// crate, and curl decodes chunked framing for us).
fn http_post_json(host: &str, port: u16, path: &str, body: &serde_json::Value, timeout_secs: u64) -> Result<String, String> {
    use std::process::{Command, Stdio};
    let tmp = std::env::temp_dir().join(format!(
        "oh-ocr-{}-{}.json",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0)
    ));
    std::fs::write(&tmp, serde_json::to_vec(body).map_err(|e| e.to_string())?)
        .map_err(|e| format!("temp write: {e}"))?;
    let url = format!("http://{host}:{port}{path}");
    let out = Command::new("curl")
        .args(["-s", "-S", "--max-time", &timeout_secs.to_string(), "-H", "Expect:", "-H", "Content-Type: application/json"])
        .arg("--data-binary")
        .arg(format!("@{}", tmp.display()))
        .arg(&url)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();
    let _ = std::fs::remove_file(&tmp);
    match out {
        Ok(o) if o.status.success() && !o.stdout.is_empty() => {
            Ok(String::from_utf8_lossy(&o.stdout).into_owned())
        }
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr);
            Err(if err.trim().is_empty() {
                format!("OCR request failed (curl exit {:?})", o.status.code())
            } else {
                format!("OCR request failed: {}", err.trim())
            })
        }
        Err(e) => Err(format!("could not start curl: {e}")),
    }
}

/// One completion against the sidecar: image + task prompt → model text.
fn ocr_completion(
    cfg: &Config,
    port: u16,
    url: &str,
    prompt: &str,
    max_tokens: u32,
) -> Result<String, String> {
    let body = json!({
        "model": "ocr-fallback",
        "stream": false,
        "max_tokens": max_tokens,
        "messages": [{
            "role": "user",
            "content": [
                {"type": "image_url", "image_url": {"url": url}},
                {"type": "text", "text": prompt},
            ]
        }]
    });
    let resp = http_post_json(
        cfg.engine.host.as_str(),
        port,
        "/v1/chat/completions",
        &body,
        OCR_COMPLETION_TIMEOUT.as_secs(),
    )?;
    let parsed: serde_json::Value = serde_json::from_str(&resp)
        .map_err(|e| format!("OCR response was not JSON: {e}; {resp:.400}"))?;
    let text = parsed
        .pointer("/choices/0/message/content")
        .and_then(|c| c.as_str())
        .map(str::trim)
        .unwrap_or("");
    if text.is_empty() {
        let e = parsed
            .pointer("/error/message")
            .and_then(|m| m.as_str())
            .unwrap_or("empty completion");
        return Err(e.to_string());
    }
    Ok(text.to_string())
}

/// Transcribe every image (data URLs) through the OCR sidecar, in order.
/// On success, one extracted-text string per input, same index. A text-free
/// image falls back to a description instead of failing the whole request.
pub fn transcribe(cfg: &Config, images: &[String]) -> Result<Vec<String>, String> {
    let o = ensure_running(cfg)?;
    let mut texts = Vec::with_capacity(images.len());
    for url in images {
        match ocr_completion(cfg, o.port, url, OCR_PROMPT, 4096) {
            Ok(text) => texts.push(text),
            Err(e) => {
                // "Free OCR." legitimately returns nothing for photos; try
                // the describe task before giving up on this image.
                texts.push(ocr_completion(cfg, o.port, url, OCR_DESCRIBE_PROMPT, 1024)
                    .map_err(|_| e)?);
            }
        }
    }
    Ok(texts)
}

/// `transcribe` with a data-URL cache in front: history images come back
/// every turn and must not re-hit the OCR engine. One result per input, and
/// only images the cache has never seen are actually transcribed.
pub fn transcribe_cached(cfg: &Config, images: &[String]) -> Result<Vec<String>, String> {
    let mut guard = OCR_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let cache = guard.get_or_insert_with(std::collections::HashMap::new);
    let mut texts: Vec<Option<String>> =
        images.iter().map(|u| cache.get(u).cloned()).collect();
    let misses: Vec<usize> = texts
        .iter()
        .enumerate()
        .filter(|(_, t)| t.is_none())
        .map(|(i, _)| i)
        .collect();
    if !misses.is_empty() {
        let fresh: Vec<String> =
            misses.iter().map(|&i| images[i].clone()).collect();
        for (k, text) in transcribe(cfg, &fresh)?.into_iter().enumerate() {
            if cache.len() >= OCR_CACHE_CAP {
                cache.clear();
            }
            cache.insert(fresh[k].clone(), text.clone());
            texts[misses[k]] = Some(text);
        }
    }
    Ok(texts.into_iter().map(|t| t.unwrap_or_default()).collect())
}

/// Pull the data-URL of every image_url part in a chat request, in order.
pub fn collect_images(req: &serde_json::Value) -> Vec<String> {
    let mut urls = Vec::new();
    let Some(msgs) = req.pointer("/messages").and_then(|m| m.as_array()) else {
        return urls;
    };
    for msg in msgs {
        let Some(parts) = msg.get("content").and_then(|c| c.as_array()) else {
            continue;
        };
        for part in parts {
            if part.get("type").and_then(|t| t.as_str()) == Some("image_url") {
                if let Some(u) = part.pointer("/image_url/url").and_then(|u| u.as_str()) {
                    if u.starts_with("data:image/") {
                        urls.push(u.to_string());
                    }
                }
            }
        }
    }
    urls
}

/// Replace every image_url part in the request with a text part carrying its
/// transcription. `texts[i]` corresponds to the i-th image from
/// `collect_images`; a None entry (OCR failed) becomes an honest note rather
/// than a silent drop.
pub fn rewrite_with_ocr(req: &mut serde_json::Value, texts: &[Option<String>]) {
    let Some(msgs) = req
        .pointer_mut("/messages")
        .and_then(|m| m.as_array_mut())
    else {
        return;
    };
    let mut idx = 0usize;
    for msg in msgs {
        let Some(parts) = msg.get_mut("content").and_then(|c| c.as_array_mut()) else {
            continue;
        };
        for part in parts {
            if part.get("type").and_then(|t| t.as_str()) != Some("image_url") {
                continue;
            }
            let replacement = match texts.get(idx).and_then(|t| t.clone()) {
                Some(text) => json!({
                    "type": "text",
                    "text": format!("[transcribed image attachment (OCR)]\n{text}")
                }),
                None => json!({
                    "type": "text",
                    "text": "[an image was attached but could not be transcribed: the OCR engine failed]"
                }),
            };
            *part = replacement;
            idx += 1;
        }
    }
}
