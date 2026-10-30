// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Background undo-history rebuild after a fast open (#224). The session
//! refuses an undo past the open boundary ("Preparing undo history…"); that
//! refusal starts a worker that replays a copy of the opened ops, and the
//! update loop installs the result when it is done.

use std::sync::{Arc, Mutex};

use itsjustcad_commands::{RebuiltHistory, ReplayControl, Session};

type Slot = Arc<Mutex<Option<Result<RebuiltHistory, String>>>>;

/// The running rebuild, if any.
#[derive(Default)]
pub struct HistoryRebuild {
    slot: Option<Slot>,
}

impl HistoryRebuild {
    /// Call every frame. Returns a line for the command log when the rebuild
    /// ends, and whether it is still running (to keep repainting).
    pub fn poll(&mut self, session: &mut Session) -> (Option<String>, bool) {
        if !session.history_pending() {
            self.slot = None;
            return (None, false);
        }
        session.set_background_history(true);
        if self.slot.is_none() && session.history_requested() {
            let Some(job) = session.history_job() else {
                return (None, false);
            };
            let slot: Slot = Arc::default();
            let out = slot.clone();
            std::thread::spawn(move || {
                let r = job.run(ReplayControl::default()).map_err(|e| e.to_string());
                *out.lock().unwrap_or_else(|e| e.into_inner()) = Some(r);
            });
            self.slot = Some(slot);
        }
        let Some(slot) = &self.slot else {
            return (None, false);
        };
        let Some(result) = slot.lock().unwrap_or_else(|e| e.into_inner()).take() else {
            return (None, true);
        };
        self.slot = None;
        let line = match result {
            Ok(rebuilt) => {
                if !session.install_history(rebuilt) {
                    return (None, false); // stale: the document was replaced
                }
                "undo history ready".to_string()
            }
            Err(e) => format!("error: undo history: {e}"),
        };
        (Some(line), false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use itsjustcad_commands::{Command, ExecError, parse};

    #[test]
    fn refused_undo_starts_the_rebuild_and_poll_installs_it() {
        let mut src = Session::default();
        for i in 0..3 {
            src.run(parse(&format!("box {i},0,0 1,1,1")).unwrap()).unwrap();
        }
        let mut s = Session::from_snapshot(src.doc.clone(), src.save_log());
        let mut rb = HistoryRebuild::default();
        assert_eq!(rb.poll(&mut s), (None, false), "nothing started yet");
        s.run(parse("box 9,0,0 1,1,1").unwrap()).unwrap();
        assert_eq!(rb.poll(&mut s), (None, false), "an edit starts nothing");
        s.run(Command::Undo).unwrap();
        assert_eq!(s.run(Command::Undo).unwrap_err(), ExecError::HistoryPending);
        let t0 = std::time::Instant::now();
        let line = loop {
            match rb.poll(&mut s) {
                (Some(line), _) => break line,
                (None, running) => assert!(running),
            }
            assert!(t0.elapsed().as_secs() < 60);
            std::thread::sleep(std::time::Duration::from_millis(1));
        };
        assert_eq!(line, "undo history ready");
        assert!(!s.history_pending());
        s.run(Command::Undo).unwrap();
        assert_eq!(s.doc.len(), 2);
    }
}
