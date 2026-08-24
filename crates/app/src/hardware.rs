// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Hardware capability probe for local-model onboarding.
//!
//! Zero heavy deps: RAM comes from `sysctl hw.memsize` (macOS) or
//! `/proc/meminfo` (Linux); cores from [`std::thread::available_parallelism`];
//! GPU/Metal presence is a best-effort per-OS guess; free disk on the config
//! volume is read from the platform stat call. The scoring policy in
//! [`recommend_tier`] is a pure function so it can be unit-tested exhaustively.

/// A model size class the machine can reasonably run locally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelTier {
    /// Not enough RAM for a comfortable local model — suggest cloud instead.
    None,
    /// ~3B-parameter model (quantised). Fits ~8 GB machines.
    Small3B,
    /// ~7B-parameter model (quantised). Wants 16 GB+.
    Mid7B,
}

impl ModelTier {
    /// Short human label for the onboarding UI.
    pub fn label(self) -> &'static str {
        match self {
            ModelTier::None => "Cloud recommended",
            ModelTier::Small3B => "3B local model",
            ModelTier::Mid7B => "7B local model",
        }
    }
}

/// Detected hardware facts. Fields are best-effort; `None`/`0` mean "unknown".
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HardwareInfo {
    /// Total physical RAM in GiB (rounded down), or `None` if undetectable.
    pub ram_gb: Option<u64>,
    /// Logical CPU cores.
    pub cores: usize,
    /// Whether a GPU/Metal accelerator is (best-effort) present.
    pub gpu: bool,
    /// Free space on the config volume in GiB, or `None` if undetectable.
    pub free_disk_gb: Option<u64>,
}

impl HardwareInfo {
    /// The recommended tier given the detected RAM and cores. When RAM is
    /// unknown we fall back to the smallest safe option.
    pub fn tier(&self) -> ModelTier {
        match self.ram_gb {
            Some(ram) => recommend_tier(ram, self.cores),
            None => ModelTier::Small3B,
        }
    }

    /// One-line recommendation sentence for the onboarding screen.
    pub fn recommendation(&self) -> String {
        let ram = self
            .ram_gb
            .map(|g| format!("{g} GB RAM"))
            .unwrap_or_else(|| "unknown RAM".to_string());
        match self.tier() {
            ModelTier::Mid7B => {
                format!("Detected {ram}, {} cores → a 7B model runs well.", self.cores)
            }
            ModelTier::Small3B => format!(
                "Detected {ram}, {} cores → use a 3B model (a 7B may be tight).",
                self.cores
            ),
            ModelTier::None => format!(
                "Detected {ram}, {} cores → too little RAM; use a 3B model or cloud.",
                self.cores
            ),
        }
    }
}

/// Pure scoring policy: pick a model tier from RAM (GiB) and core count.
///
/// Policy:
/// - `< 8 GB`  → [`ModelTier::None`] (warn; suggest a 3B or cloud).
/// - `8..16 GB` → [`ModelTier::Small3B`] (a 7B is possible but tight).
/// - `>= 16 GB` → [`ModelTier::Mid7B`].
///
/// `cores` is currently advisory (a 7B on very few cores is slow but works);
/// it is taken so callers can tighten the policy later without a signature
/// change.
pub fn recommend_tier(ram_gb: u64, _cores: usize) -> ModelTier {
    if ram_gb < 8 {
        ModelTier::None
    } else if ram_gb < 16 {
        ModelTier::Small3B
    } else {
        ModelTier::Mid7B
    }
}

/// Detect the current machine's hardware. Never panics; unknown fields are
/// left as `None`.
pub fn detect() -> HardwareInfo {
    HardwareInfo {
        ram_gb: detect_ram_gb(),
        cores: std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1),
        gpu: detect_gpu(),
        free_disk_gb: detect_free_disk_gb(),
    }
}

/// Total physical RAM in GiB (rounded down), or `None` on failure.
fn detect_ram_gb() -> Option<u64> {
    let bytes = detect_ram_bytes()?;
    Some(bytes / (1024 * 1024 * 1024))
}

#[cfg(target_os = "macos")]
fn detect_ram_bytes() -> Option<u64> {
    let out = std::process::Command::new("sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout).trim().parse::<u64>().ok()
}

#[cfg(target_os = "linux")]
fn detect_ram_bytes() -> Option<u64> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    parse_meminfo_total_bytes(&meminfo)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn detect_ram_bytes() -> Option<u64> {
    None
}

/// Parse `MemTotal:` (kB) out of a `/proc/meminfo` blob → bytes.
/// Public within the crate so the parser can be unit-tested on a sample string.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn parse_meminfo_total_bytes(meminfo: &str) -> Option<u64> {
    for line in meminfo.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            // "MemTotal:       16327584 kB"
            let mut parts = rest.split_whitespace();
            let kb: u64 = parts.next()?.parse().ok()?;
            return Some(kb * 1024);
        }
    }
    None
}

#[cfg(target_os = "macos")]
fn detect_gpu() -> bool {
    // Every Mac ships a Metal-capable GPU (Apple Silicon or discrete/Intel).
    true
}

#[cfg(not(target_os = "macos"))]
fn detect_gpu() -> bool {
    // Best-effort on Linux/other: presence of a DRM render node.
    std::path::Path::new("/dev/dri").exists()
}

/// Free bytes on the config volume, in GiB. Uses `df` (portable) so we stay
/// zero-dep; returns `None` if the command or parse fails.
fn detect_free_disk_gb() -> Option<u64> {
    let target = dirs::home_dir()?.join(".config");
    let path = if target.exists() {
        target
    } else {
        dirs::home_dir()?
    };
    let out = std::process::Command::new("df")
        .arg("-k")
        .arg(&path)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    parse_df_avail_gb(&text)
}

/// Parse the "Available" (4th) column, in 1K blocks, from `df -k` output → GiB.
/// Handles the header line plus a data line that may wrap.
pub(crate) fn parse_df_avail_gb(df_output: &str) -> Option<u64> {
    let mut lines = df_output.lines();
    let header = lines.next()?;
    // Find the index of the "Avail"/"Available" column in the header.
    let avail_idx = header
        .split_whitespace()
        .position(|h| h.starts_with("Avail"))
        .unwrap_or(3);
    // Data may span two physical lines when the filesystem name is long; join
    // the remainder and take the numeric fields after it.
    let rest: String = lines.collect::<Vec<_>>().join(" ");
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // The filesystem field may be non-numeric; locate numeric fields.
    // Simplest robust approach: take the field at `avail_idx` if numeric,
    // else scan for the Nth numeric field.
    if let Some(Ok(kb)) = fields.get(avail_idx).map(|f| f.parse::<u64>()) {
        return Some(kb / (1024 * 1024));
    }
    // Fallback: numeric columns only (filesystem name dropped).
    let nums: Vec<u64> = fields.iter().filter_map(|f| f.parse::<u64>().ok()).collect();
    // Columns after the fs name: [1k-blocks, used, avail, ...] → avail is idx 2.
    nums.get(2).map(|kb| kb / (1024 * 1024))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── recommend_tier policy ──────────────────────────────────────────────

    #[test]
    fn tier_under_8gb_is_none() {
        assert_eq!(recommend_tier(0, 4), ModelTier::None);
        assert_eq!(recommend_tier(4, 8), ModelTier::None);
        assert_eq!(recommend_tier(7, 16), ModelTier::None);
    }

    #[test]
    fn tier_8_to_16gb_is_small3b() {
        assert_eq!(recommend_tier(8, 4), ModelTier::Small3B);
        assert_eq!(recommend_tier(12, 8), ModelTier::Small3B);
        assert_eq!(recommend_tier(15, 8), ModelTier::Small3B);
    }

    #[test]
    fn tier_16gb_plus_is_mid7b() {
        assert_eq!(recommend_tier(16, 8), ModelTier::Mid7B);
        assert_eq!(recommend_tier(32, 10), ModelTier::Mid7B);
        assert_eq!(recommend_tier(128, 24), ModelTier::Mid7B);
    }

    #[test]
    fn tier_boundaries_are_inclusive_correctly() {
        // 8 is the first Small3B, 16 is the first Mid7B.
        assert_eq!(recommend_tier(8, 1), ModelTier::Small3B);
        assert_eq!(recommend_tier(16, 1), ModelTier::Mid7B);
    }

    // ── /proc/meminfo parser ───────────────────────────────────────────────

    #[test]
    fn parse_meminfo_reads_memtotal() {
        let sample = "MemTotal:       16327584 kB\n\
                      MemFree:         1234567 kB\n\
                      MemAvailable:    8000000 kB\n";
        let bytes = parse_meminfo_total_bytes(sample).unwrap();
        assert_eq!(bytes, 16327584u64 * 1024);
        // 16327584 kB ≈ 15.5 GiB → floors to 15.
        assert_eq!(bytes / (1024 * 1024 * 1024), 15);
    }

    #[test]
    fn parse_meminfo_missing_returns_none() {
        assert!(parse_meminfo_total_bytes("MemFree: 100 kB\n").is_none());
        assert!(parse_meminfo_total_bytes("").is_none());
    }

    #[test]
    fn parse_meminfo_garbage_value_returns_none() {
        assert!(parse_meminfo_total_bytes("MemTotal: notanumber kB").is_none());
    }

    // ── df parser ──────────────────────────────────────────────────────────

    #[test]
    fn parse_df_reads_avail_column() {
        // macOS-style df -k output.
        let sample = "Filesystem   1024-blocks      Used Available Capacity  Mounted on\n\
                      /dev/disk1s1   488245288 200000000 288245288    41%    /\n";
        let gb = parse_df_avail_gb(sample).unwrap();
        // 288245288 kB / 1024 / 1024 ≈ 274 GiB.
        assert_eq!(gb, 288245288u64 / (1024 * 1024));
    }

    #[test]
    fn parse_df_linux_style() {
        let sample = "Filesystem     1K-blocks      Used Available Use% Mounted on\n\
                      /dev/sda1      103081248  50000000  53081248  49% /\n";
        let gb = parse_df_avail_gb(sample).unwrap();
        assert_eq!(gb, 53081248u64 / (1024 * 1024));
    }

    // ── HardwareInfo derived output ────────────────────────────────────────

    #[test]
    fn hardware_tier_falls_back_to_small_when_ram_unknown() {
        let hw = HardwareInfo {
            ram_gb: None,
            cores: 8,
            gpu: true,
            free_disk_gb: None,
        };
        assert_eq!(hw.tier(), ModelTier::Small3B);
    }

    #[test]
    fn recommendation_mentions_ram_and_cores() {
        let hw = HardwareInfo {
            ram_gb: Some(16),
            cores: 10,
            gpu: true,
            free_disk_gb: Some(200),
        };
        let msg = hw.recommendation();
        assert!(msg.contains("16 GB"), "{msg}");
        assert!(msg.contains("10 cores"), "{msg}");
        assert!(msg.contains("7B"), "{msg}");
    }

    #[test]
    fn recommendation_warns_on_low_ram() {
        let hw = HardwareInfo {
            ram_gb: Some(4),
            cores: 4,
            gpu: false,
            free_disk_gb: Some(50),
        };
        let msg = hw.recommendation();
        assert!(msg.to_lowercase().contains("cloud"), "{msg}");
    }

    #[test]
    fn detect_never_panics() {
        // Smoke test: real detection on the test host must not panic and must
        // report at least one core.
        let hw = detect();
        assert!(hw.cores >= 1);
    }
}
