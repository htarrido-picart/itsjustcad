// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Guided `offset` — the **reference example** that locks the engine pattern.
//! Pre-select a curve, get prompted for a distance and a side to offset toward;
//! the emitted `offset sel <dist> <side>` flows through the normal parser/exec
//! substrate (the executor turns the side point into a sign). Other groups
//! mirror this shape; leave this file as the canonical template.

use super::assemble::{num_at, point_at, selector};
use super::{Input, Step, VerbScript, fmt, num};

pub static SCRIPTS: &[VerbScript] = &[VerbScript {
    verb: "offset",
    needs_selection: true,
    steps: &[
        Step::Number { prompt: "Offset distance", default: Some(1.0) },
        Step::PickPoint { prompt: "Side to offset toward" },
    ],
    assemble: assemble_offset,
}];

/// `[Objects(sel), Num(dist), Point(side)] -> "offset sel <dist> <side>"`.
fn assemble_offset(args: &[Input]) -> Result<String, String> {
    let sel = selector(args, "offset")?;
    let dist = num_at(args, 1, "offset")?;
    let side = point_at(args, 2, "offset")?;
    Ok(format!("offset {sel} {} {}", num(dist), fmt(side)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::DVec3;

    #[test]
    fn assemble_offset_is_pure() {
        let args = [
            Input::Objects("sel".into()),
            Input::Num(0.2),
            Input::Point(DVec3::new(3.0, 0.0, 0.0)),
        ];
        assert_eq!(assemble_offset(&args).unwrap(), "offset sel 0.2 3,0");
        // Missing pieces are reported, not panicked.
        assert!(assemble_offset(&args[..1]).is_err());
    }
}
