// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart
//
// Regression for issue #120: a frozen geodesic dome is one mesh with thousands
// of boundary segments, and `feature_edges` used to restart its collinear-merge
// pair scan after every fuse (~O(n³)), hanging `ijc_render_frame` long enough
// for the iOS watchdog to kill the app.

use std::time::{Duration, Instant};

use itsjustcad_commands::{parse, Session};
use itsjustcad_doc::Geometry;

fn frozen_geodesic(frequency: u32) -> kernel_mesh::Mesh {
    let mut s = Session::default();
    let id = s
        .run(parse(&format!("geodesic {frequency} 5 dome")).unwrap())
        .unwrap()
        .created[0];
    s.run(parse("freeze last").unwrap()).unwrap();
    match &s.doc.get(id).unwrap().geometry {
        Geometry::Mesh(m) => m.clone(),
        g => panic!("frozen geodesic should be a plain mesh, got {g:?}"),
    }
}

fn timed_feature_edges(frequency: u32) -> (usize, usize, Duration) {
    let mesh = frozen_geodesic(frequency);
    let t = Instant::now();
    let edges = kernel_mesh::feature_edges(&mesh);
    (mesh.faces().len(), edges.len(), t.elapsed())
}

#[test]
fn frozen_geodesic_feature_edges_are_fast() {
    // Every strut is its own box (12 outline edges, nothing collinear to fuse),
    // so the edge count is fixed and unchanged by #120. The budget is generous
    // for an unoptimised debug build on a loaded box: the fix runs these in
    // ~6 / ~46 / ~114 ms debug, while the old scan took ~0.9 s / ~4.3 s RELEASE
    // at frequency 8 / 12.
    for (frequency, expected) in [(3, 1620), (8, 11_520), (12, 25_920)] {
        let (tris, edges, dt) = timed_feature_edges(frequency);
        eprintln!("geodesic {frequency}: {tris} tris → {edges} feature edges in {dt:?}");
        assert_eq!(edges, expected, "frozen geodesic {frequency} feature edge count");
        assert!(
            dt < Duration::from_secs(2),
            "feature_edges on frozen geodesic {frequency} took {dt:?} ({edges} edges)"
        );
    }
}
