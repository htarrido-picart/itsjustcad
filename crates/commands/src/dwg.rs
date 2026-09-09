// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Assisted DWG import: detect a user-installed `dwg2dxf` (LibreDWG) binary and
//! shell out to it to convert ONE referenced file into a temp `.dxf`, then feed
//! the existing hardened DXF importer.
//!
//! ## Licensing (why detect-and-shell-out, never link)
//! LibreDWG is **GPLv3**. Linking or bundling it would relicense this AGPLv3
//! app's distribution terms. So we treat `dwg2dxf` exactly like the LLM CLIs:
//! an *external, user-installed* binary we invoke by absolute path. There is no
//! cargo dependency on LibreDWG, and nothing here links it.
//!
//! ## Why we do NOT trust the exit code (the 000-BG.dwg finding)
//! LibreDWG 0.13.3 exits 0 on a *broken* conversion of an AutoCAD-2013
//! Architectural-Desktop DWG: it drops the whole ENTITIES section (thousands of
//! hard errors, unstable AEC proxy classes) yet returns success. So after the
//! conversion we independently validate the output DXF has both an ENTITIES
//! section AND an EOF marker before importing — otherwise a truncated conversion
//! would masquerade as a valid-but-empty import. See [`validate_dxf_complete`].

use std::path::{Path, PathBuf};

/// The directories we probe for the `dwg2dxf` (LibreDWG) binary, in priority
/// order. `~/.local/bin` is probed FIRST: a user who builds a newer LibreDWG
/// from source installs it there (the Homebrew bottles lag, and the 0.13.3 that
/// Homebrew ships silently drops the ENTITIES section on AutoCAD-2013 AEC/ADT
/// files — see module docs). A source-built `dwg2dxf` links its own libredwg
/// dylib by absolute path, so preferring `~/.local/bin` is self-contained and
/// lets the newer converter win over a stale `/usr/local/bin` copy. After that
/// we fall back to the Homebrew targets (Intel + Apple Silicon) and `/usr/bin`.
/// Mirrors the LLM-CLI resolver's stance: a Finder-launched `.app` inherits a
/// stripped `PATH`, so absolute probes rescue the lookup.
///
/// Pure (modulo `$HOME`) so the search set is unit-testable.
pub fn dwg2dxf_search_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        dirs.push(home.join(".local").join("bin"));
    }
    dirs.extend([
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/bin"),
    ]);
    dirs
}

/// Resolve the absolute path to the `dwg2dxf` CLI, or `None` if it can't be
/// found. Checks the well-known install dirs first (the fix for a stripped
/// Finder `PATH`), then falls back to the inherited `PATH`.
pub fn resolve_dwg2dxf() -> Option<PathBuf> {
    resolve_dwg2dxf_in(&dwg2dxf_search_dirs(), std::env::var("PATH").ok())
}

/// Pure core of [`resolve_dwg2dxf`]: given candidate dirs and an optional `PATH`
/// string, return the first existing `dwg2dxf` binary. Injected inputs keep the
/// resolution rule unit-testable without touching the real environment.
pub fn resolve_dwg2dxf_in(search_dirs: &[PathBuf], path: Option<String>) -> Option<PathBuf> {
    for dir in search_dirs {
        let cand = dir.join("dwg2dxf");
        if cand.is_file() {
            return Some(cand);
        }
    }
    if let Some(path) = path {
        for dir in std::env::split_paths(&path) {
            let cand = dir.join("dwg2dxf");
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    None
}

/// Validate that a converted DXF is COMPLETE, not silently truncated.
///
/// LibreDWG can exit 0 while emitting a DXF that lacks its entities (see the
/// module docs). A genuine DXF the importer can use must have BOTH:
/// - an `ENTITIES` section header (`2\nENTITIES`), and
/// - a terminating `EOF` marker (`0\nEOF`).
///
/// Returns `Ok(())` when both are present, else `Err` with the clear
/// converter-too-old message. Tolerant of CRLF and trailing whitespace.
pub fn validate_dxf_complete(dxf: &str) -> Result<(), String> {
    let has_entities = dxf_has_pair(dxf, 2, "ENTITIES");
    let has_eof = dxf_has_pair(dxf, 0, "EOF");
    if has_entities && has_eof {
        Ok(())
    } else {
        Err(
            "DWG conversion incomplete (converter too old or unsupported DWG — try a newer \
             LibreDWG/ODA)"
                .to_string(),
        )
    }
}

/// True when the DXF text contains the group-code/value pair `code`\n`value`
/// anywhere (a group-code line followed immediately by the value line). Trims
/// each line so CRLF endings and stray spaces don't defeat the match.
fn dxf_has_pair(dxf: &str, code: i32, value: &str) -> bool {
    let mut lines = dxf.lines();
    while let Some(line) = lines.next() {
        if line.trim().parse::<i32>() == Ok(code)
            && lines.clone().next().is_some_and(|next| next.trim() == value)
        {
            return true;
        }
    }
    false
}

/// A successful conversion plus any SOFT warnings LibreDWG reported on stderr.
///
/// LibreDWG exits 0 and produces a COMPLETE DXF (ENTITIES + EOF) while still
/// logging thousands of `ERROR`/warning lines for AEC proxy classes and other
/// imperfect geometry (the 000-BG.dwg ADT finding: 2694 errors, valid output).
/// Those are not hard failures — the drawing imports — but the user must be told
/// the result may be imperfect. So we return the DXF text alongside a
/// human-readable warning summary (empty when the converter was silent).
#[derive(Debug, Clone)]
pub struct Converted {
    /// The complete, validated DXF text ready for the importer.
    pub dxf: String,
    /// Soft-warning lines to surface (e.g. "LibreDWG reported N warnings…").
    /// Empty ⇒ a clean conversion with nothing to warn about.
    pub warnings: Vec<String>,
}

/// Count the ERROR/warning lines LibreDWG emitted on stderr. Pure so the
/// stderr-parse rule is unit-testable without spawning a converter.
///
/// LibreDWG prefixes real problems with `ERROR` (uppercase) and lesser notes
/// with `Warning`/`warning`; we count any line mentioning either. Case- and
/// leading-whitespace-insensitive; blank lines never count.
pub fn count_libredwg_warnings(stderr: &str) -> usize {
    stderr
        .lines()
        .filter(|line| {
            let l = line.to_ascii_lowercase();
            l.contains("error") || l.contains("warning")
        })
        .count()
}

/// Build the soft-warning summary lines for a successful conversion given the
/// converter's stderr. Pure + tested. Returns an empty vec when the converter
/// was silent (a clean conversion), else a single count line the popup shows.
pub fn conversion_warnings(stderr: &str) -> Vec<String> {
    let n = count_libredwg_warnings(stderr);
    if n == 0 {
        Vec::new()
    } else {
        vec![format!(
            "LibreDWG reported {n} warning(s); some entities may be imperfect."
        )]
    }
}

/// Convert `input` (a `.dwg` path) to DXF text via a resolved `dwg2dxf` binary,
/// validating the result is complete before returning it, and capturing any
/// soft warnings the converter logged (see [`Converted`]).
///
/// SECURITY: the argument vector is FIXED — `dwg2dxf -o <tmp>.dxf <input>` — with
/// no shell, no string interpolation, and no caller-controlled flags. Only the
/// one referenced file is touched; the temp output is written into a fresh temp
/// dir and cleaned up before return (on every path).
///
/// - Absent `dwg2dxf` → clear "install LibreDWG" error.
/// - Converter exits non-zero → surfaces its stderr.
/// - Converter exits 0 but the DXF is truncated → the truncation error.
/// - Converter exits 0 with a complete DXF but noisy stderr → success WITH
///   warnings (never a failure).
pub fn convert_dwg_to_dxf(input: &str) -> Result<Converted, String> {
    let bin = resolve_dwg2dxf().ok_or_else(|| {
        "install LibreDWG to import DWG (brew install libredwg)".to_string()
    })?;
    convert_dwg_to_dxf_with(&bin, input)
}

/// Core conversion against an explicit converter binary — lets a test inject a
/// stub `dwg2dxf` that writes a known (valid or truncated) DXF.
pub fn convert_dwg_to_dxf_with(bin: &Path, input: &str) -> Result<Converted, String> {
    if !Path::new(input).is_file() {
        return Err(format!("cannot read DWG '{input}' (no such file)"));
    }
    // A fresh, per-invocation temp dir so concurrent imports never collide and
    // cleanup is a single directory removal.
    let tmp_dir = std::env::temp_dir().join(format!(
        "itsjustcad_dwg_{}_{}",
        std::process::id(),
        unique_tag()
    ));
    std::fs::create_dir_all(&tmp_dir)
        .map_err(|e| format!("cannot create temp dir for DWG conversion: {e}"))?;
    let out_dxf = tmp_dir.join("converted.dxf");

    // Absolutize the input path so a filename beginning with '-' can never be
    // parsed by dwg2dxf as an option (argument injection). We already verified
    // it is a real file above; canonicalize resolves it to a leading-slash path.
    let input_abs = std::fs::canonicalize(input).unwrap_or_else(|_| PathBuf::from(input));

    // FIXED args — no shell, no interpolation, no model-controlled flags.
    let status = std::process::Command::new(bin)
        .arg("-o")
        .arg(&out_dxf)
        .arg(&input_abs)
        .output();

    let result = (|| {
        let output = status.map_err(|e| format!("failed to run dwg2dxf: {e}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!(
                "dwg2dxf failed to convert '{input}': {}",
                stderr.trim()
            ));
        }
        // Exit 0 — but LibreDWG still logs soft warnings/errors for imperfect
        // AEC/proxy geometry. Capture them; they do NOT fail the import.
        let warnings = conversion_warnings(&String::from_utf8_lossy(&output.stderr));
        // Even on exit 0 the file may be missing or truncated — read then verify.
        //
        // Read as BYTES, not `read_to_string`: a real AutoCAD DWG carries binary
        // payloads (preview thumbnails, ACAD proxy/BINARY-chunk group codes) that
        // LibreDWG faithfully emits into the ASCII DXF, so the file is valid ASCII
        // DXF *interleaved with* non-UTF-8 bytes. `read_to_string` would reject
        // the whole 200 MB file on the first stray byte and — worse — that error
        // was mapped to "conversion incomplete", making a COMPLETE conversion of
        // a big ADT file look truncated. We decode lossily: the DXF grammar the
        // importer parses is ASCII group codes, and the only bytes replaced are
        // inside binary blobs the importer already skips.
        let bytes = std::fs::read(&out_dxf).map_err(|_| {
            "DWG conversion incomplete (converter too old or unsupported DWG — try a newer \
             LibreDWG/ODA)"
                .to_string()
        })?;
        let dxf = String::from_utf8_lossy(&bytes).into_owned();
        validate_dxf_complete(&dxf)?;
        Ok(Converted { dxf, warnings })
    })();

    // Clean up the temp dir on EVERY path (success or error).
    let _ = std::fs::remove_dir_all(&tmp_dir);
    result
}

/// A short, process-unique tag for temp paths. Uses the nanosecond clock; the
/// enclosing dir is already namespaced by pid, so this only needs to separate
/// two conversions within the same process.
fn unique_tag() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ijc_dwg_{tag}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn resolves_dwg2dxf_from_a_search_dir() {
        let bindir = tmp("resolve");
        let stub = bindir.join("dwg2dxf");
        fs::write(&stub, b"#!/bin/sh\n").unwrap();
        let stripped = "/usr/bin:/bin".to_string();
        let got = resolve_dwg2dxf_in(std::slice::from_ref(&bindir), Some(stripped));
        assert_eq!(got, Some(stub));
        fs::remove_dir_all(&bindir).unwrap();
    }

    #[test]
    fn resolves_dwg2dxf_from_path_fallback() {
        let onpath = tmp("path");
        let stub = onpath.join("dwg2dxf");
        fs::write(&stub, b"x").unwrap();
        let got = resolve_dwg2dxf_in(
            &[PathBuf::from("/nonexistent-xyz")],
            Some(onpath.to_string_lossy().into_owned()),
        );
        assert_eq!(got, Some(stub));
        fs::remove_dir_all(&onpath).unwrap();
    }

    #[test]
    fn none_when_dwg2dxf_absent_everywhere() {
        let got = resolve_dwg2dxf_in(
            &[PathBuf::from("/nonexistent-abc")],
            Some("/also/nope".to_string()),
        );
        assert_eq!(got, None);
    }

    #[test]
    fn search_dirs_include_homebrew_targets() {
        let dirs = dwg2dxf_search_dirs();
        assert!(dirs.contains(&PathBuf::from("/usr/local/bin")), "{dirs:?}");
        assert!(dirs.contains(&PathBuf::from("/opt/homebrew/bin")), "{dirs:?}");
    }

    #[test]
    fn search_dirs_prefer_local_bin_over_system() {
        // A source-built newer LibreDWG installs into ~/.local/bin; it must win
        // over a stale /usr/local/bin (Homebrew 0.13.3) copy that mangles ADT
        // files. So ~/.local/bin must be probed BEFORE the system dirs.
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            let dirs = dwg2dxf_search_dirs();
            let local = home.join(".local").join("bin");
            let local_idx = dirs.iter().position(|d| *d == local);
            let usrlocal_idx = dirs.iter().position(|d| *d == PathBuf::from("/usr/local/bin"));
            assert!(local_idx.is_some(), "{dirs:?}");
            assert!(usrlocal_idx.is_some(), "{dirs:?}");
            assert!(local_idx < usrlocal_idx, "~/.local/bin must precede /usr/local/bin: {dirs:?}");
        }
    }

    #[test]
    fn complete_dxf_passes_validation() {
        let dxf = "0\nSECTION\n2\nENTITIES\n0\nLINE\n0\nENDSEC\n0\nEOF\n";
        assert!(validate_dxf_complete(dxf).is_ok());
    }

    #[test]
    fn dxf_missing_entities_is_rejected() {
        // Header + tables + EOF but no ENTITIES section — the exact LibreDWG
        // 0.13.3 truncation shape (exit 0, no drawing).
        let dxf = "0\nSECTION\n2\nHEADER\n0\nENDSEC\n0\nEOF\n";
        let err = validate_dxf_complete(dxf).unwrap_err();
        assert!(err.contains("conversion incomplete"), "{err}");
        assert!(err.contains("newer"), "{err}");
    }

    #[test]
    fn dxf_missing_eof_is_rejected() {
        // ENTITIES present but the file is cut off before EOF (a partial write).
        let dxf = "0\nSECTION\n2\nENTITIES\n0\nLINE\n8\n0\n";
        let err = validate_dxf_complete(dxf).unwrap_err();
        assert!(err.contains("conversion incomplete"), "{err}");
    }

    #[test]
    fn validation_tolerates_crlf() {
        let dxf = "0\r\nSECTION\r\n2\r\nENTITIES\r\n0\r\nENDSEC\r\n0\r\nEOF\r\n";
        assert!(validate_dxf_complete(dxf).is_ok());
    }

    #[test]
    fn convert_absent_binary_message() {
        // Explicit nonexistent binary path → run failure surfaced cleanly.
        let dir = tmp("noconv");
        let input = dir.join("a.dwg");
        fs::write(&input, b"DWGX").unwrap();
        let err = convert_dwg_to_dxf_with(
            Path::new("/nonexistent/dwg2dxf"),
            input.to_str().unwrap(),
        )
        .unwrap_err();
        assert!(err.contains("failed to run dwg2dxf"), "{err}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn convert_missing_input_is_rejected() {
        let err = convert_dwg_to_dxf_with(Path::new("/bin/true"), "/nope/missing.dwg")
            .unwrap_err();
        assert!(err.contains("cannot read DWG"), "{err}");
    }

    /// A stub `dwg2dxf` (a shell script) that writes a VALID small DXF to the
    /// `-o` target → conversion returns the DXF text.
    #[cfg(unix)]
    #[test]
    fn convert_with_stub_emitting_valid_dxf() {
        let dir = tmp("stubvalid");
        let stub = dir.join("dwg2dxf");
        // Args arrive as: -o <out> <input>. `$2` is the out path.
        fs::write(
            &stub,
            b"#!/bin/sh\nprintf '0\\nSECTION\\n2\\nENTITIES\\n0\\nLINE\\n8\\n0\\n\
              10\\n0\\n20\\n0\\n11\\n5\\n21\\n1\\n0\\nENDSEC\\n0\\nEOF\\n' > \"$2\"\n",
        )
        .unwrap();
        make_executable(&stub);
        let input = dir.join("model.dwg");
        fs::write(&input, b"DWG-fake").unwrap();
        let got = convert_dwg_to_dxf_with(&stub, input.to_str().unwrap()).unwrap();
        assert!(got.dxf.contains("ENTITIES"), "{}", got.dxf);
        assert!(got.dxf.contains("LINE"), "{}", got.dxf);
        // A silent converter (no stderr) → no soft warnings.
        assert!(got.warnings.is_empty(), "{:?}", got.warnings);
        fs::remove_dir_all(&dir).unwrap();
    }

    /// A stub that writes a COMPLETE DXF but emits LibreDWG-style ERROR lines on
    /// stderr (the 000-BG.dwg shape: exit 0, valid drawing, noisy stderr). The
    /// conversion must SUCCEED and carry a populated warning summary — never fail.
    #[cfg(unix)]
    #[test]
    fn convert_with_stub_emitting_stderr_warnings_succeeds_with_warnings() {
        let dir = tmp("stubwarn");
        let stub = dir.join("dwg2dxf");
        fs::write(
            &stub,
            b"#!/bin/sh\n\
              printf 'ERROR: bad AEC proxy\\nERROR: unstable class\\nWarning: skipped\\n' 1>&2\n\
              printf '0\\nSECTION\\n2\\nENTITIES\\n0\\nLINE\\n8\\n0\\n\
              10\\n0\\n20\\n0\\n11\\n5\\n21\\n1\\n0\\nENDSEC\\n0\\nEOF\\n' > \"$2\"\n",
        )
        .unwrap();
        make_executable(&stub);
        let input = dir.join("adt.dwg");
        fs::write(&input, b"DWG-fake").unwrap();
        let got = convert_dwg_to_dxf_with(&stub, input.to_str().unwrap())
            .expect("noisy-but-complete conversion must succeed, not fail");
        assert!(got.dxf.contains("ENTITIES"), "{}", got.dxf);
        assert_eq!(got.warnings.len(), 1, "{:?}", got.warnings);
        assert!(got.warnings[0].contains('3'), "{:?}", got.warnings); // 3 lines
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn counts_error_and_warning_lines() {
        let stderr = "ERROR: a\nfine line\nWarning: b\nWARNING C\nokay\nerror: d\n";
        assert_eq!(count_libredwg_warnings(stderr), 4);
        assert!(count_libredwg_warnings("").eq(&0));
    }

    #[test]
    fn conversion_warnings_empty_when_silent() {
        assert!(conversion_warnings("").is_empty());
        assert!(conversion_warnings("all good\nnothing here\n").is_empty());
    }

    #[test]
    fn conversion_warnings_summarize_count() {
        let w = conversion_warnings("ERROR: x\nERROR: y\n");
        assert_eq!(w.len(), 1);
        assert!(w[0].contains('2') && w[0].contains("warning"), "{:?}", w);
    }

    /// A stub that writes a TRUNCATED DXF (no ENTITIES, no EOF) but exits 0 —
    /// the exact LibreDWG-0.13.3 failure mode. Conversion must reject it.
    #[cfg(unix)]
    #[test]
    fn convert_with_stub_emitting_truncated_dxf_is_rejected() {
        let dir = tmp("stubtrunc");
        let stub = dir.join("dwg2dxf");
        fs::write(
            &stub,
            b"#!/bin/sh\nprintf '0\\nSECTION\\n2\\nHEADER\\n0\\nENDSEC\\n' > \"$2\"\nexit 0\n",
        )
        .unwrap();
        make_executable(&stub);
        let input = dir.join("broken.dwg");
        fs::write(&input, b"DWG-fake").unwrap();
        let err = convert_dwg_to_dxf_with(&stub, input.to_str().unwrap()).unwrap_err();
        assert!(err.contains("conversion incomplete"), "{err}");
        fs::remove_dir_all(&dir).unwrap();
    }

    /// A stub that writes a COMPLETE DXF (ENTITIES + EOF) but injects a raw
    /// non-UTF-8 byte (0xFF) inside a binary group-code value — exactly what a
    /// real AutoCAD DWG's proxy/thumbnail payload produces. The conversion must
    /// succeed (lossy decode), NOT be misreported as "conversion incomplete".
    /// This is the 000-BG.dwg regression: 0.14 produced a full 200 MB DXF that
    /// `read_to_string` rejected on the first binary byte.
    #[cfg(unix)]
    #[test]
    fn convert_with_stub_emitting_non_utf8_dxf_succeeds() {
        let dir = tmp("stubbinary");
        let stub = dir.join("dwg2dxf");
        // Writes a valid DXF skeleton, then a 0xFF byte, then the EOF marker.
        fs::write(
            &stub,
            b"#!/bin/sh\n{ printf '0\\nSECTION\\n2\\nENTITIES\\n0\\nLINE\\n310\\n'; \
              printf '\\377\\377'; printf '\\n0\\nENDSEC\\n0\\nEOF\\n'; } > \"$2\"\n",
        )
        .unwrap();
        make_executable(&stub);
        let input = dir.join("binary.dwg");
        fs::write(&input, b"DWG-fake").unwrap();
        let got = convert_dwg_to_dxf_with(&stub, input.to_str().unwrap())
            .expect("non-UTF-8 but complete DXF must convert, not be flagged incomplete");
        assert!(got.dxf.contains("ENTITIES"), "{}", got.dxf);
        assert!(got.dxf.contains("EOF"), "{}", got.dxf);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    fn make_executable(p: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let mut perm = fs::metadata(p).unwrap().permissions();
        perm.set_mode(0o755);
        fs::set_permissions(p, perm).unwrap();
    }

    /// OPTIONAL real end-to-end: if a real `dwg2dxf` is installed, converting a
    /// tiny synthetic DXF-derived DWG round-trips. Ignored by default because it
    /// needs the external binary AND a `.dwg` fixture we do not ship.
    #[test]
    #[ignore = "needs a real dwg2dxf binary and a .dwg fixture"]
    fn real_dwg2dxf_end_to_end() {
        let bin = resolve_dwg2dxf().expect("dwg2dxf must be installed for this test");
        // A caller can point this at any small .dwg; kept ignored so CI never
        // depends on the binary.
        let fixture = std::env::var("ITSJUSTCAD_DWG_FIXTURE")
            .expect("set ITSJUSTCAD_DWG_FIXTURE to a .dwg path");
        let got = convert_dwg_to_dxf_with(&bin, &fixture).unwrap();
        assert!(got.dxf.contains("ENTITIES"));
    }
}
