//! config.json schema for the IDE stack.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// A measured performance profile for one model, saved by `open-harness tune`
/// (llama-bench sweep on this machine, this file). Applied automatically in
/// engine_args() whenever that model loads; manual Settings values the user
/// typed still win, because tune never edits them.
#[derive(Deserialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct PerfProfile {
    pub threads: Option<u32>,
    pub gpu_layers: Option<u32>,
    /// GPU memory margin (MiB) for the engine's auto-fit, from the winning
    /// bench config. None = use the engine's own default.
    pub fit_target_mib: Option<u32>,
    /// Flash Attention setting measured fastest: "on" / "off" / "auto".
    pub flash_attn: Option<String>,
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub saved_at: u64,
}

/// A vision adapter (mmproj) paired with the model family it belongs to.
/// Attaching the wrong mmproj crashes the engine (n_embd mismatch), so the
/// pairing is explicit config, not a filename guess.
#[derive(Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct MmprojRule {
    /// Substring matched (case-insensitive) against the model's filename.
    pub model_pattern: String,
    pub path: PathBuf,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineConfig {
    pub exe: PathBuf,
    pub host: String,
    pub port: u16,
    /// "llamacpp" (default; engine.exe + GGUF models) or "airllm" (Python
    /// layer-streaming sidecar for checkpoints too big to fit in RAM/VRAM;
    /// discovers HF safetensors folders instead of .gguf files).
    #[serde(default)]
    pub mode: String,
    /// AirLLM mode: python interpreter of the sidecar venv. Defaults to
    /// engine/airllm/.venv/Scripts/python.exe next to the launcher root.
    #[serde(default, rename = "airllmPython")]
    pub airllm_python: Option<PathBuf>,
    /// AirLLM mode: block-wise quantization of the streamed shards
    /// ("4bit" / "8bit", ~3x faster decode) or none.
    #[serde(default, rename = "airllmCompression")]
    pub airllm_compression: Option<String>,
    /// AirLLM mode: max sequence length passed to the model (its default of
    /// 512 is too small for chat; the engine ctx_size is a sane value).
    #[serde(default, rename = "airllmMaxSeqLen")]
    pub airllm_max_seq_len: Option<u32>,
    /// Deprecated: the served alias is derived from the model filename.
    /// Kept only so older config.json files still parse.
    #[serde(default)]
    #[allow(dead_code)]
    pub alias: String,
    /// When true (default), the engine starts without `-c` so llama.cpp uses
    /// the selected model's own context window (and --fit shrinks it to the
    /// available device memory). `ctx_size` is then only the fallback for
    /// models whose metadata can't be read. When false, `ctx_size` is the
    /// explicit, fixed context for every model.
    #[serde(default = "default_true")]
    pub auto_ctx: bool,
    pub ctx_size: u32,
    pub threads: u32,
    /// Bearer keys the engine requires on every request (llama.cpp --api-key;
    /// /health stays open). Generated at startup and persisted here; the UI
    /// shows them next to the base URL so other local apps can connect.
    /// Empty list = the engine runs open on localhost.
    #[serde(default, rename = "apiKeys")]
    pub api_keys: Vec<String>,
    /// Legacy single-key field, kept in sync with api_keys[0] so configs and
    /// tools written before multi-key support keep reading the primary key.
    #[serde(default, rename = "apiKey")]
    pub api_key: Option<String>,
    /// None = let llama.cpp auto-fit GPU offload to free VRAM (recommended:
    /// a manual value that exceeds VRAM crashes the engine).
    pub gpu_layers: Option<u32>,
    #[serde(default)]
    pub extra_args: Vec<String>,
    /// When true, thinking models keep their reasoning enabled (the
    /// --reasoning-budget 0 suppression is skipped) so reasoning_content
    /// streams to clients that render it. Off by default: external
    /// OpenAI-compatible apps that read only `content` then see empty
    /// replies while the model thinks. Takes effect on engine restart.
    #[serde(default, rename = "thinking")]
    pub thinking: bool,
    pub scan_dirs: Vec<PathBuf>,
    /// Filename substrings that mark a .gguf as NOT a chat model
    /// (vision adapters, VAEs, CLIP towers, LoRAs, ASR...).
    #[serde(default = "default_excludes")]
    pub exclude_patterns: Vec<String>,
    #[serde(default)]
    pub mmproj_rules: Vec<MmprojRule>,
    /// Tuned performance profiles keyed by model file stem
    /// (`open-harness tune` writes them; engine_args applies them).
    #[serde(default)]
    pub profiles: BTreeMap<String, PerfProfile>,
    /// OCR fallback: a second llama-server running a dedicated vision/OCR
    /// model (e.g. DeepSeek-OCR-2). When a chat request carries images and
    /// the loaded model has no vision projector, the images are transcribed
    /// here first so text-only models can still "see" them.
    #[serde(default, rename = "ocrFallback")]
    pub ocr_fallback: Option<OcrFallback>,
}

/// The OCR fallback sidecar's model pairing and port. Both files must exist
/// for the fallback to be offered (a half-downloaded model must not silently
/// eat image attachments).
#[derive(Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct OcrFallback {
    pub model: PathBuf,
    pub mmproj: PathBuf,
    #[serde(default = "default_ocr_port")]
    pub port: u16,
    #[serde(default = "default_ocr_enabled")]
    pub enabled: bool,
}

fn default_ocr_port() -> u16 {
    8799
}
fn default_ocr_enabled() -> bool {
    true
}

fn default_excludes() -> Vec<String> {
    vec![
        "mmproj".into(),
        "vae".into(),
        "clip".into(),
        "lora".into(),
        "adapter".into(),
        "whisper".into(),
        "t5xxl".into(),
    ]
}

/// A remote OpenAI-compatible chat API (DeepSeek, OpenRouter, OpenAI, ...).
/// The key lives in config.json on this PC: the app is local and private.
#[derive(Deserialize, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteApi {
    pub name: String,
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub model: String,
    /// Whether the profile's model accepts images. None (default) = unknown,
    /// images pass through and the remote API decides. Set false for a
    /// text-only model so the OCR fallback transcribes attachments first.
    #[serde(default)]
    pub vision: Option<bool>,
}

fn default_true() -> bool {
    true
}

/// One model entry inside a provider. `vision` only drives a UI badge:
/// whether images actually work is up to the provider and model.
#[derive(Deserialize, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderModel {
    pub id: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub vision: bool,
}

/// A cloud model provider configured on the Providers page. `format` selects
/// the wire protocol; every format is translated to the OpenAI SSE shape the
/// UI already speaks, so chat streaming is provider-agnostic client-side.
#[derive(Deserialize, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Provider {
    pub id: String,
    pub name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub base_url: String,
    /// "openai" | "anthropic" | "responses"
    #[serde(default)]
    pub format: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub models: Vec<ProviderModel>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    pub engine: EngineConfig,
    pub default_model: PathBuf,
    /// Max RAM the engine may use (GB). Models whose file is bigger are
    /// refused at load time, and the engine gets a hard OS working-set cap.
    /// None = unlimited. (serde's camelCase would rename this to
    /// "ramLimitGb"; the explicit rename keeps the config key "ramLimitGB".)
    #[serde(default, rename = "ramLimitGB")]
    pub ram_limit_gb: Option<f64>,
    /// Cloud/chat-API profiles the user can route chat through.
    #[serde(default, rename = "remoteApis")]
    pub remote_apis: Vec<RemoteApi>,
    /// Providers page entries (multi-model cloud providers with a wire
    /// format). Independent of remoteApis so old configs keep parsing.
    #[serde(default, rename = "providers")]
    pub providers: Vec<Provider>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Config, String> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        serde_json::from_str(&raw).map_err(|e| format!("bad {}: {e}", path.display()))
    }

    /// True when the engine is the AirLLM layer-streaming sidecar.
    pub fn is_airllm(&self) -> bool {
        self.engine.mode.eq_ignore_ascii_case("airllm")
    }

    /// The python interpreter that runs engine/airllm/server.py.
    pub fn airllm_python(&self) -> PathBuf {
        self.engine
            .airllm_python
            .clone()
            .unwrap_or_else(|| {
                crate::root_dir()
                    .join("engine")
                    .join("airllm")
                    .join(".venv")
                    .join("Scripts")
                    .join("python.exe")
            })
    }

    /// Base URL of the OpenAI-compatible engine endpoint.
    pub fn engine_base_url(&self) -> String {
        format!("http://{}:{}/v1", self.engine.host, self.engine.port)
    }

    /// Effective bearer-key list: engine.apiKeys, falling back to the legacy
    /// single engine.apiKey so older config.json files keep working.
    pub fn api_keys(&self) -> Vec<String> {
        let mut keys = self.engine.api_keys.clone();
        if keys.is_empty() {
            if let Some(k) = self.engine.api_key.as_deref().filter(|k| !k.trim().is_empty()) {
                keys.push(k.to_string());
            }
        }
        keys.retain(|k| !k.trim().is_empty());
        keys
    }

    /// The key internal callers (agent, chat proxy, dsh patch) authenticate
    /// with the first of the configured list, if any.
    pub fn primary_api_key(&self) -> Option<String> {
        self.api_keys().into_iter().next()
    }

    /// Error text when `model` exceeds the configured RAM limit, else None.
    /// One gate shared by the CLI (`reload_engine`) and the web API. A GGUF
    /// model is one file; an AirLLM HF checkpoint is a folder, so its whole
    /// size counts.
    pub fn ram_limit_violation(&self, model: &Path) -> Option<String> {
        let limit = self.ram_limit_gb?;
        let size = model_size(model);
        (size as f64 > limit * 1e9).then(|| {
            format!(
                "{} is {:.1} GB, over the {:.0} GB RAM limit. Pick a smaller model in the Models panel (or `open-harness models`).",
                Self::alias_for(model),
                size as f64 / 1e9,
                limit
            )
        })
    }

    /// The llama-server alias and dsh model id for a model file.
    pub fn alias_for(model: &Path) -> String {
        model
            .file_stem()
            .map(|s| s.to_string_lossy().replace(' ', "_"))
            .unwrap_or_else(|| "model".into())
    }

    /// The tuned profile for this model file, if one was saved.
    pub fn profile_for(&self, model: &Path) -> Option<&PerfProfile> {
        let stem = model.file_stem()?.to_string_lossy();
        self.engine.profiles.get(stem.as_ref())
    }

    /// Context window to serve for `model`: its own metadata when autoCtx is
    /// on (GGUF `*.context_length` / HF `max_position_embeddings`), else the
    /// fixed `ctxSize` from Settings.
    pub fn effective_ctx(&self, model: &Path) -> u32 {
        if !self.engine.auto_ctx {
            return self.engine.ctx_size;
        }
        model_native_ctx(model).unwrap_or(self.engine.ctx_size)
    }

    /// Resolve a model: explicit path, or partial filename matched against the
    /// configured scan directories (recursive, excluding non-chat models).
    /// AirLLM mode matches HF checkpoint folders; llama.cpp mode matches .gguf.
    pub fn resolve_model(&self, needle: Option<&str>) -> Result<PathBuf, String> {
        let Some(needle) = needle else {
            return Ok(self.default_model.clone());
        };
        if Path::new(needle).is_file() || Path::new(needle).is_dir() {
            return Ok(PathBuf::from(needle));
        }
        let lower = needle.to_lowercase();
        for dir in &self.engine.scan_dirs {
            let mut candidates: Vec<PathBuf> = Vec::new();
            self.collect_models(dir, &mut candidates);
            if let Some(hit) = candidates.into_iter().find(|p| {
                p.file_name().is_some_and(|f| f.to_string_lossy().to_lowercase().contains(&lower))
            }) {
                return Ok(hit);
            }
        }
        let kind = if self.is_airllm() { "HF model folder" } else { ".gguf" };
        Err(format!("no {kind} matching '{needle}' in {:?}", self.engine.scan_dirs))
    }

    /// Every loadable chat model found in the scan dirs.
    pub fn discover_models(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for dir in &self.engine.scan_dirs {
            self.collect_models(dir, &mut out);
        }
        out.sort();
        out.dedup();
        out
    }

    /// Discover one mode's model kind under `dir`.
    fn collect_models(&self, dir: &Path, out: &mut Vec<PathBuf>) {
        if self.is_airllm() {
            collect_hf_models(dir, out)
        } else {
            collect_ggufs(dir, &self.engine.exclude_patterns, out)
        }
    }
}

fn is_excluded(name: &str, patterns: &[String]) -> bool {
    let lower = name.to_lowercase();
    patterns.iter().any(|p| lower.contains(p))
}

/// Size of a model on disk: a GGUF is one file, an AirLLM HF checkpoint is a
/// folder: count the whole top level plus one level of subfolders.
pub fn model_size(model: &Path) -> u64 {
    if model.is_dir() {
        dir_size(model)
    } else {
        std::fs::metadata(model).map(|m| m.len()).unwrap_or(0)
    }
}

/// Total size of every regular file under `dir` (two levels deep; HF
/// checkpoints keep weights at the top, subfolders only hold tokenizers).
fn dir_size(dir: &Path) -> u64 {
    let mut total = 0u64;
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_file() {
            total += entry.metadata().map(|m| m.len()).unwrap_or(0);
        } else if p.is_dir() {
            if let Ok(sub) = std::fs::read_dir(&p) {
                for e in sub.flatten() {
                    if e.path().is_file() {
                        total += e.metadata().map(|m| m.len()).unwrap_or(0);
                    }
                }
            }
        }
    }
    total
}

/// HF checkpoint folders usable by AirLLM: a directory holding `config.json`
/// plus at least one weight file. Depth-capped like the GGUF walker.
fn collect_hf_models(dir: &Path, out: &mut Vec<PathBuf>) {
    collect_hf_depth(dir, out, 0);
}

fn collect_hf_depth(dir: &Path, out: &mut Vec<PathBuf>, depth: u32) {
    if depth > 5 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let p = entry.path();
        if !p.is_dir() {
            continue;
        }
        let is_model = p.join("config.json").is_file()
            && (glob_has_weights(&p) || p.join("model.safetensors.index.json").is_file());
        if is_model {
            out.push(p);
        } else {
            collect_hf_depth(&p, out, depth + 1);
        }
    }
}

/// Cheap "does this folder look like it holds weights" check: the big
/// safetensors/bin files usually sit beside config.json.
fn glob_has_weights(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else { return false };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_lowercase();
        if (name.ends_with(".safetensors") || name.ends_with(".bin")) && !name.contains("adapter") {
            return true;
        }
    }
    false
}

fn collect_ggufs(dir: &Path, patterns: &[String], out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_file() {
            let Some(name) = p.file_name().map(|f| f.to_string_lossy().to_string()) else { continue };
            if name.to_lowercase().ends_with(".gguf") && !is_excluded(&name, patterns) {
                out.push(p);
            }
        } else if p.is_dir() {
            collect_ggufs_depth(&p, patterns, out, 0);
        }
    }
}

fn collect_ggufs_depth(dir: &Path, patterns: &[String], out: &mut Vec<PathBuf>, depth: u32) {
    // Depth cap guards against junction/symlink loops.
    if depth > 8 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_file() {
            let Some(name) = p.file_name().map(|f| f.to_string_lossy().to_string()) else { continue };
            if name.to_lowercase().ends_with(".gguf") && !is_excluded(&name, patterns) {
                out.push(p);
            }
        } else if p.is_dir() {
            collect_ggufs_depth(&p, patterns, out, depth + 1);
        }
    }
}

/// The context window a model was trained for, read from its own metadata:
/// a GGUF's `*.context_length`, or an HF checkpoint folder's
/// `max_position_embeddings`. None when the file/folder doesn't say.
pub fn model_native_ctx(model: &Path) -> Option<u32> {
    if model.is_dir() {
        hf_native_ctx(model)
    } else {
        let mut r = BufReader::new(std::fs::File::open(model).ok()?);
        read_gguf_ctx(&mut r).ok().flatten()
    }
}

fn hf_native_ctx(dir: &Path) -> Option<u32> {
    let raw = std::fs::read_to_string(dir.join("config.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    // Multimodal configs keep the language tower's window under text_config.
    let ctx = find_ctx_key(&v).or_else(|| v.get("text_config").and_then(find_ctx_key))?;
    u32::try_from(ctx).ok().filter(|n| *n > 0)
}

fn find_ctx_key(v: &serde_json::Value) -> Option<u64> {
    ["max_position_embeddings", "n_positions", "seq_length", "model_max_length"]
        .iter()
        .find_map(|k| v.get(*k).and_then(|x| x.as_u64()))
        .filter(|n| *n > 0)
}

/// Scan only the metadata block of a GGUF (before the tensor data) for the
/// architecture's context length. Strings and arrays are seeked/skipped, so
/// a multi-GB model costs a few small reads, and the scan gives up after
/// 16 MB instead of walking a giant tokenizer table.
fn read_gguf_ctx<R: Read + Seek>(r: &mut R) -> std::io::Result<Option<u32>> {
    let mut magic = [0u8; 4];
    r.read_exact(&mut magic)?;
    if &magic != b"GGUF" {
        return Ok(None);
    }
    let version = read_u32(r)?;
    if !(1..=3).contains(&version) {
        return Ok(None);
    }
    let _tensor_count = read_u64(r)?;
    let kv_count = read_u64(r)?;
    for _ in 0..kv_count.min(4096) {
        if r.stream_position()? > 16 * 1024 * 1024 {
            return Ok(None);
        }
        let key = read_gguf_string(r, version)?;
        let ty = read_u32(r)?;
        let value = read_gguf_value(r, ty, version, 0)?;
        if key.ends_with("context_length") {
            let ctx = value.and_then(|n| u32::try_from(n).ok()).filter(|n| *n > 0);
            return Ok(ctx);
        }
    }
    Ok(None)
}

fn read_gguf_value<R: Read + Seek>(
    r: &mut R,
    ty: u32,
    version: u32,
    depth: u32,
) -> std::io::Result<Option<u64>> {
    if depth > 4 {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "GGUF array too deep"));
    }
    Ok(match ty {
        0 => Some(read_arr::<1, _>(r)?[0] as u64),
        1 => Some(read_arr::<1, _>(r)?[0] as i8 as i64 as u64),
        2 => Some(u16::from_le_bytes(read_arr(r)?) as u64),
        3 => Some(i16::from_le_bytes(read_arr(r)?) as i64 as u64),
        4 => Some(u32::from_le_bytes(read_arr(r)?) as u64),
        5 => Some(i32::from_le_bytes(read_arr(r)?) as i64 as u64),
        6 => {
            r.seek(SeekFrom::Current(4))?;
            None
        }
        7 => {
            r.seek(SeekFrom::Current(1))?;
            None
        }
        8 => {
            let n = read_gguf_len(r, version)?;
            r.seek(SeekFrom::Current(i64::try_from(n).unwrap_or(i64::MAX)))?;
            None
        }
        9 => {
            let elem_ty = read_u32(r)?;
            let n = read_gguf_len(r, version)?;
            if n > 10_000_000 {
                return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "GGUF array too long"));
            }
            for _ in 0..n {
                read_gguf_value(r, elem_ty, version, depth + 1)?;
            }
            None
        }
        10 => Some(u64::from_le_bytes(read_arr(r)?)),
        11 => Some(i64::from_le_bytes(read_arr(r)?) as u64),
        12 => {
            r.seek(SeekFrom::Current(8))?;
            None
        }
        _ => return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "unknown GGUF value type")),
    })
}

fn read_gguf_string<R: Read>(r: &mut R, version: u32) -> std::io::Result<String> {
    let n = read_gguf_len(r, version)?;
    if n > 1 << 20 {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "GGUF key too long"));
    }
    let mut buf = vec![0u8; n as usize];
    r.read_exact(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

fn read_gguf_len<R: Read>(r: &mut R, version: u32) -> std::io::Result<u64> {
    if version >= 2 {
        read_u64(r)
    } else {
        Ok(read_u32(r)? as u64)
    }
}

fn read_u32<R: Read>(r: &mut R) -> std::io::Result<u32> {
    Ok(u32::from_le_bytes(read_arr(r)?))
}

fn read_u64<R: Read>(r: &mut R) -> std::io::Result<u64> {
    Ok(u64::from_le_bytes(read_arr(r)?))
}

fn read_arr<const N: usize, R: Read>(r: &mut R) -> std::io::Result<[u8; N]> {
    let mut buf = [0u8; N];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn push_str(out: &mut Vec<u8>, s: &str) {
        out.extend_from_slice(&(s.len() as u64).to_le_bytes());
        out.extend_from_slice(s.as_bytes());
    }

    #[test]
    fn reads_gguf_context_length_past_string_and_array_values() {
        let mut b = Vec::new();
        b.extend_from_slice(b"GGUF");
        b.extend_from_slice(&3u32.to_le_bytes()); // version
        b.extend_from_slice(&0u64.to_le_bytes()); // tensor count
        b.extend_from_slice(&3u64.to_le_bytes()); // kv count
        push_str(&mut b, "general.architecture");
        b.extend_from_slice(&8u32.to_le_bytes()); // string
        push_str(&mut b, "llama");
        push_str(&mut b, "tokenizer.ggml.tokens");
        b.extend_from_slice(&9u32.to_le_bytes()); // array
        b.extend_from_slice(&8u32.to_le_bytes()); // of strings
        b.extend_from_slice(&2u64.to_le_bytes());
        push_str(&mut b, "a");
        push_str(&mut b, "bc");
        push_str(&mut b, "llama.context_length");
        b.extend_from_slice(&4u32.to_le_bytes()); // uint32
        b.extend_from_slice(&262_144u32.to_le_bytes());

        let got = read_gguf_ctx(&mut Cursor::new(b)).unwrap();
        assert_eq!(got, Some(262_144));
    }

    #[test]
    fn ignores_non_gguf_files() {
        assert_eq!(read_gguf_ctx(&mut Cursor::new(b"nope".to_vec())).unwrap(), None);
    }
}
