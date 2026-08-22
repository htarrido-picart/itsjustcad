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
    fn rejects_future_version() {
        let Err(err) = from_json(r#"{"mydrafter": 99, "ops": []}"#) else {
            panic!("expected version error");
        };
        assert!(matches!(err, IoError::BadVersion(99)));
    }
}
