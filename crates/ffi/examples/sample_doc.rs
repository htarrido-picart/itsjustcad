// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Emits a small `.itsjustcad.json` op-log to stdout — bundled in the iOS app so
//! the viewport shows geometry before any file is opened.
//! Run: cargo run -p itsjustcad-ffi --example sample_doc > sample.itsjustcad.json

use itsjustcad_commands::{io, parse, Session};

fn main() {
    let mut s = Session::default();
    for line in [
        "box -4,-4,0 8,8,3",
        "box -3,-3,3 2,2,4",
        "box 1,1,3 2,2,6",
        "box -3,2,3 3,1,2",
    ] {
        s.run(parse(line).expect("parse")).expect("run");
    }
    print!("{}", io::to_json(&s));
}
