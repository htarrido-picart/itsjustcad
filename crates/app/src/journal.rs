//! Crash-recovery journal: one serialized `Command` per line (JSONL), mirroring
//! the session's effective op-log. Lives in the app layer — the substrate
//! stays journal-free. Deleted on clean save/exit; survivors mean a crash,
//! and `recover` replays the newest one.

use std::path::{Path, PathBuf};

use mydrafter_commands::{Command, Session};

pub struct Journal {
    path: PathBuf,
    /// Serialized ops currently on disk; prefix-compare decides append vs rewrite.
    lines: Vec<String>,
}

impl Journal {
    /// Journal in the default directory, stamped per session (time + pid so
    /// concurrent instances never collide). The file is created lazily on
    /// the first sync with ops.
    pub fn open_default() -> Option<Self> {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        Some(Self::new(
            default_dir()?.join(format!("{stamp}-{}.jsonl", std::process::id())),
        ))
    }

    pub fn new(path: PathBuf) -> Self {
        Self { path, lines: Vec::new() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Mirror the session's effective log to disk. New ops append one line
    /// each; undo (log shrank or diverged) rewrites the file; an empty log
    /// removes it — an empty journal has nothing to recover.
    pub fn sync(&mut self, session: &Session) {
        let lines: Vec<String> = session
            .save_log()
            .iter()
            .map(|c| serde_json::to_string(c).expect("command serializes"))
            .collect();
        if lines == self.lines {
            return;
        }
        let result = if lines.is_empty() {
            remove_if_exists(&self.path)
        } else if lines.len() > self.lines.len() && lines[..self.lines.len()] == self.lines[..] {
            append_lines(&self.path, &lines[self.lines.len()..], self.lines.is_empty())
        } else {
            write_all(&self.path, &lines)
        };
        if let Err(e) = result {
            tracing::warn!("journal write failed: {e}");
        }
        self.lines = lines;
    }

    /// Clean save/exit: the op-log is safe elsewhere (or intentionally
    /// abandoned), so the journal goes away.
    pub fn discard(&mut self) {
        if let Err(e) = remove_if_exists(&self.path) {
            tracing::warn!("journal delete failed: {e}");
        }
        self.lines.clear();
    }
}

fn remove_if_exists(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

fn append_lines(path: &Path, lines: &[String], first_write: bool) -> std::io::Result<()> {
    use std::io::Write;
    if first_write && let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    for line in lines {
        writeln!(file, "{line}")?;
    }
    file.sync_all()
}

fn write_all(path: &Path, lines: &[String]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, lines.join("\n") + "\n")
}

pub fn default_dir() -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(".config").join("mydrafter").join("journal"))
}

/// Journals left by other (crashed) sessions, newest first by mtime.
pub fn recoverable(dir: &Path, exclude: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl") && p != exclude)
        .filter_map(|p| Some((p.metadata().ok()?.modified().ok()?, p)))
        .collect();
    found.sort_by_key(|a| std::cmp::Reverse(a.0));
    found.into_iter().map(|(_, p)| p).collect()
}

/// Replay a journal into a fresh session through the same apply path used
/// live — identical ids, identical objects.
pub fn load(path: &Path) -> Result<Session, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let ops: Vec<Command> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).map_err(|e| format!("bad journal line: {e}")))
        .collect::<Result<_, _>>()?;
    Session::replay(ops).map_err(|e| format!("replay failed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mydrafter_commands::parse;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!("mydrafter-journal-test-{}-{name}", std::process::id()))
    }

    fn session_with(lines: &[&str]) -> Session {
        let mut s = Session::default();
        for line in lines {
            s.run(parse(line).unwrap()).unwrap();
        }
        s
    }

    #[test]
    fn journal_lines_are_parseable_commands() {
        let dir = temp_path("format");
        let mut j = Journal::new(dir.join("s.jsonl"));
        let s = session_with(&["box 0,0,0 2,2,2", "circle 5,5,0 1", "move last 0,0,1"]);
        j.sync(&s);
        let text = std::fs::read_to_string(j.path()).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        for line in &lines {
            let cmd: Command = serde_json::from_str(line).expect("each line parses as a Command");
            // Round-trips identically — the journal is the op-log, verbatim.
            assert_eq!(
                serde_json::to_string(&cmd).unwrap(),
                *line
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn replaying_journal_reproduces_document() {
        let dir = temp_path("replay");
        let mut j = Journal::new(dir.join("s.jsonl"));
        let mut s = session_with(&["box 0,0,0 2,2,2", "rect 4,0,0 3 2"]);
        j.sync(&s);
        // Incremental append after the first sync.
        s.run(parse("extrude last 1.5").unwrap()).unwrap();
        j.sync(&s);
        let recovered = load(j.path()).unwrap();
        assert_eq!(recovered.doc.len(), s.doc.len());
        assert_eq!(
            serde_json::to_string(&recovered.save_log()).unwrap(),
            serde_json::to_string(&s.save_log()).unwrap()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn undo_rewrites_and_discard_removes() {
        let dir = temp_path("undo");
        let mut j = Journal::new(dir.join("s.jsonl"));
        let mut s = session_with(&["box 0,0,0 1,1,1", "box 3,0,0 1,1,1"]);
        j.sync(&s);
        s.run(parse("undo").unwrap()).unwrap();
        j.sync(&s);
        assert_eq!(
            std::fs::read_to_string(j.path()).unwrap().lines().count(),
            1,
            "undo shrinks the journal"
        );
        // undo + a different command: divergent tail must be rewritten, not appended
        s.run(parse("undo").unwrap()).unwrap();
        s.run(parse("circle 0,0,0 2").unwrap()).unwrap();
        j.sync(&s);
        let recovered = load(j.path()).unwrap();
        assert_eq!(
            serde_json::to_string(&recovered.save_log()).unwrap(),
            serde_json::to_string(&s.save_log()).unwrap()
        );
        j.discard();
        assert!(!j.path().exists(), "journal removed on save/exit");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_log_removes_journal_file() {
        let dir = temp_path("empty");
        let mut j = Journal::new(dir.join("s.jsonl"));
        let mut s = session_with(&["box 0,0,0 1,1,1"]);
        j.sync(&s);
        assert!(j.path().exists());
        s.run(parse("undo").unwrap()).unwrap();
        j.sync(&s);
        assert!(!j.path().exists(), "nothing to recover -> no file");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recoverable_lists_newest_first_and_excludes_own() {
        let dir = temp_path("scan");
        std::fs::create_dir_all(&dir).unwrap();
        let old = dir.join("old.jsonl");
        let new = dir.join("new.jsonl");
        let own = dir.join("own.jsonl");
        std::fs::write(&old, "x\n").unwrap();
        let past = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        std::fs::File::open(&old).unwrap().set_modified(past).unwrap();
        std::fs::write(&new, "x\n").unwrap();
        std::fs::write(&own, "x\n").unwrap();
        std::fs::File::open(&own).unwrap().set_modified(past).unwrap();
        let found = recoverable(&dir, &own);
        assert_eq!(found, vec![new.clone(), old.clone()]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
