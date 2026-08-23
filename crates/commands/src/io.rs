use serde::{Deserialize, Serialize};

use crate::{Command, ExecError, Session};

pub const FORMAT_VERSION: u32 = 1;

/// File format: the effective forward op-log, nothing else. Loading replays it
/// through the same `apply` path used live, reproducing identical ids.
#[derive(Serialize, Deserialize)]
struct FileFormat {
    mydrafter: u32,
    ops: Vec<Command>,
}

#[derive(Debug, thiserror::Error)]
pub enum IoError {
    #[error("not a mydrafter file: {0}")]
    BadFormat(#[from] serde_json::Error),
    #[error("unsupported format version {0} (this build reads {FORMAT_VERSION})")]
    BadVersion(u32),
    #[error("replay failed: {0}")]
    Replay(#[from] ExecError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub fn to_json(session: &Session) -> String {
    let file = FileFormat {
        mydrafter: FORMAT_VERSION,
        ops: session.save_log(),
    };
    serde_json::to_string_pretty(&file).expect("op-log serializes")
}

pub fn from_json(json: &str) -> Result<Session, IoError> {
    let file: FileFormat = serde_json::from_str(json)?;
    if file.mydrafter != FORMAT_VERSION {
        return Err(IoError::BadVersion(file.mydrafter));
    }
    Ok(Session::replay(file.ops)?)
}

pub fn save_file(session: &Session, path: &std::path::Path) -> Result<(), IoError> {
    Ok(std::fs::write(path, to_json(session))?)
}

pub fn load_file(path: &std::path::Path) -> Result<Session, IoError> {
    from_json(&std::fs::read_to_string(path)?)
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
        assert_eq!(s.doc.current_layer, mydrafter_doc::DEFAULT_LAYER);
        assert!(s.doc.objects().all(|o| o.layer == mydrafter_doc::DEFAULT_LAYER));
        assert_eq!(s.doc.layers.len(), 1);
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
        assert_eq!(s.doc.units, mydrafter_doc::Units::M);
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
        assert_eq!(loaded.doc.units, mydrafter_doc::Units::FtIn);
        assert_eq!(to_json(&loaded), json);
    }

    #[test]
    fn save_load_preserves_named_views() {
        let mut s = Session::default();
        s.run(parse("box 0,0,0 1,1,1").unwrap()).unwrap();
        let v = mydrafter_doc::NamedView {
            target: [4.5, -2.0, 1.25],
            distance: 27.5,
            yaw: -0.75,
            pitch: 1.2,
            fov_y: 45f32.to_radians(),
            ortho: false,
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
    fn rejects_future_version() {
        let Err(err) = from_json(r#"{"mydrafter": 99, "ops": []}"#) else {
            panic!("expected version error");
        };
        assert!(matches!(err, IoError::BadVersion(99)));
    }
}
