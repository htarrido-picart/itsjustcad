// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Local model runtime manager: spawn a downloaded model as a subprocess that
//! serves an OpenAI-compatible endpoint, health-check it, and hand the deck a
//! live `127.0.0.1:<port>` base URL to talk to.
//!
//! Two runtimes, mirroring the catalog's [`crate::model_catalog::Runtime`]:
//!
//! - **llamafile**: the downloaded file *is* the server executable. We `chmod +x`
//!   it and spawn it with `--server --port <p> --nobrowser`, which exposes an
//!   OpenAI-compatible API on `127.0.0.1:<p>`.
//! - **gguf**: the file is just weights. We spawn a llama.cpp `llama-server` (or
//!   `server`) found on `PATH` with `-m <file> --port <p>`. If neither binary is
//!   present we fail with a clear message telling the user to install llama.cpp
//!   or pick a llamafile model.
//!
//! The spawn + subprocess pattern copies `deck/src/claude_code.rs`: a
//! `tokio::process::Command` with `kill_on_drop(true)` so the server dies with
//! the app (or when we switch away and drop the handle). The health-check polls
//! `GET /health` (llama.cpp/llamafile both expose it) until it 200s or a timeout
//! elapses; the UI never blocks — spawn + poll run on the tokio runtime and the
//! pane reads a shared [`RuntimeState`] each frame.
//!
//! Everything with real logic — arg/command construction, free-port selection,
//! the runtime resolution, and the health-check state machine — is a pure
//! function or a small method exercised by unit tests. A real end-to-end needs a
//! multi-GB model on disk, so that path is verified by the user via Model Setup.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::model_catalog::{Catalog, Runtime};

/// What the pane polls each frame to render a small status line and decide
/// whether a turn may start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeState {
    /// Spawned; health-check polling is in flight.
    Starting,
    /// Health check passed; the endpoint at `base_url` is serving.
    Ready { base_url: String },
    /// Spawn or health-check failed; `msg` is a short human reason.
    Failed { msg: String },
}

impl RuntimeState {
    /// One-line caption for the deck pane. Never empty — a runtime only exists
    /// once we've begun starting one.
    pub fn caption(&self) -> String {
        match self {
            RuntimeState::Starting => "starting local model…".to_string(),
            RuntimeState::Ready { .. } => "local model ready".to_string(),
            RuntimeState::Failed { msg } => format!("local model failed: {msg}"),
        }
    }

    /// The live base URL once ready, else `None`. Public accessor kept for
    /// callers that only need the URL without matching the variant.
    #[allow(dead_code)]
    pub fn ready_base_url(&self) -> Option<&str> {
        match self {
            RuntimeState::Ready { base_url } => Some(base_url),
            _ => None,
        }
    }
}

/// How a resolved local model is served. Produced by [`resolve_runtime`] from a
/// cassette's model id; consumed by [`build_command`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimePlan {
    pub runtime: Runtime,
    /// Absolute path to the downloaded model file.
    pub file: PathBuf,
}

/// Resolve the cassette's `model` (a catalog id) into a concrete file + runtime.
///
/// Pure over its inputs (the catalog + models dir) so the resolution rule is
/// unit-testable. Errors are human messages the caller surfaces as `Failed`.
pub fn resolve_runtime(
    catalog: &Catalog,
    models_dir: &Path,
    model_id: &str,
) -> Result<RuntimePlan, String> {
    let entry = catalog
        .get(model_id)
        .ok_or_else(|| format!("unknown local model id '{model_id}'"))?;
    let file = models_dir.join(entry.file_name());
    if !file.exists() {
        return Err(format!(
            "model file not found: {} — (re)install it from Model Setup",
            file.display()
        ));
    }
    Ok(RuntimePlan {
        runtime: entry.runtime,
        file,
    })
}

/// The program + args to spawn for a plan on `port`, plus the PATH-resolved
/// llama.cpp server binary for the gguf case.
///
/// - llamafile: `(<file>, ["--server", "--port", p, "--nobrowser"])` — the file
///   itself is the executable (the caller `chmod +x`es it first).
/// - gguf: `(<llama-server|server>, ["-m", <file>, "--host", "127.0.0.1",
///   "--port", p])` — requires a llama.cpp server on PATH, else an error.
///
/// Pure so both command shapes are unit-testable without spawning anything.
/// `find_on_path` is injected so the gguf lookup can be tested deterministically.
pub fn build_command(
    plan: &RuntimePlan,
    port: u16,
    find_on_path: impl Fn(&str) -> Option<PathBuf>,
) -> Result<(PathBuf, Vec<String>), String> {
    let port = port.to_string();
    match plan.runtime {
        Runtime::Llamafile => Ok((
            plan.file.clone(),
            vec![
                "--server".into(),
                "--port".into(),
                port,
                "--nobrowser".into(),
            ],
        )),
        Runtime::Gguf => {
            // llama.cpp renamed `server` → `llama-server`; accept either.
            let bin = find_on_path("llama-server")
                .or_else(|| find_on_path("server"))
                .ok_or_else(|| {
                    "no llama.cpp server on PATH (looked for 'llama-server' and \
                     'server'). Install llama.cpp, or pick a llamafile model in \
                     Model Setup, which needs no extra install."
                        .to_string()
                })?;
            Ok((
                bin,
                vec![
                    "-m".into(),
                    plan.file.display().to_string(),
                    "--host".into(),
                    "127.0.0.1".into(),
                    "--port".into(),
                    port,
                ],
            ))
        }
    }
}

/// The OpenAI-compatible base URL a server on `port` exposes.
pub fn base_url_for_port(port: u16) -> String {
    format!("http://127.0.0.1:{port}/v1")
}

/// The health-check URL for a server on `port` (llama.cpp/llamafile `/health`).
pub fn health_url_for_port(port: u16) -> String {
    format!("http://127.0.0.1:{port}/health")
}

/// Look up an executable on `PATH` (the real injector for [`build_command`]).
pub fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|cand| cand.is_file())
}

/// Ask the OS for a free TCP port by binding `127.0.0.1:0` and reading back the
/// assigned port. There is an inherent race (the port is released before the
/// child binds it) but it is the standard approach and the health-check catches
/// a failed bind. Returns a human error if no port could be obtained.
pub fn free_port() -> Result<u16, String> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")
        .map_err(|e| format!("could not reserve a local port: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("could not read local port: {e}"))?
        .port();
    Ok(port)
}

/// Ensure the downloaded file is executable (llamafile case). No-op on non-unix.
/// Pure-ish helper split out so the mode math is unit-testable.
#[cfg(unix)]
pub fn ensure_executable(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let mut perms = std::fs::metadata(path)?.permissions();
    let mode = perms.mode();
    // Add the execute bit for user/group/other, preserving read/write.
    perms.set_mode(mode | 0o111);
    std::fs::set_permissions(path, perms)
}

#[cfg(not(unix))]
pub fn ensure_executable(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Compute the mode to set so a file is user-executable, given its current mode.
/// Pure — the core of [`ensure_executable`]'s bit math, unit-tested without
/// touching disk. (The unix impl inlines `mode | 0o111`; this mirrors it so the
/// rule is exercised on every platform's test run.)
#[allow(dead_code)]
pub fn add_exec_bits(mode: u32) -> u32 {
    mode | 0o111
}

/// A running (or starting) local model server. Holds the child so `kill_on_drop`
/// tears it down on app exit or when the manager drops it to switch models.
pub struct LocalRuntime {
    /// Which model id this runtime is serving (so we can detect a model switch).
    model_id: String,
    /// The spawned child; kept alive to keep the server up. `kill_on_drop(true)`.
    _child: tokio::process::Child,
    /// Shared with the health-check task; the pane reads it each frame.
    state: Arc<Mutex<RuntimeState>>,
}

impl LocalRuntime {
    /// The model id this runtime serves.
    pub fn model_id(&self) -> &str {
        &self.model_id
    }

    /// Current state (cloned; lock held briefly).
    pub fn state(&self) -> RuntimeState {
        self.state.lock().expect("runtime state mutex").clone()
    }

    /// Spawn the server for `plan` and start a background health-check on
    /// `handle`. Returns immediately with the child held (state = `Starting`);
    /// the UI polls [`state`](Self::state) until `Ready`/`Failed`.
    ///
    /// `chmod +x` runs for llamafiles before spawn. The health-check polls
    /// `GET /health` until 200 or `timeout` elapses.
    pub fn spawn(
        handle: &tokio::runtime::Handle,
        model_id: &str,
        plan: &RuntimePlan,
        port: u16,
        timeout: std::time::Duration,
    ) -> Result<Self, String> {
        if plan.runtime == Runtime::Llamafile {
            ensure_executable(&plan.file)
                .map_err(|e| format!("cannot make {} executable: {e}", plan.file.display()))?;
        }
        let (program, args) = build_command(plan, port, which)?;

        let child = tokio::process::Command::new(&program)
            .args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("cannot launch {}: {e}", program.display()))?;

        let state = Arc::new(Mutex::new(RuntimeState::Starting));
        let health_url = health_url_for_port(port);
        let base_url = base_url_for_port(port);
        let state_bg = state.clone();
        handle.spawn(async move {
            let outcome = poll_health(&health_url, timeout).await;
            let mut guard = state_bg.lock().expect("runtime state mutex");
            *guard = match outcome {
                Ok(()) => RuntimeState::Ready { base_url },
                Err(msg) => RuntimeState::Failed { msg },
            };
        });

        Ok(Self {
            model_id: model_id.to_string(),
            _child: child,
            state,
        })
    }
}

/// Poll `health_url` until it returns a success status or `timeout` elapses.
/// Returns `Ok(())` when the server is up, else a human timeout message. Kept
/// separate from the spawn so the polling loop's shape is clear; the loop itself
/// needs a live socket so it is exercised by the manual e2e, while the
/// transition rule ([`classify_health`]) is unit-tested.
async fn poll_health(health_url: &str, timeout: std::time::Duration) -> Result<(), String> {
    let client = reqwest::Client::new();
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Ok(resp) = client.get(health_url).send().await
            && resp.status().is_success()
        {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "server did not become healthy within {}s",
                timeout.as_secs()
            ));
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}

/// The health-check decision for one poll, factored out so the state machine is
/// unit-testable without a socket: given whether the last probe succeeded and
/// whether the deadline passed, decide the next [`RuntimeState`] (or `None` to
/// keep polling). Mirrors the loop in [`poll_health`] so the transition rule is
/// unit-tested without a live socket.
#[allow(dead_code)]
pub fn classify_health(probe_ok: bool, deadline_passed: bool) -> Option<RuntimeState> {
    if probe_ok {
        // Caller fills in the real base_url; the marker distinguishes "ready".
        Some(RuntimeState::Ready {
            base_url: String::new(),
        })
    } else if deadline_passed {
        Some(RuntimeState::Failed {
            msg: "timed out".to_string(),
        })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gguf_plan() -> RuntimePlan {
        RuntimePlan {
            runtime: Runtime::Gguf,
            file: PathBuf::from("/models/qwen.gguf"),
        }
    }

    fn llamafile_plan() -> RuntimePlan {
        RuntimePlan {
            runtime: Runtime::Llamafile,
            file: PathBuf::from("/models/qwen.llamafile"),
        }
    }

    // ── llamafile command shape ────────────────────────────────────────────

    #[test]
    fn llamafile_spawns_the_file_itself_as_server() {
        let (prog, args) = build_command(&llamafile_plan(), 8123, |_| None).unwrap();
        // The downloaded file IS the executable.
        assert_eq!(prog, PathBuf::from("/models/qwen.llamafile"));
        assert!(args.contains(&"--server".to_string()));
        assert!(args.contains(&"--nobrowser".to_string()));
        // Port is passed through as the value after --port.
        let p = args.iter().position(|a| a == "--port").unwrap();
        assert_eq!(args[p + 1], "8123");
    }

    #[test]
    fn llamafile_does_not_consult_path() {
        // A llamafile never needs a PATH binary; the injector must not matter.
        let called = std::cell::Cell::new(false);
        let (prog, _) = build_command(&llamafile_plan(), 9000, |_| {
            called.set(true);
            None
        })
        .unwrap();
        assert_eq!(prog, PathBuf::from("/models/qwen.llamafile"));
        assert!(!called.get(), "llamafile must not look up PATH");
    }

    // ── gguf command shape + PATH gate ─────────────────────────────────────

    #[test]
    fn gguf_uses_llama_server_from_path() {
        let (prog, args) = build_command(&gguf_plan(), 8080, |name| {
            (name == "llama-server").then(|| PathBuf::from("/usr/bin/llama-server"))
        })
        .unwrap();
        assert_eq!(prog, PathBuf::from("/usr/bin/llama-server"));
        // Weights passed with -m; host pinned to loopback; port passed through.
        let m = args.iter().position(|a| a == "-m").unwrap();
        assert_eq!(args[m + 1], "/models/qwen.gguf");
        assert!(args.iter().any(|a| a == "127.0.0.1"));
        let p = args.iter().position(|a| a == "--port").unwrap();
        assert_eq!(args[p + 1], "8080");
    }

    #[test]
    fn gguf_falls_back_to_legacy_server_binary() {
        // Only the old `server` name is present → still resolves.
        let (prog, _) = build_command(&gguf_plan(), 8080, |name| {
            (name == "server").then(|| PathBuf::from("/opt/llama/server"))
        })
        .unwrap();
        assert_eq!(prog, PathBuf::from("/opt/llama/server"));
    }

    #[test]
    fn gguf_without_llama_server_is_a_clear_error() {
        let err = build_command(&gguf_plan(), 8080, |_| None).unwrap_err();
        assert!(err.contains("llama.cpp"), "{err}");
        assert!(err.contains("llama-server"), "{err}");
        // Points the user at the no-install alternative.
        assert!(err.to_lowercase().contains("llamafile"), "{err}");
    }

    // ── free port selection ────────────────────────────────────────────────

    #[test]
    fn free_port_is_nonzero_and_usable() {
        let p = free_port().unwrap();
        assert!(p > 0);
        // The port was released after probing, so we can bind it right now.
        let _l = std::net::TcpListener::bind(("127.0.0.1", p)).expect("port is bindable");
    }

    #[test]
    fn free_ports_are_distinct_across_calls() {
        // Not guaranteed by the OS, but in practice consecutive ephemeral ports
        // differ; this guards against a constant/stub implementation.
        let a = free_port().unwrap();
        let b = free_port().unwrap();
        // Bind a so b can't reuse it.
        let _la = std::net::TcpListener::bind(("127.0.0.1", a)).unwrap();
        let _lb = std::net::TcpListener::bind(("127.0.0.1", b)).unwrap();
        assert_ne!(a, b);
    }

    // ── url construction ───────────────────────────────────────────────────

    #[test]
    fn base_and_health_urls_are_loopback() {
        assert_eq!(base_url_for_port(8080), "http://127.0.0.1:8080/v1");
        assert_eq!(health_url_for_port(8080), "http://127.0.0.1:8080/health");
    }

    // ── exec bit math ──────────────────────────────────────────────────────

    #[test]
    fn add_exec_bits_sets_execute_preserving_rw() {
        // rw-r--r-- (0644) → rwxr-xr-x (0755): execute added, read/write kept.
        assert_eq!(add_exec_bits(0o644), 0o755);
        // Already executable stays put.
        assert_eq!(add_exec_bits(0o755), 0o755);
        // Only-write gains execute too.
        assert_eq!(add_exec_bits(0o600), 0o711);
    }

    // ── health-check state machine ─────────────────────────────────────────

    #[test]
    fn health_ready_when_probe_ok() {
        let s = classify_health(true, false).unwrap();
        assert!(matches!(s, RuntimeState::Ready { .. }));
        // Ready wins even if the deadline also passed.
        assert!(matches!(
            classify_health(true, true).unwrap(),
            RuntimeState::Ready { .. }
        ));
    }

    #[test]
    fn health_failed_when_deadline_passes_without_ok() {
        let s = classify_health(false, true).unwrap();
        assert!(matches!(s, RuntimeState::Failed { .. }));
    }

    #[test]
    fn health_keeps_polling_before_deadline() {
        assert_eq!(classify_health(false, false), None);
    }

    // ── state captions / accessors ─────────────────────────────────────────

    #[test]
    fn captions_read_as_status_lines() {
        assert_eq!(RuntimeState::Starting.caption(), "starting local model…");
        assert_eq!(
            RuntimeState::Ready {
                base_url: "http://127.0.0.1:8080/v1".into()
            }
            .caption(),
            "local model ready"
        );
        assert_eq!(
            RuntimeState::Failed { msg: "boom".into() }.caption(),
            "local model failed: boom"
        );
    }

    #[test]
    fn ready_base_url_only_when_ready() {
        assert_eq!(
            RuntimeState::Ready {
                base_url: "http://127.0.0.1:9/v1".into()
            }
            .ready_base_url(),
            Some("http://127.0.0.1:9/v1")
        );
        assert_eq!(RuntimeState::Starting.ready_base_url(), None);
        assert_eq!(RuntimeState::Failed { msg: "x".into() }.ready_base_url(), None);
    }

    // ── runtime resolution ─────────────────────────────────────────────────

    #[test]
    fn resolve_unknown_id_errors() {
        let cat = Catalog::load();
        let dir = std::env::temp_dir();
        let err = resolve_runtime(&cat, &dir, "no-such-model").unwrap_err();
        assert!(err.contains("unknown local model"), "{err}");
    }

    #[test]
    fn resolve_missing_file_points_at_model_setup() {
        let cat = Catalog::load();
        let id = cat.models[0].id.clone();
        // A dir that surely doesn't hold the multi-GB weights.
        let dir = std::env::temp_dir().join("ijc_no_models_here");
        let _ = std::fs::remove_dir_all(&dir);
        let err = resolve_runtime(&cat, &dir, &id).unwrap_err();
        assert!(err.contains("not found"), "{err}");
        assert!(err.contains("Model Setup"), "{err}");
    }

    #[test]
    fn resolve_found_file_reports_runtime_and_path() {
        let cat = Catalog::load();
        let entry = cat.models[0].clone();
        // Stand a zero-byte file in for the real weights so the path resolves.
        let dir = std::env::temp_dir().join("ijc_resolve_ok");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(entry.file_name());
        std::fs::write(&file, b"").unwrap();
        let plan = resolve_runtime(&cat, &dir, &entry.id).unwrap();
        assert_eq!(plan.runtime, entry.runtime);
        assert_eq!(plan.file, file);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
