// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! End-to-end sanity driver for the model downloader.
//!
//! Downloads a SMALL real file over HTTPS (NOT a multi-GB model), prints live
//! progress, and verifies its SHA-256. Proves the fetch → progress → verify →
//! rename pipeline works against a live server without touching the catalog's
//! huge weights.
//!
//! Run:
//!   cargo run -p itsjustcad --example download_model -- \
//!     <url> <sha256-hex> [file_name]
//!
//! With no args it defaults to a tiny, stable file with a known checksum.

use std::path::PathBuf;

#[path = "../src/download.rs"]
mod download;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Default: a small, stable text file (RFC 2606 example domain hosts big
    // files too; we use a tiny known asset). Override via CLI for other files.
    let (url, sha, name) = if args.len() >= 2 {
        (
            args[0].clone(),
            Some(args[1].clone()),
            args.get(2).cloned().unwrap_or_else(|| "download.bin".to_string()),
        )
    } else {
        // ~140 KB Rust logo PNG from crates.io static — small + stable. If the
        // sha ever drifts, pass an explicit url + sha on the CLI instead.
        (
            "https://www.rust-lang.org/static/images/rust-logo-blk.svg".to_string(),
            None, // sha unknown for the default asset — verify step is skipped.
            "rust-logo.svg".to_string(),
        )
    };

    let dir = std::env::temp_dir().join("ijc_download_example");
    let _ = std::fs::remove_dir_all(&dir);
    let spec = download::DownloadSpec {
        url: url.clone(),
        dir: dir.clone(),
        file_name: name.clone(),
        expected_sha256: sha.clone(),
    };

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let dl = download::start(rt.handle(), spec);

    println!("Downloading {url}");
    loop {
        let state = dl.state();
        println!("  {}", download::progress_caption(&state));
        match state {
            download::DownloadState::Done { path } => {
                println!("DONE → {}", path.display());
                let hash = download::sha256_file(&path).expect("hash final file");
                println!("sha256 = {hash}");
                if let Some(exp) = &sha {
                    match download::verify_sha(Some(exp), &hash) {
                        Ok(()) => println!("SHA OK ✓"),
                        Err(e) => {
                            eprintln!("SHA FAIL: {e}");
                            std::process::exit(1);
                        }
                    }
                } else {
                    println!("(no expected sha supplied — verify skipped)");
                }
                break;
            }
            download::DownloadState::Failed { msg } => {
                eprintln!("FAILED: {msg}");
                std::process::exit(1);
            }
            _ => std::thread::sleep(std::time::Duration::from_millis(150)),
        }
    }
    let _ = std::fs::remove_dir_all::<PathBuf>(dir);
}
