//! Block content library: on-disk ~/.config/itsjustcad/blocks/*.block.json
//! with a starter set embedded from assets/blocks/ and seeded on first run.
//!
//! Commands: `blocklib list`, `blockload <name>`, `blocksave <name>`.

use std::path::PathBuf;

use itsjustcad_doc::BlockGeometry;
use serde::{Deserialize, Serialize};

/// On-disk format for a `.block.json` file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockFile {
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    pub geometries: Vec<BlockGeometry>,
}

/// Starter blocks embedded at compile time from `assets/blocks/`.
pub(crate) static STARTER_BLOCKS: &[(&str, &str)] = &[
    (
        "door-single.block.json",
        include_str!("../../../assets/blocks/door-single.block.json"),
    ),
    (
        "window-double.block.json",
        include_str!("../../../assets/blocks/window-double.block.json"),
    ),
    (
        "tree.block.json",
        include_str!("../../../assets/blocks/tree.block.json"),
    ),
    (
        "person-scale-figure.block.json",
        include_str!("../../../assets/blocks/person-scale-figure.block.json"),
    ),
    (
        "north-arrow.block.json",
        include_str!("../../../assets/blocks/north-arrow.block.json"),
    ),
    (
        "grid-bubble.block.json",
        include_str!("../../../assets/blocks/grid-bubble.block.json"),
    ),
];

/// Returns `~/.config/itsjustcad/blocks/`.
pub fn blocklib_dir() -> Option<PathBuf> {
    Some(
        dirs::home_dir()?
            .join(".config")
            .join("itsjustcad")
            .join("blocks"),
    )
}

/// Seed `~/.config/itsjustcad/blocks/` from the embedded starter blocks if the
/// directory does not exist or is empty. Silently skips on any I/O error.
pub fn seed_if_empty() {
    let Some(dir) = blocklib_dir() else { return };
    seed_dir(&dir);
}

/// List all block names available in the library directory.
/// Returns `(names, dir_path_string)`.
pub fn list() -> Result<(Vec<String>, String), BlockLibError> {
    let dir = blocklib_dir().ok_or(BlockLibError::NoHomeDir)?;
    list_in_dir(&dir)
}

/// Load a named block from the library. Returns the `BlockFile`.
pub fn load(name: &str) -> Result<BlockFile, BlockLibError> {
    let dir = blocklib_dir().ok_or(BlockLibError::NoHomeDir)?;
    load_from_dir(&dir, name)
}

/// Save a block definition to the library under `name`.
pub fn save(
    name: &str,
    description: &str,
    geometries: Vec<BlockGeometry>,
) -> Result<PathBuf, BlockLibError> {
    let dir = blocklib_dir().ok_or(BlockLibError::NoHomeDir)?;
    save_to_dir(&dir, name, description, geometries)
}

fn validate_name(name: &str) -> Result<(), BlockLibError> {
    if name.is_empty() {
        return Err(BlockLibError::BadName("name cannot be empty".to_string()));
    }
    if name.contains(['/', '\\', '.']) {
        return Err(BlockLibError::BadName(format!(
            "name '{name}' cannot contain '/', '\\\\' or '.'"
        )));
    }
    Ok(())
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum BlockLibError {
    #[error("block '{0}' not found in library (use 'blocklib list')")]
    NotFound(String),
    #[error("no home directory")]
    NoHomeDir,
    #[error("block library I/O error: {0}")]
    Io(String),
    #[error("block file parse error: {0}")]
    Parse(String),
    #[error("invalid block name: {0}")]
    BadName(String),
}

// ── dir-parameterised helpers (testable, also called from exec) ───────────

/// Seed a specific directory with embedded starter blocks if empty or absent.
pub(crate) fn seed_dir(dir: &std::path::Path) {
    let is_empty = match std::fs::read_dir(dir) {
        Ok(mut rd) => rd.next().is_none(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if std::fs::create_dir_all(dir).is_err() {
                return;
            }
            true
        }
        Err(_) => return,
    };
    if !is_empty {
        return;
    }
    for (filename, content) in STARTER_BLOCKS {
        let _ = std::fs::write(dir.join(filename), content);
    }
}

pub(crate) fn list_in_dir(
    dir: &std::path::Path,
) -> Result<(Vec<String>, String), BlockLibError> {
    let dir_str = dir.display().to_string();
    match std::fs::read_dir(dir) {
        Ok(rd) => {
            let mut names: Vec<String> = rd
                .filter_map(|e| {
                    let e = e.ok()?;
                    let fname = e.file_name();
                    let s = fname.to_string_lossy();
                    s.ends_with(".block.json").then(|| {
                        s.strip_suffix(".block.json")
                            .unwrap_or(&s)
                            .to_string()
                    })
                })
                .collect();
            names.sort();
            Ok((names, dir_str))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((vec![], dir_str)),
        Err(e) => Err(BlockLibError::Io(e.to_string())),
    }
}

pub(crate) fn load_from_dir(
    dir: &std::path::Path,
    name: &str,
) -> Result<BlockFile, BlockLibError> {
    validate_name(name)?;
    let path = dir.join(format!("{name}.block.json"));
    let text = std::fs::read_to_string(&path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            BlockLibError::NotFound(name.to_string())
        } else {
            BlockLibError::Io(e.to_string())
        }
    })?;
    serde_json::from_str(&text).map_err(|e| BlockLibError::Parse(e.to_string()))
}

pub(crate) fn save_to_dir(
    dir: &std::path::Path,
    name: &str,
    description: &str,
    geometries: Vec<BlockGeometry>,
) -> Result<PathBuf, BlockLibError> {
    validate_name(name)?;
    std::fs::create_dir_all(dir).map_err(|e| BlockLibError::Io(e.to_string()))?;
    let bf = BlockFile {
        name: name.to_string(),
        description: description.to_string(),
        geometries,
    };
    let text =
        serde_json::to_string_pretty(&bf).map_err(|e| BlockLibError::Io(e.to_string()))?;
    let path = dir.join(format!("{name}.block.json"));
    std::fs::write(&path, &text).map_err(|e| BlockLibError::Io(e.to_string()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("itsjustcad_blocklib_tests")
            .join(label);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Parse every embedded starter block — catches JSON format regressions.
    #[test]
    fn starter_blocks_parse() {
        for (fname, content) in STARTER_BLOCKS {
            let bf: BlockFile = serde_json::from_str(content)
                .unwrap_or_else(|e| panic!("{fname} failed to parse: {e}"));
            assert!(!bf.geometries.is_empty(), "{fname} must have geometry");
            assert!(!bf.name.is_empty(), "{fname} must have a name");
        }
    }

    #[test]
    fn seed_writes_starter_files() {
        let dir = tmp_dir("seed_writes");
        seed_dir(&dir);
        let count = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .count();
        assert_eq!(count, STARTER_BLOCKS.len(), "all starters written");
    }

    #[test]
    fn seed_is_idempotent_when_not_empty() {
        let dir = tmp_dir("seed_idem");
        seed_dir(&dir);
        // Add an extra file — second seed must not wipe it.
        let extra = dir.join("custom.block.json");
        fs::write(&extra, "{}").unwrap();
        seed_dir(&dir);
        assert!(extra.exists(), "extra file preserved after second seed");
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = tmp_dir("roundtrip");
        use kernel_curve::Curve;
        let geoms = vec![BlockGeometry::Curve(Curve::Line {
            a: glam::DVec3::ZERO,
            b: glam::DVec3::new(1.0, 0.0, 0.0),
        })];
        save_to_dir(&dir, "my-block", "a test block", geoms).unwrap();
        let loaded = load_from_dir(&dir, "my-block").unwrap();
        assert_eq!(loaded.name, "my-block");
        assert_eq!(loaded.description, "a test block");
        assert_eq!(loaded.geometries.len(), 1);
    }

    #[test]
    fn list_returns_names_sorted() {
        let dir = tmp_dir("list_sorted");
        seed_dir(&dir);
        let (names, _) = list_in_dir(&dir).unwrap();
        assert!(!names.is_empty(), "list must be non-empty after seed");
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted, "names must be sorted");
    }

    #[test]
    fn list_returns_only_block_json_files() {
        let dir = tmp_dir("list_filter");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("foo.block.json"), include_str!("../../../assets/blocks/tree.block.json")).unwrap();
        fs::write(dir.join("README.txt"), "ignore me").unwrap();
        let (names, _) = list_in_dir(&dir).unwrap();
        assert_eq!(names, vec!["foo"]);
    }

    #[test]
    fn load_unknown_returns_not_found() {
        let dir = tmp_dir("load_unknown");
        let err = load_from_dir(&dir, "doesnotexist").unwrap_err();
        assert!(
            matches!(err, BlockLibError::NotFound(_)),
            "unexpected err: {err}"
        );
    }

    #[test]
    fn load_bad_json_returns_parse_error() {
        let dir = tmp_dir("bad_json");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("bad.block.json"), "not json").unwrap();
        let err = load_from_dir(&dir, "bad").unwrap_err();
        assert!(matches!(err, BlockLibError::Parse(_)));
    }

    #[test]
    fn name_path_traversal_rejected() {
        assert!(validate_name("../etc/passwd").is_err());
        assert!(validate_name("foo/bar").is_err());
        assert!(validate_name("foo.bar").is_err());
        assert!(validate_name("").is_err());
        assert!(validate_name("valid-name").is_ok());
        assert!(validate_name("tree").is_ok());
    }

    #[test]
    fn blockload_then_insert_cycle() {
        // Full cycle: load a starter from the library, define it in the doc,
        // insert instances — mirrors the commands flow.
        use crate::exec::Session;
        let dir = tmp_dir("load_insert");
        seed_dir(&dir);

        let bf = load_from_dir(&dir, "tree").unwrap();
        assert_eq!(bf.name, "tree");

        let mut s = Session::default();
        s.doc.blocks.insert(bf.name.clone(), bf.geometries);
        s.run(crate::parse::parse("insert tree 5,5,0").unwrap()).unwrap();
        s.run(crate::parse::parse("insert tree 10,3,0").unwrap()).unwrap();
        assert_eq!(s.doc.len(), 2, "two tree instances in doc");
    }
}
