//! open-harness.exe — launcher/supervisor for the Open Harness stack.
//!
//! Systems layer (Rust): starts/stops/waits on the llama.cpp engine, hot-swaps
//! models, writes the dsh provider patch, serves the Open Harness web UI
//! (server.rs) and opens it in a native desktop window. Third-party binaries
//! (llama-server, dsh via npx) are spawned as detached children and survive
//! this process exiting.

mod agent;
mod config;
mod gui;
mod net;
mod ocr;
mod patch;
mod proc;
mod providers;
mod server;
mod state;
mod store;
mod telemetry;

use config::Config;
use net::{http_status, pid_listening_on, port_open};
use patch::{unix_now, DSH_PACKAGE};
use proc::{kill_tree, spawn_logged};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// The dsh agent IDE (opt-in via `start` / its log URL) listens here; the
/// Open Harness UI owns 3080.
const DSH_PORT: u16 = 3081;
const ENGINE_HEALTH_TIMEOUT: Duration = Duration::from_millis(1500);
const START_WAIT_SECS: u64 = 900;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        // no arguments = double-click: behave like a normal desktop program
        None => cmd_app(None),
        Some("app") => cmd_app(args.get(1).map(String::as_str)),
        Some("start") => cmd_start(args.get(1).map(String::as_str)),
        Some("serve") => cmd_serve(),
        Some("load") => match args.get(1) {
            Some(n) => cmd_load(n),
            None => arg_missing("load <partial-model-name>"),
        },
        Some("scan") => match args.get(1) {
            Some(d) => cmd_scan(d),
            None => arg_missing("scan <folder-to-index>"),
        },
        Some("stop") => cmd_stop(),
        Some("status") => cmd_status(),
        Some("models" | "list") => cmd_models(),
        Some("doctor") => cmd_doctor(args.iter().any(|s| s == "--json")),
        Some("tune") => {
            let needle = args
                .iter()
                .skip(1)
                .find(|s| !s.starts_with("--"))
                .map(String::as_str);
            cmd_tune(needle, args.iter().any(|s| s == "--quick"))
        }
        Some("wait") => cmd_wait(args.get(1).and_then(|s| s.parse().ok()).unwrap_or(START_WAIT_SECS)),
        Some(_) => {
            eprintln!(
                "Open Harness — local AI supervisor\n\n\
                 USAGE:\n\
                 \x20 open-harness app [model]    start stack, open the Open Harness window\n\
                 \x20 open-harness serve          headless: stack + web UI (blocks)\n\
                 \x20 open-harness start [model]  headless: also boot the dsh agent IDE\n\
                 \x20 open-harness load <name>    hot-swap the served model (partial .gguf name)\n\
                 \x20 open-harness scan <folder>  index a folder anywhere on disk (persisted)\n\
                 \x20 open-harness models         list every loadable model found\n\
                 \x20 open-harness doctor          hardware + stack health report (--json)\n\
                 \x20 open-harness tune [model]    measure the fastest flags for a model (--quick)\n\
                 \x20 open-harness stop           stop engine + UIs\n\
                 \x20 open-harness status         stack status (exit 0 = engine healthy)\n\
                 \x20 open-harness wait [secs]    block until engine healthy"
            );
            2
        }
    };
    std::process::exit(code);
}

fn arg_missing(usage: &str) -> i32 {
    eprintln!("[oh] missing argument — usage: open-harness {usage}");
    2
}

pub(crate) fn root_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

pub(crate) fn config_path() -> PathBuf {
    let root = root_dir();
    let c = root.join("config.json");
    if c.is_file() {
        return c;
    }
    PathBuf::from("config.json")
}

pub(crate) fn load_config() -> Result<Config, String> {
    Config::load(&config_path())
}

/// Optional voice/behavior prompt the user maintains in system-prompt.md next
/// to config.json; joined into every system message when present.
pub(crate) fn system_prompt_md() -> Option<String> {
    std::fs::read_to_string(root_dir().join("system-prompt.md"))
        .ok()
        .filter(|s| !s.trim().is_empty())
}

pub(crate) fn state_path() -> PathBuf {
    root_dir().join(".engine-state.json")
}

pub(crate) fn loaded_model() -> Option<PathBuf> {
    state::load(&state_path()).map(|s| PathBuf::from(s.model))
}

fn engine_args(cfg: &Config, model: &Path) -> Vec<String> {
    if cfg.is_airllm() {
        return airllm_args(cfg, model);
    }
    let e = &cfg.engine;
    // A tuned profile (open-harness tune) was measured on this machine for
    // this exact file, so it wins over the global defaults where present.
    let profile = cfg.profile_for(model);
    let threads = profile.and_then(|p| p.threads).unwrap_or(e.threads);
    let mut args = vec![
        "-m".into(),
        model.display().to_string(),
        "--host".into(),
        e.host.clone(),
        "--port".into(),
        e.port.to_string(),
        "-c".into(),
        e.ctx_size.to_string(),
        "-t".into(),
        threads.to_string(),
        "--alias".into(),
        Config::alias_for(model),
    ];
    if let Some(ngl) = profile.and_then(|p| p.gpu_layers).or(e.gpu_layers) {
        args.push("-ngl".into());
        args.push(ngl.to_string());
    }
    if let Some(p) = profile {
        // The engine auto-fits GPU offload to free VRAM; the profile's margin
        // (MiB kept free per GPU for KV cache and buffers) tunes that fit.
        if let Some(mib) = p.fit_target_mib {
            args.push("--fit-target".into());
            args.push(mib.to_string());
        }
        if let Some(fa) = p.flash_attn.as_deref() {
            args.push("-fa".into());
            args.push(fa.into());
        }
    }
    args.extend(e.extra_args.iter().cloned());
    // Vision adapters only pair with their own model family (a mismatched
    // mmproj crashes the engine with an n_embd error).
    for rule in &e.mmproj_rules {
        if rule.path.is_file()
            && model
                .file_name()
                .is_some_and(|f| f.to_string_lossy().to_lowercase().contains(&rule.model_pattern.to_lowercase()))
        {
            args.push("--mmproj".into());
            args.push(rule.path.display().to_string());
        }
    }
    args
}

/// Command-line for engine/airllm/server.py. The sidecar serves /health,
/// /props and /v1/chat/completions on the same port llama-server uses, so
/// everything downstream of the spawn is mode-agnostic.
fn airllm_args(cfg: &Config, model: &Path) -> Vec<String> {
    let e = &cfg.engine;
    let server_py = root_dir().join("engine").join("airllm").join("server.py");
    let mut args = vec![
        server_py.display().to_string(),
        "--model".into(),
        model.display().to_string(),
        "--host".into(),
        e.host.clone(),
        "--port".into(),
        e.port.to_string(),
        "--max-seq-len".into(),
        e.airllm_max_seq_len.unwrap_or(e.ctx_size).to_string(),
    ];
    if let Some(comp) = e.airllm_compression.as_deref() {
        args.push("--compression".into());
        args.push(comp.to_string());
    }
    args
}

pub(crate) fn engine_healthy(cfg: &Config) -> bool {    http_status(&cfg.engine.host, cfg.engine.port, "/health", ENGINE_HEALTH_TIMEOUT) == Some(200)
}

pub(crate) fn same_path(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase(),
    }
}

fn spawn_engine(cfg: &Config, model: &Path) -> std::io::Result<u32> {
    let args = engine_args(cfg, model);
    let (exe, workdir) = if cfg.is_airllm() {
        let py = cfg.airllm_python();
        if !py.is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!(
                    "airllm venv python not found: {} — run engine\\airllm\\setup-airllm.bat once",
                    py.display()
                ),
            ));
        }
        (py, root_dir().join("engine").join("airllm"))
    } else {
        let wd = cfg
            .engine
            .exe
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        (cfg.engine.exe.clone(), wd)
    };
    println!("[oh] starting engine: {}", exe.display());
    println!("[oh]   model: {}", model.display());
    println!("[oh]   args:  {}", args.join(" "));
    let child = spawn_logged(
        Command::new(&exe).args(&args).current_dir(workdir),
        &root_dir().join("engine.log"),
    )?;
    let pid = child.id();
    let mut st = state::load(&state_path()).unwrap_or_default();
    st.engine_pid = Some(pid);
    st.model = model.display().to_string();
    st.started_at = Some(unix_now());
    state::save(&state_path(), &st);
    println!("[oh]   pid {pid}");
    if let Some(limit) = cfg.ram_limit_gb {
        let bytes = (limit * 1024.0 * 1024.0 * 1024.0) as usize;
        if proc::limit_working_set(pid, bytes) {
            println!("[oh]   RAM cap {limit:.0} GB applied");
        } else {
            eprintln!("[oh]   WARNING: could not apply the RAM cap (engine runs uncapped)");
        }
    }
    Ok(pid)
}

/// Make the engine serve `model`: no-op when it already does, otherwise stop
/// the current engine and start the new one. `wait_secs` = Some → block until
/// healthy; None → return immediately (model loads in the background).
fn reload_engine(cfg: &Config, model: &Path, wait_secs: Option<u64>) -> Result<bool, String> {
    let already = engine_healthy(cfg)
        && loaded_model().is_some_and(|m| same_path(&m, model));
    if already {
        println!("[oh] engine already serving {}", model.display());
        return Ok(false);
    }
    if let Some(err) = cfg.ram_limit_violation(model) {
        return Err(err);
    }
    // Always drain, not only when the health probe answers: a RAM-starved
    // engine can miss the 1.5 s probe while very much alive, and spawning on
    // top of it is how two llama-servers end up sharing the port.
    if let Some(pid) = state::load(&state_path()).and_then(|s| s.engine_pid) {
        println!("[oh] stopping engine pid {pid} (model swap)...");
        let _ = kill_tree(pid);
    }
    // Drain EVERY listener on the engine port before spawning. The state
    // pid can be stale while an orphaned engine still holds the port, and
    // llama-server's SO_REUSEADDR lets two engines share it silently —
    // requests then land on a random one of the two (wrong model, phantom
    // vision flags, mid-stream resets). Wait for the port to actually
    // empty instead of trusting a /health probe a second engine can keep
    // answering.
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        match pid_listening_on(cfg.engine.port) {
            Some(pid) => {
                println!("[oh] engine port {} still held by pid {pid} — stopping it", cfg.engine.port);
                let _ = kill_tree(pid);
                std::thread::sleep(Duration::from_millis(400));
            }
            None => break,
        }
    }
    while Instant::now() < deadline && port_open(cfg.engine.host.as_str(), cfg.engine.port) {
        std::thread::sleep(Duration::from_millis(200));
    }
    spawn_engine(cfg, model).map_err(|e| format!("failed to start engine: {e}"))?;
    if let Some(secs) = wait_secs {
        println!("[oh] waiting for model to load (first load pages weights from disk)...");
        if !wait_healthy(cfg, secs) {
            return Err("engine failed to load — see the error lines above / engine.log".into());
        }
        // Bookkeep the last model that was confirmed serving.
        let mut st = state::load(&state_path()).unwrap_or_default();
        st.last_healthy = Some(model.display().to_string());
        state::save(&state_path(), &st);
    }
    Ok(true)
}

/// The model that should be served: an explicit needle wins, otherwise keep
/// whatever is already loaded, else the configured default — falling back to
/// the largest model that fits the RAM limit so the app still boots.
fn desired_model(cfg: &Config, needle: Option<&str>) -> Result<PathBuf, String> {
    let Some(n) = needle else {
        let model = loaded_model().unwrap_or_else(|| cfg.default_model.clone());
        if let Some(limit) = cfg.ram_limit_gb {
            let size = std::fs::metadata(&model).map(|s| s.len()).unwrap_or(0);
            if size as f64 > limit * 1e9 {
                let fits = cfg
                    .discover_models()
                    .into_iter()
                    .filter(|p| {
                        std::fs::metadata(p).map(|s| s.len()).unwrap_or(0) as f64 <= limit * 1e9
                    })
                    .max_by_key(|p| std::fs::metadata(p).map(|s| s.len()).unwrap_or(0));
                if let Some(p) = fits {
                    println!(
                        "[oh] default model is over the {limit:.0} GB RAM limit — using {} instead (change it in Settings)",
                        Config::alias_for(&p)
                    );
                    return Ok(p);
                }
            }
        }
        return Ok(model);
    };
    cfg.resolve_model(Some(n))
}

fn spawn_dsh() -> std::io::Result<u32> {
    println!("[oh] starting agent IDE ({DSH_PACKAGE}) on port {DSH_PORT}...");
    let child = spawn_logged(
        Command::new("cmd")
            .args(["/C", "npx", "-y", DSH_PACKAGE, "web", "--no-open", "--port", &DSH_PORT.to_string()])
            .current_dir(root_dir()),
        &root_dir().join("dsh-ide.log"),
    )?;
    let pid = child.id();
    let mut st = state::load(&state_path()).unwrap_or_default();
    st.dsh_pid = Some(pid);
    state::save(&state_path(), &st);
    println!("[oh]   pid {pid}");
    Ok(pid)
}

/// Single-flight guard for agent IDE boots: the desktop app pre-starts dsh in
/// the background, and the UI button can fire while that boot is in flight —
/// a second competing spawn would just lose the port race and hang.
static DSH_BUSY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Make sure the agent IDE is serving and we hold its authenticated URL.
///
/// dsh binds its port early but prints the URL line only after its plugin
/// tree settles (~1 min warm, longer on the first npx download or while the
/// engine is also loading a model). So: if the port is already open, WAIT for
/// the URL instead of restarting — killing a booting dsh resets the whole
/// startup and is exactly what made the IDE feel impossible to open.
pub(crate) fn ensure_dsh_url() -> Result<String, String> {
    use std::sync::atomic::Ordering;
    let log = root_dir().join("dsh-ide.log");
    if DSH_BUSY
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        // A pre-start or another request is already booting the IDE — wait
        // for its URL instead of spawning a second dsh.
        return match wait_dsh_url(&log, 300) {
            Some(url) => Ok(url),
            None => Err("agent IDE is still starting — try again in a moment".into()),
        };
    }
    let result = ensure_dsh_url_inner(&log);
    DSH_BUSY.store(false, Ordering::SeqCst);
    result
}

fn ensure_dsh_url_inner(log: &Path) -> Result<String, String> {
    if port_open("127.0.0.1", DSH_PORT) {
        // The log is truncated on every spawn, so any URL in it belongs to
        // the most recent dsh boot — the process now listening.
        if let Some(url) = dsh_url_from_log(&log) {
            println!("[oh] IDE already listening on port {DSH_PORT}");
            return Ok(url);
        }
        println!("[oh] IDE port is open but its auth URL is not printed yet; waiting...");
        if let Some(url) = wait_dsh_url(&log, 90) {
            return Ok(url);
        }
        println!("[oh] IDE is running but its auth URL never appeared — restarting it");
        if let Some(pid) = state::load(&state_path())
            .and_then(|s| s.dsh_pid)
            .or_else(|| pid_listening_on(DSH_PORT))
        {
            let _ = kill_tree(pid);
            std::thread::sleep(Duration::from_millis(1500));
        }
    }
    if let Err(e) = spawn_dsh() {
        return Err(format!("failed to start IDE UI: {e}"));
    }
    match wait_dsh_url(&log, 280) {
        Some(url) => Ok(url),
        None => Err(
            "agent IDE did not print its auth URL within 280s (see dsh-ide.log; \
             the first-ever npx download or a busy disk can make this slow — try again)"
                .into(),
        ),
    }
}

/// Poll `log` for the authenticated dsh URL, up to `secs`.
fn wait_dsh_url(log: &Path, secs: u64) -> Option<String> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if let Some(url) = dsh_url_from_log(log) {
            return Some(url);
        }
        std::thread::sleep(Duration::from_millis(1000));
    }
    None
}

/// Extract the authenticated dsh URL from its log output.
fn dsh_url_from_log(log: &Path) -> Option<String> {
    let text = std::fs::read_to_string(log).ok()?;
    let line = text
        .lines()
        .find(|l| l.contains("http://") && l.contains("token="))?;
    let start = line.find("http://")?;
    let end = line[start..]
        .find(|c: char| c.is_whitespace())
        .map(|i| start + i)
        .unwrap_or(line.len());
    Some(line[start..end].to_string())
}

pub(crate) fn pid_alive(pid: u32) -> bool {
    let Ok(out) = Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .output()
    else {
        return true; // can't tell — assume alive and keep polling
    };
    String::from_utf8_lossy(&out.stdout).contains(&pid.to_string())
}

/// Last error lines from the engine log, for fast-fail diagnostics.
fn engine_log_errors() -> Option<String> {
    let text = std::fs::read_to_string(root_dir().join("engine.log")).ok()?;
    let errs: Vec<&str> = text
        .lines()
        .filter(|l| l.contains(" E ") || l.to_lowercase().contains("error"))
        .collect();
    if errs.is_empty() {
        return None;
    }
    Some(errs.iter().rev().take(3).rev().map(|l| format!("  {l}\n")).collect())
}

fn wait_healthy(cfg: &Config, secs: u64) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if engine_healthy(cfg) {
            return true;
        }
        // Fast-fail: a dead engine process will never become healthy, and a
        // broken .gguf makes llama-server exit within seconds. Surface the
        // log's error lines instead of polling out the whole timeout.
        if let Some(pid) = state::load(&state_path()).and_then(|s| s.engine_pid) {
            if !pid_alive(pid) {
                eprintln!("[oh] engine process {pid} exited during load. Last errors:");
                match engine_log_errors() {
                    Some(errs) => eprintln!("{errs}"),
                    None => eprintln!("  (no error lines in engine.log)"),
                }
                return false;
            }
        }
        std::thread::sleep(Duration::from_millis(1500));
    }
    engine_healthy(cfg)
}

fn cmd_start(needle: Option<&str>) -> i32 {
    let cfg = match load_config() {
        Ok(c) => c,
        Err(e) => return fail(&e),
    };
    let model = match desired_model(&cfg, needle) {
        Ok(m) => m,
        Err(e) => return fail(&e),
    };
    if let Err(e) = reload_engine(&cfg, &model, Some(START_WAIT_SECS)) {
        return fail(&e);
    }
    if let Err(e) = patch::write_patch(&cfg, &Config::alias_for(&model)) {
        eprintln!("[oh] WARNING: could not write dsh config: {e}");
    }
    let url = match ensure_dsh_url() {
        Ok(u) => u,
        Err(e) => {
            eprintln!("[oh] WARNING: {e}");
            format!("http://127.0.0.1:{DSH_PORT}")
        }
    };
    println!("[oh] ────────────────────────────────────────────");
    println!("[oh] engine: {} (alias {})", cfg.engine_base_url(), Config::alias_for(&model));
    println!("[oh] dsh:    {url}");
    println!("[oh] ────────────────────────────────────────────");
    remember_supervisor();
    // The Open Harness UI lives in this process — stay alive until stopped.
    match server::spawn() {
        Ok(p) => println!("[oh] UI: http://127.0.0.1:{p}"),
        Err(e) => eprintln!("[oh] WARNING: web UI unavailable: {e}"),
    }
    println!("[oh] serving until stopped (open-harness stop)");
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

/// Headless Open Harness only: stack + web UI, no dsh. Blocks until stopped.
fn cmd_serve() -> i32 {
    let cfg = match load_config() {
        Ok(c) => c,
        Err(e) => return fail(&e),
    };
    let model = match desired_model(&cfg, None) {
        Ok(m) => m,
        Err(e) => return fail(&e),
    };
    if let Err(e) = reload_engine(&cfg, &model, None) {
        return fail(&e);
    }
    if let Err(e) = patch::write_patch(&cfg, &Config::alias_for(&model)) {
        eprintln!("[oh] WARNING: could not write dsh config: {e}");
    }
    remember_supervisor();
    match server::spawn() {
        Ok(p) => println!("[oh] Open Harness UI: http://127.0.0.1:{p}"),
        Err(e) => return fail(&e),
    }
    println!("[oh] serving until stopped (open-harness stop)");
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

/// Record this process so `stop` can end a headless server too.
fn remember_supervisor() {
    let mut st = state::load(&state_path()).unwrap_or_default();
    st.supervisor_pid = Some(std::process::id());
    state::save(&state_path(), &st);
}

/// Desktop experience: ensure the stack is up, then open the Open Harness UI
/// in its own native window (WebView2). Closing the window leaves the engine
/// running in the background; use `stop` to shut everything down.
fn cmd_app(needle: Option<&str>) -> i32 {
    // Another Open Harness instance already owns the stack: never manage the
    // engine from here (two supervisors would fight over one engine) — just
    // open another window onto the running UI.
    if gui::ui_already_serving() {
        println!("[oh] stack already running — opening a window on it");
        return match gui::open_app("Open Harness", || Ok(3080)) {
            Ok(()) => {
                println!("[oh] window closed — the running instance keeps going");
                0
            }
            Err(e) => return fail(&e),
        };
    }
    let cfg = match load_config() {
        Ok(c) => c,
        Err(e) => return fail(&e),
    };
    let model = match desired_model(&cfg, needle) {
        Ok(m) => m,
        Err(e) => return fail(&e),
    };
    // No blocking wait: the window opens while the model loads, and the UI
    // polls /api/status so it shows the load progress itself.
    if let Err(e) = reload_engine(&cfg, &model, None) {
        return fail(&e);
    }
    if let Err(e) = patch::write_patch(&cfg, &Config::alias_for(&model)) {
        eprintln!("[oh] WARNING: could not write dsh config: {e}");
    }
    remember_supervisor();
    println!("[oh] opening native window...");
    match gui::open_app("Open Harness", server::spawn) {
        Ok(()) => {
            println!("[oh] window closed — engine keeps running in the background (stop with: open-harness stop)");
            0
        }
        Err(e) => {
            eprintln!("[oh] {e}");
            eprintln!("[oh] falling back to your default browser");
            let _ = Command::new("cmd").args(["/C", "start", "", "http://127.0.0.1:3080"]).spawn();
            1
        }
    }
}

/// Hot-swap the served model: restart the engine with the new .gguf, wait for
/// health, update the dsh patch. The UI picks the new model up from
/// /api/status. If the new model fails to load, roll back to the previously
/// served one. Shared by the `load` CLI command and the web API.
pub(crate) fn perform_load(model: &Path) -> Result<(), String> {
    let cfg = load_config()?;
    let previous = state::load(&state_path())
        .and_then(|s| s.last_healthy)
        .map(PathBuf::from)
        .or_else(loaded_model);
    let alias = Config::alias_for(model);
    println!("[oh] loading {alias}");
    if let Err(e) = reload_engine(&cfg, model, Some(START_WAIT_SECS)) {
        eprintln!("[oh] load failed: {e}");
        if let Some(prev) = previous.filter(|p| !same_path(p, model)) {
            eprintln!("[oh] rolling back to {}", prev.display());
            match reload_engine(&cfg, &prev, Some(300)) {
                Ok(_) => {
                    let prev_alias = Config::alias_for(&prev);
                    let _ = patch::write_patch(&cfg, &prev_alias);
                    eprintln!("[oh] rollback OK — still serving {prev_alias}");
                }
                Err(re) => eprintln!("[oh] rollback failed: {re}"),
            }
        }
        return Err(e);
    }
    if let Err(e) = patch::write_patch(&cfg, &alias) {
        eprintln!("[oh] WARNING: could not write dsh config: {e}");
    }
    println!("[oh] now serving: {alias}");
    Ok(())
}

fn cmd_load(needle: &str) -> i32 {
    let cfg = match load_config() {
        Ok(c) => c,
        Err(e) => return fail(&e),
    };
    let model = match cfg.resolve_model(Some(needle)) {
        Ok(m) => m,
        Err(e) => return fail(&e),
    };
    match perform_load(&model) {
        Ok(()) => {
            println!("[oh] the UI picks it up automatically; dsh sessions pick it up on the next request");
            0
        }
        Err(_) => 1,
    }
}

/// Add a folder to engine.scanDirs (deduped, persisted to config.json).
/// Returns true when newly added, false when it was already indexed.
pub(crate) fn add_scan_dir(cfg_path: &Path, dir: &Path) -> Result<bool, String> {
    let raw = std::fs::read_to_string(cfg_path).map_err(|_| "cannot read config.json".to_string())?;
    let mut v: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| format!("bad config.json: {e}"))?;
    let entry = dir.display().to_string();
    let dirs = v
        .get_mut("engine")
        .and_then(|e| e.get_mut("scanDirs"))
        .and_then(|d| d.as_array_mut())
        .ok_or("config.json has no engine.scanDirs array")?;
    if dirs.iter().any(|d| d.as_str() == Some(entry.as_str())) {
        return Ok(false);
    }
    dirs.push(serde_json::Value::String(entry));
    serde_json::to_string_pretty(&v)
        .map_err(|e| e.to_string())
        .and_then(|s| std::fs::write(cfg_path, s).map_err(|e| e.to_string()))?;
    Ok(true)
}

/// Persist a new scan folder into config.json and show what it added.
fn cmd_scan(dir: &str) -> i32 {
    let path = PathBuf::from(dir);
    if !path.is_dir() {
        return fail(&format!("not a folder: {}", path.display()));
    }
    match add_scan_dir(&config_path(), &path) {
        Ok(true) => println!("[oh] indexed folder: {}", path.display()),
        Ok(false) => println!("[oh] already indexed: {}", path.display()),
        Err(e) => return fail(&e),
    }
    let cfg = match load_config() {
        Ok(c) => c,
        Err(e) => return fail(&e),
    };
    let models = cfg.discover_models();
    println!("[oh] {} loadable model(s) across all indexed folders:", models.len());
    let loaded = loaded_model();
    for m in &models {
        let mark = if loaded.as_ref().is_some_and(|l| same_path(l, m)) {
            " [loaded]"
        } else if same_path(&cfg.default_model, m) {
            " [default]"
        } else {
            ""
        };
        let size = std::fs::metadata(m).map(|md| md.len()).unwrap_or(0);
        println!("  {} ({:.1} GB){}", Config::alias_for(m), size as f64 / 1e9, mark);
    }
    println!("[oh] serve one with: open-harness load <partial-name> — or pick it in the UI");
    0
}

fn cmd_models() -> i32 {
    let cfg = match load_config() {
        Ok(c) => c,
        Err(e) => return fail(&e),
    };
    let models = cfg.discover_models();
    if models.is_empty() {
        println!("No loadable models found. Index a folder with: open-harness scan <folder>");
        return 0;
    }
    println!("Loadable models:");
    let loaded = loaded_model();
    for m in &models {
        let mark = if loaded.as_ref().is_some_and(|l| same_path(l, m)) {
            " [loaded]"
        } else if same_path(&cfg.default_model, m) {
            " [default]"
        } else {
            ""
        };
        let size = std::fs::metadata(m).map(|md| md.len()).unwrap_or(0);
        println!("  {} ({:.1} GB){}", Config::alias_for(m), size as f64 / 1e9, mark);
    }
    println!("serve one with: open-harness load <partial-name>");
    0
}

/* ── doctor: hardware + stack health report (Colibri's `doctor` idea) ── */

/// One doctor finding: "ok" / "warn" / "fail" / "info", a short label, detail.
struct Finding {
    status: &'static str,
    label: String,
    detail: String,
}

impl Finding {
    fn new(status: &'static str, label: impl Into<String>, detail: impl Into<String>) -> Self {
        Self { status, label: label.into(), detail: detail.into() }
    }
}

fn report_doctor(findings: Vec<Finding>, as_json: bool, code: i32) -> i32 {
    if as_json {
        let v = serde_json::json!({
            "ok": code == 0,
            "findings": findings.iter().map(|x| serde_json::json!({
                "status": x.status, "label": x.label, "detail": x.detail,
            })).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
    } else {
        println!("[oh] doctor — hardware + stack report");
        for x in &findings {
            let mark = match x.status {
                "ok" => "✓",
                "warn" => "!",
                "fail" => "✗",
                _ => "·",
            };
            println!(" {mark} {:<15} {}", x.label, x.detail);
        }
        let (oks, warns, fails) = (
            findings.iter().filter(|x| x.status == "ok").count(),
            findings.iter().filter(|x| x.status == "warn").count(),
            findings.iter().filter(|x| x.status == "fail").count(),
        );
        println!("[oh] doctor: {oks} ok, {warns} warning(s), {fails} failure(s)");
    }
    code
}

/// llama.cpp-specific doctor check: run the binary's --version banner.
fn doctor_llamacpp(exe: &Path, f: &mut Vec<Finding>) {
    if !exe.is_file() {
        f.push(Finding::new(
            "fail",
            "engine binary",
            format!("missing: {}", exe.display()),
        ));
        return;
    }
    let ver = std::process::Command::new(exe)
        .arg("--version")
        .output()
        .ok()
        .and_then(|o| {
            // llama.cpp prints the version banner on stderr.
            String::from_utf8_lossy(&o.stderr)
                .lines()
                .chain(String::from_utf8_lossy(&o.stdout).lines())
                .find(|l| l.contains("version:"))
                .map(String::from)
        })
        .unwrap_or_default();
    f.push(Finding::new(
        "ok",
        "engine binary",
        format!("{} ({})", exe.display(), ver),
    ));
}

fn cmd_doctor(as_json: bool) -> i32 {
    const GB: u64 = 1024 * 1024 * 1024;
    let mut f: Vec<Finding> = Vec::new();
    let cfg = match load_config() {
        Ok(c) => c,
        Err(e) => {
            f.push(Finding::new("fail", "config", e));
            return report_doctor(f, as_json, 1);
        }
    };
    f.push(Finding::new("ok", "config", format!("{} parsed", config_path().display())));

    if let Some(ram) = telemetry::sys_ram() {
        let status = if ram.avail < 4 * GB { "fail" } else { "ok" };
        f.push(Finding::new(
            status,
            "system RAM",
            format!(
                "{:.1} GB total, {:.1} GB available",
                ram.total as f64 / 1e9,
                ram.avail as f64 / 1e9
            ),
        ));
    } else {
        f.push(Finding::new("warn", "system RAM", "could not read the memory status"));
    }

    let gpus = telemetry::gpus();
    if gpus.is_empty() {
        f.push(Finding::new(
            "warn",
            "GPU",
            "none detected via nvidia-smi — inference will be CPU-only",
        ));
    } else {
        for g in &gpus {
            f.push(Finding::new(
                "ok",
                "GPU",
                format!(
                    "{} · {:.1} GB VRAM ({:.2} GB in use now)",
                    g.name,
                    g.total as f64 / 1024.0,
                    g.used as f64 / 1024.0
                ),
            ));
        }
    }

    if cfg.is_airllm() {
        let py = cfg.airllm_python();
        if py.is_file() {
            f.push(Finding::new(
                "ok",
                "engine binary",
                format!("airllm sidecar ({})", py.display()),
            ));
        } else {
            f.push(Finding::new(
                "fail",
                "engine binary",
                format!(
                    "airllm venv python missing: {} — run engine\\airllm\\setup-airllm.bat",
                    py.display()
                ),
            ));
        }
    } else {
        doctor_llamacpp(&cfg.engine.exe, &mut f);
    }

    let model = loaded_model().unwrap_or_else(|| cfg.default_model.clone());
    if model.is_file() || model.is_dir() {
        let size = crate::config::model_size(&model);
        f.push(Finding::new(
            "ok",
            "model",
            format!(
                "{} ({:.1} GB)",
                Config::alias_for(&model),
                size as f64 / 1e9
            ),
        ));
        // Placement verdict — Colibri's multitier plan, in one line: what
        // lives in VRAM, in RAM, or has to stream from disk.
        let vram_total: u64 = gpus.iter().map(|g| g.total * 1024 * 1024).sum();
        let ram_total = telemetry::sys_ram().map(|r| r.total).unwrap_or(0);
        let over_limit = cfg
            .ram_limit_gb
            .is_some_and(|lim| size as f64 > lim * 1e9);
        let tier = if vram_total > 0 && size <= vram_total {
            "fits fully in VRAM — fastest tier"
        } else if size <= ram_total {
            "hybrid: auto-fit offloads what fits in VRAM, the rest stays resident in RAM via mmap"
        } else {
            "model exceeds physical RAM — heavy disk streaming, expect slow decode"
        };
        f.push(Finding::new(
            if over_limit { "warn" } else { "info" },
            "placement",
            if over_limit {
                format!("{tier} (also over the configured RAM limit — load will be refused)")
            } else {
                tier.to_string()
            },
        ));
    } else {
        f.push(Finding::new(
            "fail",
            "model",
            format!("missing: {}", model.display()),
        ));
    }

    f.push(Finding::new(
        "info",
        "RAM limit",
        match cfg.ram_limit_gb {
            Some(lim) => format!("{lim:.0} GB (bigger models are refused at load)"),
            None => "off".into(),
        },
    ));

    let st = state::load(&state_path()).unwrap_or_default();
    let healthy = engine_healthy(&cfg);
    if healthy {
        f.push(Finding::new(
            "ok",
            "engine",
            format!("healthy at {} — serving {}", cfg.engine_base_url(), st.model),
        ));
    } else if st.engine_pid.is_some_and(crate::pid_alive) {
        f.push(Finding::new(
            "warn",
            "engine",
            "process alive but not answering /health (still loading, or wedged — see engine.log)",
        ));
    } else {
        f.push(Finding::new(
            "info",
            "engine",
            "not running — start with Start-IDE.bat or: open-harness app",
        ));
    }

    for (port, what) in [(3080u16, "Open Harness UI"), (cfg.engine.port, "engine API"), (DSH_PORT, "agent IDE")] {
        let open = port_open("127.0.0.1", port);
        f.push(Finding::new(
            if open { "ok" } else { "info" },
            what,
            format!("port {port} {}", if open { "listening" } else { "down" }),
        ));
    }

    if cfg.engine.profiles.is_empty() {
        f.push(Finding::new(
            "info",
            "tuned profiles",
            "none yet — measure one with: open-harness tune",
        ));
    } else {
        f.push(Finding::new(
            "ok",
            "tuned profiles",
            cfg.engine
                .profiles
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", "),
        ));
    }

    let missing_dirs: Vec<String> = cfg
        .engine
        .scan_dirs
        .iter()
        .filter(|d| !d.is_dir())
        .map(|d| d.display().to_string())
        .collect();
    if !missing_dirs.is_empty() {
        f.push(Finding::new(
            "warn",
            "scan dirs",
            format!("missing folders: {}", missing_dirs.join(", ")),
        ));
    }
    let missing_mmproj: Vec<String> = cfg
        .engine
        .mmproj_rules
        .iter()
        .filter(|r| !r.path.is_file())
        .map(|r| r.path.display().to_string())
        .collect();
    if !missing_mmproj.is_empty() {
        f.push(Finding::new(
            "warn",
            "vision adapters",
            format!("missing files: {}", missing_mmproj.join(", ")),
        ));
    }

    let code = if f.iter().any(|x| x.status == "fail") { 1 } else { 0 };
    report_doctor(f, as_json, code)
}

/* ── tune: measure the fastest engine flags for one model (Colibri's `tune`) ── */

/// One llama-bench configuration and its measured decode speed.
struct BenchResult {
    fa: String,
    fit_mib: u32,
    ngl: u32,
    threads: u32,
    tok_s: f64,
}

/// Split one CSV line, respecting double-quoted fields (cpu_info can hold
/// commas). Doubled quotes inside a field are literal quotes.
fn split_csv_line(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                if in_quotes && chars.peek() == Some(&'"') {
                    chars.next();
                    cur.push('"');
                } else {
                    in_quotes = !in_quotes;
                }
            }
            ',' if !in_quotes => {
                out.push(cur.trim().to_string());
                cur.clear();
            }
            _ => cur.push(c),
        }
    }
    out.push(cur.trim().to_string());
    out
}

/// Parse llama-bench's `-o csv` output: header + one row per test; return the
/// first row's (n_gpu_layers, fit_target, n_threads, avg_ts).
fn parse_bench_csv(text: &str) -> Option<(u32, u32, u32, f64)> {
    let mut rows = text.lines().filter(|l| l.contains(','));
    let header = split_csv_line(rows.next()?);
    let idx = |name: &str| header.iter().position(|h| h == name);
    let (i_ngl, i_fit, i_threads, i_ts) = (
        idx("n_gpu_layers")?,
        idx("fit_target")?,
        idx("n_threads")?,
        idx("avg_ts")?,
    );
    let row = split_csv_line(rows.next()?);
    Some((
        row.get(i_ngl)?.parse().ok()?,
        row.get(i_fit)?.parse().ok()?,
        row.get(i_threads)?.parse().ok()?,
        row.get(i_ts)?.parse().ok()?,
    ))
}

/// Write the winning config into config.json as engine.profiles[<stem>].
/// Raw JSON mutation (like add_scan_dir) so unknown keys survive.
fn save_tuned_profile(
    cfg_path: &Path,
    model: &Path,
    best: &BenchResult,
    cpu_only: bool,
) -> Result<(), String> {
    let raw = std::fs::read_to_string(cfg_path).map_err(|e| format!("cannot read config.json: {e}"))?;
    let mut v: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| format!("bad config.json: {e}"))?;
    let eng = v
        .get_mut("engine")
        .and_then(|e| e.as_object_mut())
        .ok_or("config.json has no engine object")?;
    let profiles = eng
        .entry("profiles".to_string())
        .or_insert_with(|| serde_json::json!({}));
    let stem = Config::alias_for(model);
    let p = serde_json::json!({
        "threads": if cpu_only { serde_json::json!(best.threads) } else { serde_json::Value::Null },
        "gpuLayers": if cpu_only { serde_json::json!(0) } else { serde_json::Value::Null },
        "fitTargetMib": if best.fit_mib > 0 { serde_json::json!(best.fit_mib) } else { serde_json::Value::Null },
        "flashAttn": serde_json::json!(best.fa),
        "note": format!("tuned by open-harness tune: {:.1} tok/s decode", best.tok_s),
        "savedAt": unix_now(),
    });
    let map = profiles
        .as_object_mut()
        .ok_or("engine.profiles is not an object")?;
    map.insert(stem, p);
    serde_json::to_string_pretty(&v)
        .map_err(|e| e.to_string())
        .and_then(|s| std::fs::write(cfg_path, s).map_err(|e| e.to_string()))
}

fn cmd_tune(needle: Option<&str>, quick: bool) -> i32 {
    let cfg = match load_config() {
        Ok(c) => c,
        Err(e) => return fail(&e),
    };
    let model = match needle {
        Some(n) => cfg.resolve_model(Some(n)),
        None => Ok(loaded_model().unwrap_or_else(|| cfg.default_model.clone())),
    }
    .and_then(|m| {
        if m.is_file() {
            Ok(m)
        } else {
            Err(format!("model file not found: {}", m.display()))
        }
    });
    let model = match model {
        Ok(m) => m,
        Err(e) => return fail(&e),
    };
    let bench = cfg
        .engine
        .exe
        .parent()
        .map(|p| p.join("llama-bench.exe"))
        .unwrap_or_else(|| PathBuf::from("llama-bench.exe"));
    if !bench.is_file() {
        return fail(&format!("llama-bench.exe not found at {}", bench.display()));
    }
    if engine_healthy(&cfg) {
        println!(
            "[oh] NOTE: the engine is running and holds model memory. Bench still works \
             (it mmaps the same file), but VRAM contention can skew GPU numbers — \
             'open-harness stop' first for the cleanest measurement."
        );
    }
    let gpus = telemetry::gpus();
    let cpu_only = gpus.is_empty();
    let (pp, n, reps) = if quick { (256u32, 64u32, 1u32) } else { (512u32, 128u32, 2u32) };
    // With a GPU the levers are the auto-fit margin (how much VRAM is kept
    // free for KV cache) and Flash Attention; without one, thread count.
    let threads = cfg.engine.threads;
    let configs: Vec<(u32, u32, &str, u32)> = if cpu_only {
        vec![(0, 0, "auto", threads), (0, 0, "auto", (threads / 2).max(2))]
    } else if quick {
        vec![(99, 700, "auto", threads), (99, 700, "on", threads)]
    } else {
        vec![
            (99, 700, "auto", threads),
            (99, 700, "on", threads),
            (99, 700, "off", threads),
            (99, 1400, "auto", threads),
        ]
    };
    println!(
        "[oh] tuning {} — {} config(s) × (pp {pp} + tg {n}) × {reps} rep(s); each config reloads the model",
        Config::alias_for(&model),
        configs.len()
    );
    let model_str = model.display().to_string();
    let mut results: Vec<BenchResult> = Vec::new();
    for (ngl, fit, fa, threads) in &configs {
        let mut cmd = Command::new(&bench);
        cmd.args([
            "-m", &model_str,
            "-p", &pp.to_string(),
            "-n", &n.to_string(),
            "-r", &reps.to_string(),
            "-o", "csv",
            "-t", &threads.to_string(),
            "-fa", fa,
            "-ngl", &ngl.to_string(),
        ]);
        if *fit > 0 {
            cmd.args(["-fitt", &fit.to_string()]);
        }
        println!("[oh]   bench: -ngl {ngl} -fitt {fit} -fa {fa} -t {threads} …");
        match cmd.output() {
            Err(e) => eprintln!("[oh]   bench failed to start: {e}"),
            Ok(o) if !o.status.success() => {
                let err = String::from_utf8_lossy(&o.stderr);
                eprintln!(
                    "[oh]   config failed: {}",
                    err.lines().last().unwrap_or("unknown error")
                );
            }
            Ok(o) => {
                let text = String::from_utf8_lossy(&o.stdout);
                match parse_bench_csv(&text) {
                    Some((b_ngl, b_fit, b_threads, ts)) => {
                        println!("[oh]   → {:.1} tok/s decode", ts);
                        results.push(BenchResult {
                            fa: (*fa).into(),
                            fit_mib: b_fit,
                            ngl: b_ngl,
                            threads: b_threads,
                            tok_s: ts,
                        });
                    }
                    None => eprintln!("[oh]   could not parse llama-bench output"),
                }
            }
        }
    }
    if results.is_empty() {
        return fail("no bench config succeeded — see the errors above");
    }
    results.sort_by(|a, b| b.tok_s.partial_cmp(&a.tok_s).unwrap_or(std::cmp::Ordering::Equal));
    println!("[oh] results (fastest decode first):");
    for r in &results {
        println!(
            "[oh]   {:7.1} tok/s · fa {} · fit-target {} MiB · ngl {} · t {}",
            r.tok_s, r.fa, r.fit_mib, r.ngl, r.threads
        );
    }
    let best = &results[0];
    let cpu_profile = cpu_only;
    match save_tuned_profile(&config_path(), &model, best, cpu_profile) {
        Ok(()) => println!(
            "[oh] saved the tuned profile for {} — it applies the next time this model loads (engine_args picks it up; Settings values you typed still win)",
            Config::alias_for(&model)
        ),
        Err(e) => return fail(&format!("could not save the profile: {e}")),
    }
    0
}

fn cmd_stop() -> i32 {
    let mut code = 0;
    let st = state::load(&state_path());
    // The web UI lives inside the supervisor process; stopping it takes a
    // headless `serve`/`start` down with the rest of the stack.
    if let Some(pid) = st.as_ref().and_then(|s| s.supervisor_pid) {
        if pid != std::process::id() {
            println!("[oh] stopping supervisor pid {pid}");
            let _ = kill_tree(pid);
        }
    }
    if let Some(pid) = st.as_ref().and_then(|s| s.engine_pid) {
        println!("[oh] stopping engine pid {pid}");
        if kill_tree(pid).is_err() {
            code = 1;
        }
    }
    // dsh: state pid first, else whoever listens on the port (covers state loss)
    let dsh_pid = st
        .as_ref()
        .and_then(|s| s.dsh_pid)
        .or_else(|| pid_listening_on(DSH_PORT));
    if let Some(pid) = dsh_pid {
        println!("[oh] stopping agent IDE pid {pid}");
        if kill_tree(pid).is_err() {
            code = 1;
        }
    }
    // The OCR fallback sidecar, when one has ever been spawned.
    if let Some(pid) = st.as_ref().and_then(|s| s.ocr_pid) {
        println!("[oh] stopping OCR fallback engine pid {pid}");
        let _ = kill_tree(pid);
    }
    let _ = std::fs::remove_file(state_path());
    if code == 0 {
        println!("[oh] stopped.");
    }
    code
}

fn cmd_status() -> i32 {
    let cfg = match load_config() {
        Ok(c) => c,
        Err(e) => return fail(&e),
    };
    let loaded = loaded_model();
    let healthy = engine_healthy(&cfg);
    println!("engine:    {} ({})", cfg.engine_base_url(), if healthy { "healthy" } else { "down" });
    match &loaded {
        Some(m) => println!("model:     {} [alias {}]", m.display(), Config::alias_for(m)),
        None => println!("model:     <unknown>"),
    }
    println!(
        "open ui:   {}",
        if port_open("127.0.0.1", 3080) { "http://127.0.0.1:3080" } else { "down" }
    );
    println!(
        "dsh ui:    {}",
        if port_open("127.0.0.1", DSH_PORT) {
            dsh_url_from_log(&root_dir().join("dsh-ide.log"))
                .unwrap_or_else(|| format!("listening on port {DSH_PORT}"))
        } else {
            "down".into()
        }
    );
    println!(
        "scan dirs: {:?}",
        cfg.engine.scan_dirs.iter().map(|d| d.display().to_string()).collect::<Vec<_>>()
    );
    if healthy {
        0
    } else {
        1
    }
}

fn cmd_wait(secs: u64) -> i32 {
    let cfg = match load_config() {
        Ok(c) => c,
        Err(e) => return fail(&e),
    };
    if wait_healthy(&cfg, secs) {
        println!("[oh] engine healthy at {}", cfg.engine_base_url());
        0
    } else {
        eprintln!("[oh] engine not healthy after {secs}s");
        1
    }
}

fn fail(msg: &str) -> i32 {
    eprintln!("[oh] {msg}");
    1
}
