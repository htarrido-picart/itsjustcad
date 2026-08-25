// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

use serde::{Deserialize, Serialize};

use crate::{Command, ExecError, Session};

pub const FORMAT_VERSION: u32 = 1;

/// Checkpoint sidecar version. Bumped independently of the op-log format; a
/// stale or unrecognized checkpoint is simply ignored (full replay).
pub const CHECKPOINT_VERSION: u32 = 1;

/// File format: the effective forward op-log plus, optionally, design-option
/// branches. Loading replays `ops` through the same `apply` path used live,
/// reproducing identical ids; branches are seeded verbatim (replayed only when
/// switched to). `branches`/`branch` are serde-default so pre-branch files load
/// unchanged, and are omitted from the output when there are no branches.
///
/// The version field is written as `"itsjustcad"` but reads either spelling:
/// `#[serde(alias = "mydrafter")]` accepts the historical key, so every file
/// ever written by the old product still loads. The "v1 replays forever"
/// promise holds across the rename — see FORMAT.md.
#[derive(Serialize, Deserialize)]
struct FileFormat {
    #[serde(alias = "mydrafter")]
    itsjustcad: u32,
    /// Stable document identity (RFC-4122 UUID string). `serde(default)` +
    /// `skip_serializing_if` keeps pre-uuid files loading AND keeps a document
    /// that never had a uuid byte-identical on re-save. The app keys per-doc
    /// chat sessions off this; it is NOT replayed and does not affect geometry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    uuid: Option<String>,
    ops: Vec<Command>,
    /// Named branches of the op-log (each a saved effective log). Empty for
    /// files with no design options.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    branches: std::collections::BTreeMap<String, Vec<Command>>,
    /// The branch the live `ops` belong to. Absent (→ "main") in old files and
    /// whenever there are no branches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    branch: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum IoError {
    #[error("not an itsjustcad file: {0}")]
    BadFormat(#[from] serde_json::Error),
    #[error("unsupported format version {0} (this build reads {FORMAT_VERSION})")]
    BadVersion(u32),
    #[error("replay failed: {0}")]
    Replay(#[from] ExecError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub fn to_json(session: &Session) -> String {
    let branches = session.branches().clone();
    // Only record the current branch marker when there are branches; a
    // branchless file stays byte-identical to the pre-branch format.
    let branch = (!branches.is_empty()).then(|| session.current_branch().to_string());
    let file = FileFormat {
        itsjustcad: FORMAT_VERSION,
        uuid: session.doc_uuid().map(str::to_string),
        ops: session.save_log(),
        branches,
        branch,
    };
    serde_json::to_string_pretty(&file).expect("op-log serializes")
}

pub fn from_json(json: &str) -> Result<Session, IoError> {
    let file: FileFormat = serde_json::from_str(json)?;
    if file.itsjustcad != FORMAT_VERSION {
        return Err(IoError::BadVersion(file.itsjustcad));
    }
    let mut session = Session::replay(file.ops)?;
    session.set_branches(
        file.branches,
        file.branch.unwrap_or_else(|| crate::exec::MAIN_BRANCH.to_string()),
    );
    session.set_doc_uuid(file.uuid);
    Ok(session)
}

/// Checkpoint sidecar: a serialized `Document` snapshot plus the op count it
/// corresponds to. Optional fast-open cache — the op-log stays the source of
/// truth, so a missing, stale, or corrupt checkpoint just falls back to replay.
#[derive(Serialize, Deserialize)]
struct Checkpoint {
    mydrafter_checkpoint: u32,
    /// Number of forward ops the snapshot reflects; must match the op-log's
    /// length for the snapshot to be trusted.
    op_count: usize,
    doc: itsjustcad_doc::Document,
}

/// Sidecar path for a document: `<file>.checkpoint`.
fn checkpoint_path(path: &std::path::Path) -> std::path::PathBuf {
    let mut p = path.as_os_str().to_owned();
    p.push(".checkpoint");
    std::path::PathBuf::from(p)
}

/// Serialize the session's document snapshot for the checkpoint sidecar.
fn checkpoint_json(session: &Session) -> String {
    let cp = Checkpoint {
        mydrafter_checkpoint: CHECKPOINT_VERSION,
        op_count: session.save_log().len(),
        doc: session.doc.clone(),
    };
    serde_json::to_string(&cp).expect("document snapshot serializes")
}

/// Write the op-log file and, alongside it, a `<file>.checkpoint` fast-open
/// cache. A failure to write the checkpoint is non-fatal: the primary file is
/// already saved and the cache is optional. Deleting the checkpoint is always
/// safe — the next open just replays the op-log.
pub fn save_file(session: &Session, path: &std::path::Path) -> Result<(), IoError> {
    std::fs::write(path, to_json(session))?;
    // Best-effort: never let a checkpoint problem fail an otherwise-good save.
    let _ = std::fs::write(checkpoint_path(path), checkpoint_json(session));
    Ok(())
}

/// Load a document. If a `<file>.checkpoint` exists whose `op_count` matches the
/// op-log, seed the session directly from the snapshot and skip replay;
/// otherwise (no sidecar, version/count mismatch, or any parse error) fall back
/// to a full op-log replay. Correctness is guaranteed by a `debug_assert` in
/// [`Session::from_snapshot`]'s lazy history rebuild and by the round-trip test.
pub fn load_file(path: &std::path::Path) -> Result<Session, IoError> {
    let json = std::fs::read_to_string(path)?;
    let file: FileFormat = serde_json::from_str(&json)?;
    if file.itsjustcad != FORMAT_VERSION {
        return Err(IoError::BadVersion(file.itsjustcad));
    }

    let branch = file.branch.unwrap_or_else(|| crate::exec::MAIN_BRANCH.to_string());

    if let Ok(text) = std::fs::read_to_string(checkpoint_path(path))
        && let Ok(cp) = serde_json::from_str::<Checkpoint>(&text)
        && cp.mydrafter_checkpoint == CHECKPOINT_VERSION
        && cp.op_count == file.ops.len()
    {
        let mut session = Session::from_snapshot(cp.doc, file.ops);
        session.set_branches(file.branches, branch);
        session.set_doc_uuid(file.uuid);
        return Ok(session);
    }

    let mut session = Session::replay(file.ops)?;
    session.set_branches(file.branches, branch);
    session.set_doc_uuid(file.uuid);
    Ok(session)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse;

    #[test]
    fn save_load_save_byte_identical() {
        let mut s = Session::default();
        for line in [
            "rect 0,0,0 6 4",
            "extrude last 3",
            "circle 12,2,0 2.5",
            "extrude last 8",
            "curve 0,-6 4,-9 10,-5",
            "copy all 0,20,0",
            "move last 0,0,1",
        ] {
            s.run(parse(line).unwrap()).unwrap();
        }
        let json1 = to_json(&s);
        let loaded = from_json(&json1).unwrap();
        let json2 = to_json(&loaded);
        assert_eq!(json1, json2);
        assert_eq!(s.doc.len(), loaded.doc.len());
    }

    #[test]
    fn amended_session_saves_and_replays_stably() {
        let mut s = Session::default();
        for line in ["box 0,0,0 5,5,3", "box 1,1,-1 2,2,5", "difference last 2 last"] {
            s.run(parse(line).unwrap()).unwrap();
        }
        s.run(parse("amend 0 box 0,0,0 8,8,3").unwrap()).unwrap();

        // The amend itself never lands in the log — only the edited ops do.
        let json1 = to_json(&s);
        assert!(!json1.contains("amend"), "{json1}");
        let loaded = from_json(&json1).unwrap();
        assert_eq!(to_json(&loaded), json1, "replay-stable");
        assert_eq!(loaded.doc.len(), s.doc.len());
        let ids: Vec<_> = s.doc.objects().map(|o| o.id).collect();
        let loaded_ids: Vec<_> = loaded.doc.objects().map(|o| o.id).collect();
        assert_eq!(ids, loaded_ids, "identical objects after replay");
    }

    #[test]
    fn pre_uuid_file_loads_and_stays_byte_identical_on_resave() {
        // A hand-written file from before the doc uuid existed: no `uuid` key.
        // It must load, expose no uuid, and re-serialize byte-for-byte identical
        // (skip_serializing_if keeps the header clean for never-stamped docs).
        let json = r#"{
            "mydrafter": 1,
            "ops": [
                {"cmd": "box",
                 "id": "00000000-0000-4000-8000-000000000001",
                 "corner": [0.0, 0.0, 0.0], "size": [2.0, 2.0, 2.0]}
            ]
        }"#;
        let s = from_json(json).unwrap();
        assert!(s.doc_uuid().is_none(), "pre-uuid file must carry no uuid");
        // Re-save produces no uuid field at all.
        let out = to_json(&s);
        assert!(!out.contains("uuid"), "never-stamped doc must not gain a uuid: {out}");
        // And round-trips stably.
        assert_eq!(to_json(&from_json(&out).unwrap()), out);
    }

    #[test]
    fn uuid_round_trips_when_present() {
        let mut s = Session::default();
        s.run(parse("box 0,0,0 1,1,1").unwrap()).unwrap();
        s.set_doc_uuid(Some("11112222-3333-4444-5555-666677778888".into()));
        let json = to_json(&s);
        assert!(json.contains("\"uuid\""), "uuid must be written: {json}");
        let loaded = from_json(&json).unwrap();
        assert_eq!(
            loaded.doc_uuid(),
            Some("11112222-3333-4444-5555-666677778888")
        );
        // Replay-stable with the uuid in place.
        assert_eq!(to_json(&loaded), json);
    }

    #[test]
    fn uuid_survives_amend() {
        let mut s = Session::default();
        s.run(parse("box 0,0,0 5,5,3").unwrap()).unwrap();
        s.set_doc_uuid(Some("aaaa0000-0000-4000-8000-000000000001".into()));
        s.amend(0, parse("box 0,0,0 8,8,3").unwrap()).unwrap();
        assert_eq!(
            s.doc_uuid(),
            Some("aaaa0000-0000-4000-8000-000000000001"),
            "amend must preserve document identity"
        );
    }

    #[test]
    fn pre_layer_file_loads_onto_default_layer() {
        // Hand-written v1 file from before layers existed: no layer ops,
        // no layer fields anywhere.
        let json = r#"{
            "mydrafter": 1,
            "ops": [
                {"cmd": "box",
                 "id": "00000000-0000-4000-8000-000000000001",
                 "corner": [0.0, 0.0, 0.0], "size": [2.0, 2.0, 2.0]},
                {"cmd": "move", "targets": {"sel": "last", "n": 1},
                 "delta": [1.0, 0.0, 0.0]}
            ]
        }"#;
        let s = from_json(json).unwrap();
        assert_eq!(s.doc.len(), 1);
        assert_eq!(s.doc.current_layer, itsjustcad_doc::DEFAULT_LAYER);
        assert!(s.doc.objects().all(|o| o.layer == itsjustcad_doc::DEFAULT_LAYER));
        // A pre-layer file introduces no layer ops, so the loaded document
        // carries only the seeded default layers (Default + Layer 01..05).
        assert_eq!(s.doc.layers.len(), 6);
        assert!(s.doc.layers.contains_key(itsjustcad_doc::DEFAULT_LAYER));
    }

    #[test]
    fn save_load_preserves_layers() {
        let mut s = Session::default();
        for line in [
            "layer walls",
            "layercolor walls 0.9,0.1,0.1",
            "box 0,0,0 1,1,1",
            "hide walls",
        ] {
            s.run(parse(line).unwrap()).unwrap();
        }
        let loaded = from_json(&to_json(&s)).unwrap();
        assert_eq!(loaded.doc.layers, s.doc.layers);
        assert_eq!(loaded.doc.current_layer, "walls");
        assert_eq!(to_json(&loaded), to_json(&s));
    }

    #[test]
    fn hideobj_round_trips_and_old_files_default_visible() {
        let mut s = Session::default();
        for line in ["box 0,0,0 1,1,1", "box 5,0,0 1,1,1", "hideobj last"] {
            s.run(parse(line).unwrap()).unwrap();
        }
        let json = to_json(&s);
        assert!(json.contains("hide_obj"), "{json}");
        let loaded = from_json(&json).unwrap();
        let vis: Vec<bool> = loaded.doc.objects().map(|o| o.visible).collect();
        assert_eq!(vis, [true, false]);
        assert_eq!(to_json(&loaded), json, "replay-stable");

        // Old files have no visibility ops; everything loads visible.
        let old = r#"{
            "mydrafter": 1,
            "ops": [
                {"cmd": "box",
                 "id": "00000000-0000-4000-8000-000000000001",
                 "corner": [0.0, 0.0, 0.0], "size": [2.0, 2.0, 2.0]}
            ]
        }"#;
        let s = from_json(old).unwrap();
        assert!(s.doc.objects().all(|o| o.visible));
    }

    #[test]
    fn pre_units_file_defaults_to_meters() {
        // Old files have no units op; they load as meters.
        let json = r#"{
            "mydrafter": 1,
            "ops": [
                {"cmd": "box",
                 "id": "00000000-0000-4000-8000-000000000001",
                 "corner": [0.0, 0.0, 0.0], "size": [2.0, 2.0, 2.0]}
            ]
        }"#;
        let s = from_json(json).unwrap();
        assert_eq!(s.doc.units, itsjustcad_doc::Units::M);
    }

    #[test]
    fn save_load_preserves_units() {
        let mut s = Session::default();
        for line in ["units ftin", "box 0,0,0 12ft,12ft,9ft"] {
            s.run(parse(line).unwrap()).unwrap();
        }
        let json = to_json(&s);
        assert!(json.contains("\"units\""), "{json}");
        let loaded = from_json(&json).unwrap();
        assert_eq!(loaded.doc.units, itsjustcad_doc::Units::FtIn);
        assert_eq!(to_json(&loaded), json);
    }

    #[test]
    fn save_load_preserves_named_views() {
        let mut s = Session::default();
        s.run(parse("box 0,0,0 1,1,1").unwrap()).unwrap();
        let v = itsjustcad_doc::NamedView {
            target: [4.5, -2.0, 1.25],
            distance: 27.5,
            yaw: -0.75,
            pitch: 1.2,
            fov_y: 45f32.to_radians(),
            ortho: false,
            two_point: false,
            pano: None,
        };
        s.run(crate::Command::ViewSave { name: "entry".to_string(), camera: Some(v) })
            .unwrap();
        let json = to_json(&s);
        assert!(json.contains("view_save"), "{json}");
        let loaded = from_json(&json).unwrap();
        assert_eq!(loaded.doc.named_views, s.doc.named_views);
        assert_eq!(loaded.doc.named_views["entry"], v);
        assert_eq!(to_json(&loaded), json, "replay-stable");
    }

    #[test]
    fn pre_named_views_file_loads_with_no_views() {
        // Old files have no view ops; they load with an empty view table.
        let json = r#"{
            "mydrafter": 1,
            "ops": [
                {"cmd": "box",
                 "id": "00000000-0000-4000-8000-000000000001",
                 "corner": [0.0, 0.0, 0.0], "size": [2.0, 2.0, 2.0]}
            ]
        }"#;
        let s = from_json(json).unwrap();
        assert!(s.doc.named_views.is_empty());
        assert!(s.doc.pending_view.is_none());
    }

    #[test]
    fn pre_underlay_file_loads_with_no_underlay() {
        // Old files have no underlay op; they load with none.
        let json = r#"{
            "mydrafter": 1,
            "ops": [
                {"cmd": "box",
                 "id": "00000000-0000-4000-8000-000000000001",
                 "corner": [0.0, 0.0, 0.0], "size": [2.0, 2.0, 2.0]}
            ]
        }"#;
        let s = from_json(json).unwrap();
        assert!(s.doc.underlay.is_none());
    }

    #[test]
    fn save_load_preserves_underlay() {
        // The height is carried on the logged op, so no image file is needed.
        let mut s = Session::default();
        s.run(Command::Underlay {
            path: "site.png".into(),
            corner: Some(glam::DVec3::new(1.0, 2.0, 0.0)),
            width: Some(20.0),
            height: Some(10.0),
        })
        .unwrap();
        s.run(parse("underlayopacity 0.4").unwrap()).unwrap();
        let json = to_json(&s);
        assert!(json.contains("\"underlay\""), "{json}");
        let loaded = from_json(&json).unwrap();
        assert_eq!(loaded.doc.underlay, s.doc.underlay);
        assert_eq!(to_json(&loaded), json, "replay-stable");
    }

    // ---- checkpoint fast-open cache ----

    fn built_session() -> Session {
        let mut s = Session::default();
        for line in [
            "box 0,0,0 5,5,3",
            "box 1,1,-1 2,2,5",
            "difference last 2 last",
            "circle 12,2,0 2.5",
            "extrude last 8",
            "layer walls",
            "sun 40.71 -74.01 2024-06-21 15:00",
        ] {
            s.run(parse(line).unwrap()).unwrap();
        }
        s
    }

    fn objects_of(s: &Session) -> Vec<itsjustcad_doc::SceneObject> {
        s.doc.objects().cloned().collect()
    }

    #[test]
    fn fast_open_snapshot_equals_full_replay() {
        // The core correctness invariant: a session seeded from the snapshot is
        // indistinguishable from one rebuilt by replaying the op-log.
        let s = built_session();
        let log = s.save_log();
        let replayed = Session::replay(log.clone()).unwrap();
        let fast = Session::from_snapshot(s.doc.clone(), log);
        assert_eq!(fast.doc, replayed.doc, "snapshot doc == replay doc");
        assert_eq!(fast.save_log(), replayed.save_log(), "same forward log");
        assert_eq!(objects_of(&fast), objects_of(&replayed));
    }

    #[test]
    fn checkpoint_round_trips_and_skips_replay() {
        let dir = std::env::temp_dir().join(format!("itsjustcad_cp_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("scene.mydrafter");
        let s = built_session();
        save_file(&s, &path).unwrap();

        // The sidecar exists and carries the matching op count.
        let cp_text = std::fs::read_to_string(checkpoint_path(&path)).unwrap();
        let cp: Checkpoint = serde_json::from_str(&cp_text).unwrap();
        assert_eq!(cp.op_count, s.save_log().len());

        // Loading with a valid checkpoint yields the same document as a replay.
        let loaded = load_file(&path).unwrap();
        assert_eq!(loaded.doc, s.doc);
        assert_eq!(to_json(&loaded), to_json(&s), "save is byte-identical");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn stale_checkpoint_falls_back_to_replay() {
        let dir = std::env::temp_dir().join(format!("itsjustcad_stale_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("scene.mydrafter");
        let s = built_session();
        save_file(&s, &path).unwrap();

        // Corrupt the checkpoint's op_count so it no longer matches the op-log.
        let mut cp: Checkpoint =
            serde_json::from_str(&std::fs::read_to_string(checkpoint_path(&path)).unwrap())
                .unwrap();
        cp.op_count += 999;
        std::fs::write(checkpoint_path(&path), serde_json::to_string(&cp).unwrap()).unwrap();

        // Load must ignore the stale sidecar and replay to the correct state.
        let loaded = load_file(&path).unwrap();
        assert_eq!(loaded.doc, s.doc, "stale checkpoint ignored, replay is correct");

        // Deleting the checkpoint is always safe: plain replay still works.
        std::fs::remove_file(checkpoint_path(&path)).unwrap();
        let loaded2 = load_file(&path).unwrap();
        assert_eq!(loaded2.doc, s.doc);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn undo_works_after_fast_open() {
        // Fast-open defers inverse materialization; the first undo must rebuild
        // the history and behave exactly like a replayed session's undo.
        let s = built_session();
        let before = objects_of(&s);
        let mut fast = Session::from_snapshot(s.doc.clone(), s.save_log());

        // A brand-new edit, then undo it — exercises ensure_history on the edit
        // path and the undo path.
        fast.run(parse("box 20,20,0 1,1,1").unwrap()).unwrap();
        assert_eq!(fast.doc.len(), before.len() + 1);
        fast.run(Command::Undo).unwrap();
        assert_eq!(objects_of(&fast), before, "undo returns to opened state");

        // Undoing further walks back into the pre-open history: undo the last
        // pre-open op (a `sun`) and compare to a replay that stops one op short.
        fast.run(Command::Undo).unwrap();
        let mut short_log = s.save_log();
        short_log.pop();
        let expected = Session::replay(short_log).unwrap();
        assert_eq!(fast.doc.sun, expected.doc.sun, "pre-open sun op undone");
        fast.run(Command::Redo).unwrap();
        assert_eq!(objects_of(&fast), before, "redo restores opened state");
        assert_eq!(fast.doc.sun, s.doc.sun);
    }

    #[test]
    fn old_mydrafter_version_field_still_loads() {
        // Backward-compat guarantee across the mydrafter → ItsJustCAD rename:
        // a file written by the OLD product spells the version field
        // `"mydrafter"`. The serde alias must accept it so no existing file
        // ever fails to load. New saves write `"itsjustcad"`.
        let old = r#"{
            "mydrafter": 1,
            "ops": [
                {"cmd": "box",
                 "id": "00000000-0000-4000-8000-000000000001",
                 "corner": [0.0, 0.0, 0.0], "size": [2.0, 2.0, 2.0]}
            ]
        }"#;
        let s = from_json(old).expect("old mydrafter-keyed file loads");
        assert_eq!(s.doc.len(), 1);
        // And a fresh save now emits the new key while staying replay-stable.
        let json = to_json(&s);
        assert!(json.contains("\"itsjustcad\": 1"), "new key written: {json}");
        assert!(!json.contains("\"mydrafter\""), "old key not re-emitted: {json}");
        assert_eq!(to_json(&from_json(&json).unwrap()), json, "replay-stable");
    }

    #[test]
    fn rejects_future_version() {
        let Err(err) = from_json(r#"{"mydrafter": 99, "ops": []}"#) else {
            panic!("expected version error");
        };
        assert!(matches!(err, IoError::BadVersion(99)));
    }

    #[test]
    fn branches_round_trip_through_file() {
        // Build two options, land on the second, then save/reload.
        let mut s = Session::default();
        s.run(parse("box 0,0,0 2,2,10").unwrap()).unwrap();
        s.run(parse("option save tower").unwrap()).unwrap();
        s.run(parse("box 0,0,0 8,8,3").unwrap()).unwrap();
        s.run(parse("option save courtyard").unwrap()).unwrap();
        s.run(parse("option tower").unwrap()).unwrap();

        let json = to_json(&s);
        assert!(json.contains("\"branches\""), "branches serialized: {json}");
        assert!(json.contains("courtyard"));
        let loaded = from_json(&json).unwrap();

        assert_eq!(loaded.current_branch(), "tower");
        assert_eq!(loaded.branches().len(), 2);
        assert_eq!(loaded.branches(), s.branches());
        // Live doc equals the tower branch (1 tall box).
        assert_eq!(loaded.doc.len(), 1);
        assert_eq!(to_json(&loaded), json, "replay-stable with branches");
    }

    #[test]
    fn old_file_without_branches_loads_on_main() {
        // Pre-branch files have no `branches`/`branch` fields.
        let old = r#"{
            "mydrafter": 1,
            "ops": [
                {"cmd": "box",
                 "id": "00000000-0000-4000-8000-000000000001",
                 "corner": [0.0, 0.0, 0.0], "size": [2.0, 2.0, 2.0]}
            ]
        }"#;
        let s = from_json(old).unwrap();
        assert_eq!(s.doc.len(), 1);
        assert!(s.branches().is_empty());
        assert_eq!(s.current_branch(), crate::MAIN_BRANCH);
        // A branchless session serializes without the optional fields, so the
        // format stays byte-compatible with old readers.
        let json = to_json(&s);
        assert!(!json.contains("branches"), "no branch fields emitted: {json}");
        assert!(!json.contains("\"branch\""));
    }
}
