// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Async model-file downloader with progress, resume, and SHA-256 verify.
//!
//! Design: the actual HTTPS fetch runs on the app's tokio runtime as a detached
//! background task. The UI thread never blocks — it only reads a shared
//! [`DownloadState`] (behind an `Arc<Mutex>`) each frame and renders it. A cancel
//! is a single atomic flag the task polls between chunks.
//!
//! On-disk protocol: bytes stream into `<dir>/<file>.part`; on a clean, verified
//! finish the `.part` is renamed to `<dir>/<file>`. A later run RESUMES by asking
//! the server for the remaining byte range (HTTP `Range`) starting at the size of
//! the existing `.part`.
//!
//! Everything that carries real logic — the resume-offset rule, the percent/speed
//! formatting, the SHA verify, and the state transitions — is a pure function or
//! a small method exercised by unit tests. The network fetch itself is NOT
//! unit-tested (it needs a live server); a `--example` drives it end-to-end.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};

/// What the UI polls each frame. Cheap to clone; the background task swaps the
/// shared copy under a mutex as it makes progress.
#[derive(Debug, Clone, PartialEq)]
pub enum DownloadState {
    /// No download in flight. The default resting state exposed to the UI; the
    /// active fetch never constructs it, but callers may (e.g. a reset).
    #[allow(dead_code)]
    Idle,
    /// Bytes are streaming. `total` is `None` when the server sent no
    /// `Content-Length` (rare for model hosts, but handled).
    Downloading {
        done: u64,
        total: Option<u64>,
        bytes_per_sec: f64,
    },
    /// Transfer finished; hashing the file to check it against the expected sum.
    Verifying,
    /// Complete and (if a hash was supplied) verified. `path` is the final file.
    Done { path: PathBuf },
    /// Failed or cancelled. `msg` is a short human reason.
    Failed { msg: String },
}

impl DownloadState {
    /// Fraction complete in `0.0..=1.0`, or `None` when the total is unknown or
    /// the state has no meaningful progress.
    pub fn fraction(&self) -> Option<f32> {
        match self {
            DownloadState::Downloading {
                done,
                total: Some(total),
                ..
            } if *total > 0 => Some((*done as f64 / *total as f64).clamp(0.0, 1.0) as f32),
            DownloadState::Verifying => None,
            DownloadState::Done { .. } => Some(1.0),
            _ => None,
        }
    }

    /// True while a fetch/verify is actively running (used to disable the
    /// Install button and offer Cancel).
    #[allow(dead_code)] // used by itsjustcad (app crate); invisible to download_model example
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            DownloadState::Downloading { .. } | DownloadState::Verifying
        )
    }
}

/// A live download the UI holds onto: shared state to read, a cancel flag to
/// flip. Dropping it does not stop the task — call [`Download::cancel`].
pub struct Download {
    state: Arc<Mutex<DownloadState>>,
    #[allow(dead_code)] // read via cancel() method; field itself only written in start()
    cancel: Arc<AtomicBool>,
}

impl Download {
    /// Read the current state (cloned so the lock is held only briefly).
    pub fn state(&self) -> DownloadState {
        self.state.lock().expect("download state mutex").clone()
    }

    /// Request cancellation. The background task notices between chunks, drops
    /// the connection, and moves to `Failed { "cancelled" }`.
    #[allow(dead_code)] // called by itsjustcad (app crate); invisible to download_model example
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    /// Test-only fake download with no background task: hand back the shared
    /// state + cancel handles so a test can drive `state()` forward and observe
    /// whether `cancel()` was ever requested. Used to prove that closing the
    /// Model Setup panel does NOT cancel an in-flight download.
    #[cfg(test)]
    pub(crate) fn for_test(
        initial: DownloadState,
    ) -> (Self, Arc<Mutex<DownloadState>>, Arc<AtomicBool>) {
        let state = Arc::new(Mutex::new(initial));
        let cancel = Arc::new(AtomicBool::new(false));
        (
            Self {
                state: state.clone(),
                cancel: cancel.clone(),
            },
            state,
            cancel,
        )
    }
}

/// Everything the fetch needs. `dir` is the models directory; `file_name` is the
/// final on-disk name; `expected_sha256` (lower-case hex) is verified when set.
#[derive(Debug, Clone)]
pub struct DownloadSpec {
    pub url: String,
    pub dir: PathBuf,
    pub file_name: String,
    pub expected_sha256: Option<String>,
}

impl DownloadSpec {
    /// Final path (`<dir>/<file>`) once the download completes.
    pub fn final_path(&self) -> PathBuf {
        self.dir.join(&self.file_name)
    }

    /// Partial path (`<dir>/<file>.part`) written during the transfer.
    pub fn part_path(&self) -> PathBuf {
        self.dir.join(format!("{}.part", self.file_name))
    }
}

/// The default models directory: `~/.config/itsjustcad/models`.
#[allow(dead_code)] // called by itsjustcad (app crate); invisible to download_model example
pub fn models_dir() -> Option<PathBuf> {
    Some(
        dirs::home_dir()?
            .join(".config")
            .join("itsjustcad")
            .join("models"),
    )
}

/// Resume offset for a fresh request: the size of an existing `.part`, or `0`.
///
/// Pure so the resume rule is unit-testable without a live server. A missing
/// file (or a stat error) yields `0` — we restart from the beginning.
pub fn resume_offset(part_path: &Path) -> u64 {
    std::fs::metadata(part_path).map(|m| m.len()).unwrap_or(0)
}

/// Format a byte count as a compact human string (`1.5 GB`, `812 KB`).
pub fn fmt_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

/// Format a transfer rate (`12.4 MB/s`). Sub-KB rates read as `B/s`.
pub fn fmt_speed(bytes_per_sec: f64) -> String {
    if !bytes_per_sec.is_finite() || bytes_per_sec <= 0.0 {
        return "—".to_string();
    }
    format!("{}/s", fmt_bytes(bytes_per_sec as u64))
}

/// A one-line progress caption for the UI: `42% · 1.2 GB / 2.9 GB · 12.4 MB/s`.
pub fn progress_caption(state: &DownloadState) -> String {
    match state {
        DownloadState::Idle => "Idle".to_string(),
        DownloadState::Downloading {
            done,
            total,
            bytes_per_sec,
        } => {
            let pct = state
                .fraction()
                .map(|f| format!("{}%", (f * 100.0).round() as u32))
                .unwrap_or_else(|| "…".to_string());
            let of = match total {
                Some(t) => format!("{} / {}", fmt_bytes(*done), fmt_bytes(*t)),
                None => fmt_bytes(*done),
            };
            format!("{pct} · {of} · {}", fmt_speed(*bytes_per_sec))
        }
        DownloadState::Verifying => "Verifying checksum…".to_string(),
        DownloadState::Done { .. } => "Done".to_string(),
        DownloadState::Failed { msg } => format!("Failed: {msg}"),
    }
}

/// SHA-256 of a byte buffer, as lower-case hex. Pure — used by tests against a
/// known vector and by the download example's verify step.
#[allow(dead_code)] // used by tests + the download_model example
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// Hash a file on disk in streaming fashion (constant memory) → lower-case hex.
pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Compare an actual hash against an expected one, case-insensitively. Returns
/// `Ok(())` when they match (or no expectation was set), else a human message.
/// Pure — the core of the verify step.
pub fn verify_sha(expected: Option<&str>, actual: &str) -> Result<(), String> {
    match expected {
        None => Ok(()),
        Some(exp) if exp.eq_ignore_ascii_case(actual) => Ok(()),
        Some(exp) => Err(format!(
            "checksum mismatch: expected {}, got {}",
            exp.to_lowercase(),
            actual
        )),
    }
}

/// Start a download on `handle`. Returns a [`Download`] the UI polls; the fetch
/// runs detached. Progress speed is a rolling average over ~1 s windows.
pub fn start(handle: &tokio::runtime::Handle, spec: DownloadSpec) -> Download {
    let state = Arc::new(Mutex::new(DownloadState::Downloading {
        done: 0,
        total: None,
        bytes_per_sec: 0.0,
    }));
    let cancel = Arc::new(AtomicBool::new(false));
    let dl = Download {
        state: state.clone(),
        cancel: cancel.clone(),
    };
    handle.spawn(async move {
        let result = run_fetch(spec, state.clone(), cancel).await;
        if let Err(msg) = result {
            // Only overwrite the state if we didn't already land on Done: a
            // cancel between the last chunk and rename should still read Failed.
            let mut guard = state.lock().expect("download state mutex");
            if !matches!(*guard, DownloadState::Done { .. }) {
                *guard = DownloadState::Failed { msg };
            }
        }
    });
    dl
}

/// The streaming fetch: resume-aware GET, chunk loop with cancel + speed, then
/// verify + rename. Errors bubble up as a short message the caller stores.
async fn run_fetch(
    spec: DownloadSpec,
    state: Arc<Mutex<DownloadState>>,
    cancel: Arc<AtomicBool>,
) -> Result<(), String> {
    use futures::StreamExt as _;
    use std::io::Write as _;

    std::fs::create_dir_all(&spec.dir).map_err(|e| format!("cannot create models dir: {e}"))?;
    let part_path = spec.part_path();
    let start_at = resume_offset(&part_path);

    let client = reqwest::Client::new();
    let mut req = client.get(&spec.url);
    if start_at > 0 {
        req = req.header(reqwest::header::RANGE, format!("bytes={start_at}-"));
    }
    let resp = req.send().await.map_err(|e| format!("request failed: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("server returned {status}"));
    }
    // If we asked to resume but the server ignored the Range (200 not 206), the
    // body is the WHOLE file — restart from zero, truncating the stale .part.
    let resuming = start_at > 0 && status == reqwest::StatusCode::PARTIAL_CONTENT;
    let base = if resuming { start_at } else { 0 };
    // Content-Length is the REMAINING length; add the resume base for the total.
    let total = resp.content_length().map(|len| base + len);

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(resuming)
        .truncate(!resuming)
        .open(&part_path)
        .map_err(|e| format!("cannot open .part: {e}"))?;

    let mut done = base;
    let mut stream = resp.bytes_stream();
    let mut window_start = std::time::Instant::now();
    let mut window_bytes: u64 = 0;
    let mut bytes_per_sec = 0.0;

    while let Some(chunk) = stream.next().await {
        if cancel.load(Ordering::SeqCst) {
            return Err("cancelled".to_string());
        }
        let chunk = chunk.map_err(|e| format!("stream error: {e}"))?;
        file.write_all(&chunk)
            .map_err(|e| format!("write error: {e}"))?;
        done += chunk.len() as u64;
        window_bytes += chunk.len() as u64;

        // Recompute the rolling speed roughly once a second.
        let elapsed = window_start.elapsed().as_secs_f64();
        if elapsed >= 1.0 {
            bytes_per_sec = window_bytes as f64 / elapsed;
            window_start = std::time::Instant::now();
            window_bytes = 0;
        }
        *state.lock().expect("download state mutex") = DownloadState::Downloading {
            done,
            total,
            bytes_per_sec,
        };
    }
    file.flush().map_err(|e| format!("flush error: {e}"))?;
    drop(file);

    // Verify the finished .part before promoting it.
    *state.lock().expect("download state mutex") = DownloadState::Verifying;
    if spec.expected_sha256.is_some() {
        let actual = sha256_file(&part_path).map_err(|e| format!("hashing failed: {e}"))?;
        verify_sha(spec.expected_sha256.as_deref(), &actual)?;
    }

    let final_path = spec.final_path();
    std::fs::rename(&part_path, &final_path).map_err(|e| format!("rename failed: {e}"))?;
    *state.lock().expect("download state mutex") = DownloadState::Done { path: final_path };
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    // ── resume-offset rule ─────────────────────────────────────────────────

    #[test]
    fn resume_offset_missing_file_is_zero() {
        let dir = std::env::temp_dir().join("ijc_dl_test_missing");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(resume_offset(&dir.join("nope.part")), 0);
    }

    #[test]
    fn resume_offset_reads_partial_size() {
        let dir = std::env::temp_dir().join("ijc_dl_test_resume");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("m.part");
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(&[0u8; 4096]).unwrap();
        drop(f);
        assert_eq!(resume_offset(&p), 4096);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // ── sha256 (known vectors) ─────────────────────────────────────────────

    #[test]
    fn sha256_of_empty_is_known_vector() {
        // The canonical SHA-256 of the empty string.
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn sha256_of_abc_is_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn sha256_file_matches_buffer() {
        let dir = std::env::temp_dir().join("ijc_dl_test_hashfile");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("blob.bin");
        let data = b"the quick brown fox";
        std::fs::write(&p, data).unwrap();
        assert_eq!(sha256_file(&p).unwrap(), sha256_hex(data));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // ── verify rule ────────────────────────────────────────────────────────

    #[test]
    fn verify_none_always_ok() {
        assert!(verify_sha(None, "anything").is_ok());
    }

    #[test]
    fn verify_matches_case_insensitively() {
        let h = sha256_hex(b"abc");
        assert!(verify_sha(Some(&h.to_uppercase()), &h).is_ok());
    }

    #[test]
    fn verify_mismatch_reports_both() {
        let err = verify_sha(Some("deadbeef"), "cafef00d").unwrap_err();
        assert!(err.contains("deadbeef"), "{err}");
        assert!(err.contains("cafef00d"), "{err}");
    }

    // ── formatting ─────────────────────────────────────────────────────────

    #[test]
    fn fmt_bytes_scales_units() {
        assert_eq!(fmt_bytes(512), "512 B");
        assert_eq!(fmt_bytes(2048), "2 KB");
        assert_eq!(fmt_bytes(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(fmt_bytes(3 * 1024 * 1024 * 1024), "3.0 GB");
    }

    #[test]
    fn fmt_speed_handles_zero_and_nan() {
        assert_eq!(fmt_speed(0.0), "—");
        assert_eq!(fmt_speed(f64::NAN), "—");
        assert_eq!(fmt_speed(-5.0), "—");
        assert_eq!(fmt_speed(1024.0), "1 KB/s");
    }

    // ── state / fraction ───────────────────────────────────────────────────

    #[test]
    fn fraction_half_way() {
        let s = DownloadState::Downloading {
            done: 50,
            total: Some(100),
            bytes_per_sec: 10.0,
        };
        assert_eq!(s.fraction(), Some(0.5));
    }

    #[test]
    fn fraction_unknown_total_is_none() {
        let s = DownloadState::Downloading {
            done: 50,
            total: None,
            bytes_per_sec: 10.0,
        };
        assert_eq!(s.fraction(), None);
    }

    #[test]
    fn fraction_done_is_full() {
        let s = DownloadState::Done {
            path: PathBuf::from("/tmp/m.gguf"),
        };
        assert_eq!(s.fraction(), Some(1.0));
    }

    #[test]
    fn fraction_clamps_over_100() {
        // A resumed download can momentarily report done > total if the server
        // total estimate lags; the fraction must not exceed 1.0.
        let s = DownloadState::Downloading {
            done: 120,
            total: Some(100),
            bytes_per_sec: 1.0,
        };
        assert_eq!(s.fraction(), Some(1.0));
    }

    #[test]
    fn is_active_only_while_running() {
        assert!(DownloadState::Verifying.is_active());
        assert!(DownloadState::Downloading {
            done: 0,
            total: None,
            bytes_per_sec: 0.0
        }
        .is_active());
        assert!(!DownloadState::Idle.is_active());
        assert!(!DownloadState::Done {
            path: PathBuf::from("/x")
        }
        .is_active());
        assert!(!DownloadState::Failed { msg: "x".into() }.is_active());
    }

    #[test]
    fn progress_caption_shows_pct_size_speed() {
        let s = DownloadState::Downloading {
            done: 1024 * 1024,
            total: Some(4 * 1024 * 1024),
            bytes_per_sec: 512.0 * 1024.0,
        };
        let cap = progress_caption(&s);
        assert!(cap.contains("25%"), "{cap}");
        assert!(cap.contains("1.0 MB"), "{cap}");
        assert!(cap.contains("4.0 MB"), "{cap}");
        assert!(cap.contains("KB/s"), "{cap}");
    }

    #[test]
    fn progress_caption_failed_shows_msg() {
        let s = DownloadState::Failed {
            msg: "cancelled".into(),
        };
        assert_eq!(progress_caption(&s), "Failed: cancelled");
    }

    // ── spec paths ─────────────────────────────────────────────────────────

    #[test]
    fn spec_paths_are_dir_joined() {
        let spec = DownloadSpec {
            url: "https://example/m.gguf".into(),
            dir: PathBuf::from("/models"),
            file_name: "m.gguf".into(),
            expected_sha256: None,
        };
        assert_eq!(spec.final_path(), PathBuf::from("/models/m.gguf"));
        assert_eq!(spec.part_path(), PathBuf::from("/models/m.gguf.part"));
    }
}
