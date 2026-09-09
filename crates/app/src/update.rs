// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! In-app update check against GitHub Releases (M-autoupdate).
//!
//! OFFLINE-first, like the basemap: the app never auto-polls on launch by
//! default — the network is reached ONLY when the user picks **Help ▸ Check for
//! Updates…** (or opts into the launch check via `ui.json`). The single endpoint
//! contacted is `api.github.com` (public, no auth); GitHub requires a
//! `User-Agent`, so we send one.
//!
//! The design mirrors [`crate::download`]: the HTTPS fetch runs on the app's
//! tokio runtime as a detached task; the UI polls a shared [`UpdateState`]
//! (behind an `Arc<Mutex>`) each frame and renders it. Nothing blocks the paint
//! thread.
//!
//! The two decision functions — [`is_newer`] (semver compare, `v`-prefix
//! tolerant) and [`parse_latest_release`] (extract tag / html_url / asset url
//! from the releases JSON) — are PURE and unit-tested with no network.
//!
//! UNSIGNED CAVEAT: ItsJustCAD is not code-signed / notarized yet, so we do NOT
//! attempt a silent in-place binary swap of the running app (Gatekeeper would
//! quarantine it and the swap is fragile). The honest unsigned behaviour is
//! **download-and-reveal**: we open the release page in the browser and surface
//! the Gatekeeper note. Seamless install is deferred to M-sign.

use std::sync::{Arc, Mutex};

/// This build's version, baked at compile time from the crate's Cargo version
/// (`0.4.0`). The single source of truth the update check compares against.
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The GitHub "latest release" endpoint for this repo. Public, no auth.
pub const RELEASES_LATEST_URL: &str =
    "https://api.github.com/repos/htarrido-picart/itsjustcad/releases/latest";

/// User-Agent sent with the API request. GitHub REJECTS requests without one.
const USER_AGENT: &str = "ItsJustCAD-UpdateCheck (+https://github.com/htarrido-picart/itsjustcad)";

/// The result of parsing a GitHub `releases/latest` payload: the release tag
/// (e.g. `v0.4.1`), the human release page (`html_url`), and the first asset's
/// direct download URL if the release ships one. Pure data — no network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LatestRelease {
    /// The release tag, e.g. `"v0.4.1"`. May or may not carry a `v` prefix.
    pub tag: String,
    /// The GitHub release page URL (what the Download button opens by default).
    pub html_url: String,
    /// The first release asset's `browser_download_url`, if the release has one.
    pub asset_url: Option<String>,
}

/// The live state the UI polls each frame. Mirrors [`crate::download`]'s
/// `DownloadState`: an idle/checking/result machine behind an `Arc<Mutex>`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum UpdateState {
    /// No check has been run this session (initial).
    #[default]
    Idle,
    /// A check is in flight (network request running on the tokio runtime).
    Checking,
    /// Check succeeded and the current build is the newest. Carries the latest
    /// (which equals the running version, up to `v`-prefix normalization).
    UpToDate { latest: String },
    /// A newer release exists. Carries the parsed release for the popup to show
    /// current → latest and offer the Download button.
    Available { release: LatestRelease },
    /// The check failed (offline, rate-limited, DNS, malformed JSON). `msg` is a
    /// short human reason surfaced in the popup — never a panic, never silent.
    Failed { msg: String },
}

/// A running (or finished) update check the UI holds onto: the shared state to
/// poll each frame.
pub struct UpdateCheck {
    state: Arc<Mutex<UpdateState>>,
}

impl UpdateCheck {
    /// Read the current state (cloned so the lock is held only briefly).
    pub fn state(&self) -> UpdateState {
        self.state.lock().expect("update state mutex").clone()
    }
}

/// Kick off an update check on the tokio runtime. Returns immediately with an
/// [`UpdateCheck`] whose state starts at [`UpdateState::Checking`]; the detached
/// task fetches `releases/latest`, parses it, compares against [`APP_VERSION`],
/// and stores the terminal state. Never blocks the caller / the paint thread.
pub fn start(handle: &tokio::runtime::Handle) -> UpdateCheck {
    let state = Arc::new(Mutex::new(UpdateState::Checking));
    let state_bg = state.clone();
    handle.spawn(async move {
        let result = fetch_latest().await;
        let next = match result {
            Ok(release) => {
                if is_newer(APP_VERSION, &release.tag) {
                    UpdateState::Available { release }
                } else {
                    UpdateState::UpToDate { latest: release.tag }
                }
            }
            Err(msg) => UpdateState::Failed { msg },
        };
        *state_bg.lock().expect("update state mutex") = next;
    });
    UpdateCheck { state }
}

/// Perform the actual HTTPS GET + JSON parse. Reuses the app's existing `reqwest`
/// stack (rustls-tls, no new HTTP dependency). A short timeout keeps a stalled
/// server from leaving the popup on "Checking…" forever. Errors are mapped to
/// short human strings — the caller surfaces them, never panics.
async fn fetch_latest() -> Result<LatestRelease, String> {
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .get(RELEASES_LATEST_URL)
        // GitHub recommends pinning the API version + JSON accept header.
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|_| "offline or GitHub unavailable".to_string())?;
    let status = resp.status();
    if !status.is_success() {
        // 403 is GitHub's rate-limit response; other codes get a generic note.
        if status.as_u16() == 403 {
            return Err("GitHub rate limit reached — try again later".to_string());
        }
        return Err(format!("GitHub returned {status}"));
    }
    let body = resp.text().await.map_err(|e| e.to_string())?;
    parse_latest_release(&body)
}

/// Parse a GitHub `releases/latest` JSON body into a [`LatestRelease`]. PURE —
/// no network, unit-tested with a sample payload. Extracts `tag_name`,
/// `html_url`, and the first `assets[].browser_download_url` (if present). A
/// missing/blank `tag_name` is an error (nothing to compare against).
pub fn parse_latest_release(body: &str) -> Result<LatestRelease, String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("bad JSON from GitHub: {e}"))?;
    let tag = v
        .get("tag_name")
        .and_then(|t| t.as_str())
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| "release has no tag_name".to_string())?;
    let html_url = v
        .get("html_url")
        .and_then(|u| u.as_str())
        .unwrap_or("https://github.com/htarrido-picart/itsjustcad/releases")
        .to_string();
    let asset_url = v
        .get("assets")
        .and_then(|a| a.as_array())
        .and_then(|arr| arr.first())
        .and_then(|first| first.get("browser_download_url"))
        .and_then(|u| u.as_str())
        .map(str::to_string);
    Ok(LatestRelease {
        tag,
        html_url,
        asset_url,
    })
}

/// Split a version string into numeric `(major, minor, patch)`, tolerating a
/// leading `v`/`V` and ignoring any pre-release/build suffix after the patch
/// (`0.4.1-beta` → `(0,4,1)`). Missing components default to 0 (`1` → `(1,0,0)`).
/// Non-numeric components are treated as 0. Pure helper for [`is_newer`].
fn parse_semver(s: &str) -> (u64, u64, u64) {
    let s = s.trim();
    let s = s.strip_prefix(['v', 'V']).unwrap_or(s);
    // Drop any pre-release / build metadata (`-rc1`, `+build`).
    let core = s.split(['-', '+']).next().unwrap_or(s);
    let mut it = core.split('.');
    let part = |o: Option<&str>| o.and_then(|p| p.trim().parse::<u64>().ok()).unwrap_or(0);
    (part(it.next()), part(it.next()), part(it.next()))
}

/// True when `latest` is a strictly newer semantic version than `current`.
/// Tolerates a `v` prefix on either side and compares major, then minor, then
/// patch. Equal versions return `false` (up to date). PURE — the core of the
/// update decision, unit-tested across patch/minor/major bumps.
pub fn is_newer(current: &str, latest: &str) -> bool {
    parse_semver(latest) > parse_semver(current)
}

/// A URL is safe to hand to a browser helper only if it is HTTPS and free of
/// characters that the Windows `cmd /C start` shell would interpret (`&`, `^`,
/// `|`, `<`, `>`, `"`, `%`, whitespace). The release/asset URLs we open come
/// from the GitHub API JSON — validated, not trusted — so a tampered response
/// can't turn a "download" field into a shell command. Legit GitHub URLs pass.
pub fn is_safe_browser_url(url: &str) -> bool {
    url.starts_with("https://")
        && !url
            .chars()
            .any(|c| c.is_whitespace() || "&^|<>\"%".contains(c))
}

/// Open a URL in the user's default browser. Best-effort — a failure to spawn
/// the helper is silently ignored (nothing to recover). Mirrors the app's
/// existing `Command::new("open")` reveal helpers, gated per-platform. Rejects
/// any non-HTTPS or shell-unsafe URL (see [`is_safe_browser_url`]).
pub fn open_url(url: &str) {
    if !is_safe_browser_url(url) {
        return;
    }
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("cmd")
        .args(["/C", "start", "", url])
        .spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semver_ordering_patch_minor_major() {
        assert!(is_newer("0.4.0", "0.4.1"), "patch bump is newer");
        assert!(is_newer("0.4.1", "0.5.0"), "minor bump is newer");
        assert!(is_newer("0.5.0", "1.0.0"), "major bump is newer");
        assert!(is_newer("0.4.0", "1.0.0"), "big jump is newer");
    }

    #[test]
    fn browser_url_guard_rejects_unsafe() {
        // Legit GitHub URLs pass.
        assert!(is_safe_browser_url(
            "https://github.com/owner/itsjustcad/releases/tag/v0.5.0"
        ));
        assert!(is_safe_browser_url(
            "https://github.com/owner/itsjustcad/releases/download/v0.5.0/app.zip"
        ));
        // Non-HTTPS is refused.
        assert!(!is_safe_browser_url("http://github.com/x"));
        assert!(!is_safe_browser_url("file:///etc/passwd"));
        // Shell-metachar injection (Windows `cmd /C start`) is refused.
        assert!(!is_safe_browser_url(
            "https://github.com/x & calc.exe"
        ));
        assert!(!is_safe_browser_url("https://github.com/x\"^|<>"));
    }

    #[test]
    fn equal_version_is_not_newer() {
        assert!(!is_newer("0.4.0", "0.4.0"));
        // v-prefix on either/both sides normalizes to equal.
        assert!(!is_newer("0.4.0", "v0.4.0"));
        assert!(!is_newer("v0.4.0", "0.4.0"));
        assert!(!is_newer("v0.4.0", "v0.4.0"));
    }

    #[test]
    fn older_latest_is_not_newer() {
        assert!(!is_newer("0.4.1", "0.4.0"), "downgrade is not newer");
        assert!(!is_newer("1.0.0", "0.9.9"));
        assert!(!is_newer("0.5.0", "0.4.9"));
    }

    #[test]
    fn v_prefix_tolerated_both_directions() {
        assert!(is_newer("v0.4.0", "v0.4.1"));
        assert!(is_newer("0.4.0", "v0.4.1"));
        assert!(is_newer("v0.4.0", "0.4.1"));
    }

    #[test]
    fn prerelease_suffix_ignored_to_patch() {
        // We compare on the numeric core only; suffixes don't make it newer.
        assert_eq!(parse_semver("0.4.1-beta"), (0, 4, 1));
        assert_eq!(parse_semver("v0.4.1+build.7"), (0, 4, 1));
        assert!(!is_newer("0.4.1", "0.4.1-rc1"));
    }

    #[test]
    fn short_versions_default_missing_components_to_zero() {
        assert_eq!(parse_semver("1"), (1, 0, 0));
        assert_eq!(parse_semver("1.2"), (1, 2, 0));
        assert!(is_newer("0.4.0", "1"));
        assert!(!is_newer("1.0.0", "1"));
    }

    #[test]
    fn app_version_is_baked_from_cargo() {
        // Guards the version source: the constant must equal the crate version.
        assert_eq!(APP_VERSION, env!("CARGO_PKG_VERSION"));
        // Sanity: it parses as a real semver triple.
        let (maj, min, _pat) = parse_semver(APP_VERSION);
        assert!(maj > 0 || min > 0, "APP_VERSION should be non-zero: {APP_VERSION}");
    }

    // ── JSON parse (pure, no network) ────────────────────────────────────────

    const SAMPLE: &str = r#"{
        "tag_name": "v0.4.1",
        "name": "ItsJustCAD 0.4.1",
        "html_url": "https://github.com/htarrido-picart/itsjustcad/releases/tag/v0.4.1",
        "body": "Bug fixes and a shiny update checker.",
        "assets": [
            {
                "name": "ItsJustCAD.dmg",
                "browser_download_url": "https://github.com/htarrido-picart/itsjustcad/releases/download/v0.4.1/ItsJustCAD.dmg"
            },
            {
                "name": "checksums.txt",
                "browser_download_url": "https://github.com/htarrido-picart/itsjustcad/releases/download/v0.4.1/checksums.txt"
            }
        ]
    }"#;

    #[test]
    fn parses_tag_url_and_first_asset() {
        let r = parse_latest_release(SAMPLE).expect("valid release JSON");
        assert_eq!(r.tag, "v0.4.1");
        assert_eq!(
            r.html_url,
            "https://github.com/htarrido-picart/itsjustcad/releases/tag/v0.4.1"
        );
        assert_eq!(
            r.asset_url.as_deref(),
            Some("https://github.com/htarrido-picart/itsjustcad/releases/download/v0.4.1/ItsJustCAD.dmg"),
            "picks the FIRST asset's download url"
        );
    }

    #[test]
    fn parses_release_with_no_assets() {
        let body = r#"{
            "tag_name": "v0.5.0",
            "html_url": "https://example.com/r/0.5.0",
            "assets": []
        }"#;
        let r = parse_latest_release(body).expect("valid");
        assert_eq!(r.tag, "v0.5.0");
        assert_eq!(r.asset_url, None, "no assets ⇒ no asset url, still parses");
    }

    #[test]
    fn missing_tag_is_an_error() {
        let body = r#"{ "html_url": "https://example.com", "assets": [] }"#;
        assert!(parse_latest_release(body).is_err());
        // Blank tag is also rejected.
        let blank = r#"{ "tag_name": "  ", "assets": [] }"#;
        assert!(parse_latest_release(blank).is_err());
    }

    #[test]
    fn malformed_json_is_an_error_not_a_panic() {
        assert!(parse_latest_release("not json at all").is_err());
        assert!(parse_latest_release("").is_err());
    }

    #[test]
    fn end_to_end_parse_then_compare() {
        // The exact flow the background task runs, minus the network. Uses a
        // fixed baseline (not APP_VERSION) so a workspace version bump can never
        // flip this assertion — the sample release tag is v0.4.1.
        let r = parse_latest_release(SAMPLE).unwrap();
        assert!(is_newer("0.4.0", &r.tag), "0.4.0 build < v0.4.1 release");
    }
}
