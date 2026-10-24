//! config.json schema for the IDE stack.

use serde::Deserialize;
use std::collections::BTreeMap;
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
    pub ctx_size: u32,
    pub threads: u32,
    /// None = let llama.cpp auto-fit GPU offload to free VRAM (recommended:
    /// a manual value that exceeds VRAM crashes the engine).
    pub gpu_layers: Option<u32>,
    #[serde(default)]
    pub extra_args: Vec<String>,
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
/// The key lives in config.json on this PC — the app is local and private.
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

/// One model entry inside a provider. `vision` only drives a UI badge —
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
/// folder — count the whole top level plus one level of subfolders.
pub fn model_size(model: &Path) -> u64 {
    if model.is_dir() {
        dir_size(model)
    } else {
        std::fs::metadata(model).map(|m| m.len()).unwrap_or(0)
    }
}

/// Total size of every regular file under `dir` (two levels deep — HF
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
