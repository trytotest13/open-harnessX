//! Runtime state file (.engine-state.json). Tolerates the old JS-manager
//! schema, which stored the engine pid under the key "pid".

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Default)]
pub struct State {
    #[serde(default, alias = "pid")]
    pub engine_pid: Option<u32>,
    #[serde(default)]
    pub dsh_pid: Option<u32>,
    /// The OCR fallback sidecar (llama-server with a vision/OCR model) once
    /// spawned, so `stop` takes it down too.
    #[serde(default)]
    pub ocr_pid: Option<u32>,
    /// The supervisor process itself when it hosts the web UI (`serve`/`start`)
    /// so `stop` can end it too.
    #[serde(default)]
    pub supervisor_pid: Option<u32>,
    #[serde(default)]
    pub model: String,
    /// Context window the running engine actually loaded (tokens), read from
    /// /props after the model becomes healthy. None for engines started by
    /// older builds or when the engine didn't report it.
    #[serde(default, rename = "nCtx")]
    pub n_ctx: Option<u32>,
    /// Last model confirmed healthy: the rollback target when a new load
    /// fails (the "model" field may name a model that failed to load).
    #[serde(default)]
    pub last_healthy: Option<String>,
    #[serde(default)]
    pub started_at: Option<u64>,
}

pub fn load(path: &std::path::Path) -> Option<State> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

pub fn save(path: &std::path::Path, state: &State) {
    if let Ok(json) = serde_json::to_string_pretty(state) {
        let _ = std::fs::write(path, json);
    }
}
