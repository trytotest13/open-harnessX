//! Writes the dsh profile patch that wires dsh (the opt-in agent IDE) to the
//! local engine.
//! Mirrors bin/configure-ide.js semantics; see docs/user/guide/providers.md
//! in the dsh repo: a patch replaces the targeted row's whole config, so
//! each row restates every key it owns.

use crate::config::Config;
use std::path::PathBuf;

pub const DSH_PROVIDER_ID: &str = "local-engine";
pub const DSH_PACKAGE: &str = "@deepseek-ai/dsh@0.2.0-rc.2";

/// Rows this tool owns; everything else in the patch file (dsh's own writes,
/// user rows) is preserved verbatim across rewrites.
const MANAGED_IDS: [&str; 5] = [
    "llm-pi-ai",
    "agent-default-model",
    "llm-deepseek",
    "llm-deepseek-account",
    "deepseek-account",
];

pub fn dsh_home() -> PathBuf {
    if let Ok(home) = std::env::var("DSH_HOME") {
        return PathBuf::from(home);
    }
    let user = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_default();
    PathBuf::from(user).join(".dsh")
}

pub fn patch_path() -> PathBuf {
    dsh_home().join("profiles").join("web").join("cordis.patch.yml")
}

pub fn write_patch(cfg: &Config, model: &std::path::Path) -> Result<PathBuf, String> {
    let alias = Config::alias_for(model);
    let path = patch_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    // Preserve any rows we don't manage (the dsh UI and the user may write
    // their own into this file).
    let foreign = std::fs::read_to_string(&path)
        .map(|text| preserve_foreign_rows(&text))
        .unwrap_or_default();
    if !foreign.is_empty() {
        println!("[oh] preserving existing non-managed rows in {}", path.display());
    }

    let ctx = crate::engine_ctx_for(cfg, model);
    let yaml = format!(
        "# Managed by open-harness.exe: wires dsh to the local llama.cpp server.\n\
         # Model changes take effect on the next request; no restart needed.\n\
         # Non-managed rows below are preserved from previous edits.\n\
         {managed}{foreign}",
        managed = managed_rows(cfg, &alias, ctx),
        foreign = foreign,
    );
    std::fs::write(&path, yaml).map_err(|e| e.to_string())?;
    Ok(path)
}

fn managed_rows(cfg: &Config, alias: &str, ctx: u32) -> String {
    // The engine rejects unauthenticated requests when an API key is set, so
    // the provider row has to carry it (ensure_api_key guarantees it exists).
    let auth = cfg
        .primary_api_key()
        .map(|k| format!("\x20       apiKey: {k}\n"))
        .unwrap_or_default();
    format!(
        "- id: llm-pi-ai\n\
         \x20 config:\n\
         \x20   providers:\n\
         \x20     {DSH_PROVIDER_ID}:\n\
         \x20       api: openai-completions\n\
         \x20       baseURL: {base}\n\
         {auth}\
         \x20       compat:\n\
         \x20         supportsDeveloperRole: false\n\
         \x20         maxTokensField: max_tokens\n\
         \x20       models:\n\
         \x20         - id: {alias}\n\
         \x20           contextWindow: {ctx}\n\
         \x20           maxTokens: {max}\n\
         \n\
         - id: agent-default-model\n\
         \x20 config:\n\
         \x20   provider: {DSH_PROVIDER_ID}\n\
         \x20   model: {alias}\n\
         \n\
         # The cloud DeepSeek routes are mounted by dsh's base bundle but are\n\
         # unusable here without an API key: hide them from the model picker.\n\
         - id: llm-deepseek\n\
         \x20 disabled: true\n\
         - id: llm-deepseek-account\n\
         \x20 disabled: true\n\
         - id: deepseek-account\n\
         \x20 disabled: true\n\
         \n",
        base = cfg.engine_base_url(),
        max = ctx / 2,
    )
}

/// Split a patch file into top-level `- id: <x>` blocks and return the ones
/// whose id is not managed by this tool (header comments are dropped).
/// Only column-0 "- id:" lines start a block: nested list entries (e.g. a
/// model row under `models:`) are indented and stay inside their block.
fn preserve_foreign_rows(text: &str) -> String {
    let mut blocks: Vec<Vec<&str>> = Vec::new();
    for line in text.lines() {
        if line.starts_with("- id:") {
            blocks.push(vec![line]);
        } else if !blocks.is_empty() && !line.trim().is_empty() {
            blocks.last_mut().unwrap().push(line);
        }
        // skip header comment lines before the first block
    }
    let mut out = String::new();
    for block in &blocks {
        let id = block[0]
            .split("id:")
            .nth(1)
            .map(|s| s.trim().trim_end_matches(':').trim())
            .unwrap_or("");
        if MANAGED_IDS.contains(&id) {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        for l in block {
            out.push_str(l);
            out.push('\n');
        }
    }
    if out.is_empty() {
        String::new()
    } else {
        format!("\n# ── preserved rows (not managed by deepseek-ide.exe) ──\n{out}")
    }
}

pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
