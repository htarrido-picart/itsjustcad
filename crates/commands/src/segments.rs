// SPDX-License-Identifier: AGPL-3.0-only

//! Shared helpers for the vector exporters (SVG, PDF, DXF), #278.
//!
//! - [`par_objects`]: the per-object segment collector. Each object's work
//!   (curve tessellation, mesh feature edges, Hershey text, hatch fill) runs on
//!   the rayon pool; the ordered `collect` keeps results in document order, so
//!   the output is byte-identical to a sequential pass.
//! - [`par_filter_map`]: ordered parallel `filter_map` over a slice (the PDF
//!   per-segment hidden-line cull + clip, #332).
//! - [`push_trimmed`]: writes a fixed-precision number straight into the output
//!   buffer with trailing zeros trimmed, without a per-number `String`.

use std::fmt::Write as _;

use itsjustcad_doc::{Document, SceneObject};
use rayon::prelude::*;

/// Map `f` over the document's objects on the rayon pool, in document order.
pub(crate) fn par_objects<'a, T, F>(doc: &'a Document, f: F) -> Vec<T>
where
    T: Send,
    F: Fn(&'a SceneObject) -> T + Sync + Send,
{
    let objs: Vec<&SceneObject> = doc.objects().collect();
    #[cfg(test)]
    if FORCE_SEQ.with(|s| s.get()) {
        return objs.into_iter().map(f).collect();
    }
    objs.into_par_iter().map(f).collect()
}

/// Ordered parallel `filter_map` over a slice on the rayon pool: same items, same
/// order as the sequential `iter().filter_map(f)` (which [`sequential`] forces).
pub(crate) fn par_filter_map<T, U, F>(items: &[T], f: F) -> Vec<U>
where
    T: Sync,
    U: Send,
    F: Fn(&T) -> Option<U> + Sync + Send,
{
    #[cfg(test)]
    if FORCE_SEQ.with(|s| s.get()) {
        return items.iter().filter_map(f).collect();
    }
    items.par_iter().filter_map(f).collect()
}

#[cfg(test)]
thread_local! {
    static FORCE_SEQ: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Run `f` with [`par_objects`] and [`par_filter_map`] forced onto the calling thread, in order. Test
/// reference for the parallel path.
#[cfg(test)]
pub(crate) fn sequential<R>(f: impl FnOnce() -> R) -> R {
    FORCE_SEQ.with(|s| s.set(true));
    let r = f();
    FORCE_SEQ.with(|s| s.set(false));
    r
}

/// Append `v` with `prec` decimals, trailing zeros trimmed (`1.5000` -> `1.5`,
/// `2.0000` -> `2.0`). Same bytes as `format!("{v:.prec$}")` followed by the
/// trim, without the intermediate `String`.
pub(crate) fn push_trimmed(out: &mut String, v: f64, prec: usize) {
    let start = out.len();
    let _ = write!(out, "{v:.prec$}");
    while out.len() > start && out.ends_with('0') {
        out.pop();
    }
    if out.ends_with('.') {
        out.push('0');
    }
}

/// A mixed document for the exporter byte-equality tests: curves, meshes,
/// linear and angular dims, text, hatch, two layers with weights and colors,
/// per-object lineweights, and a sheet with two views. Many objects, so the
/// rayon pool really splits the work.
#[cfg(test)]
pub(crate) fn mixed_session() -> crate::Session {
    let mut s = crate::Session::default();
    let mut run = |line: &str| {
        s.run(crate::parse(line).unwrap_or_else(|e| panic!("{line}: {e:?}")))
            .unwrap_or_else(|e| panic!("{line}: {e:?}"));
    };
    run("units m");
    run("layer walls");
    run("layercolor walls 0.8,0.1,0.1");
    run("layerweight walls 0.5");
    for i in 0..24 {
        let x = i as f64 * 3.1;
        run(&format!("box {x},0,0 2.2,1.3,{}", 1.0 + i as f64 * 0.17));
        run(&format!("rect {x},5,0 2.5 1.75"));
        run("extrude last 2.4");
        run(&format!("circle {x},10,0 {}", 0.3 + i as f64 * 0.05));
        if i % 3 == 0 {
            run("lineweight last 0.35");
        }
    }
    run("layer annotations");
    run("layercolor annotations 0.1,0.2,0.9");
    run("layerweight annotations 0.18");
    for i in 0..12 {
        let x = i as f64 * 4.0;
        run(&format!("line {x},-3,0 {},-1.5,0.25", x + 2.7));
        run(&format!("polyline {x},-6,0 {},-5.2,0 {},-7.1,0 closed", x + 1.3, x + 2.9));
        run(&format!("arc {x},-10,0 1.15 {} 250", i * 7));
        run(&format!("dim {x},-12,0 {},-12,0 0.6", x + 3.3));
        run(&format!("dimangular {x},-15,0 {},-15,0 {},-13.5,0 0.8", x + 1.0, x + 0.4));
        run(&format!("text {x},-18,0 Lot {i} north 0.3"));
    }
    run("rect 0,-25,0 6 4");
    run("hatch last crosshatch 30 0.4");
    run("rect 8,-25,0 6 4");
    run("hatch last ansi31 0.3");
    run("sheet s1 a1");
    run("sheetview s1 top 200");
    run("sheetview s1 front 200");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pre-#278 formatter, kept as the reference.
    fn trimmed_seq(v: f64, prec: usize) -> String {
        let mut s = format!("{v:.prec$}");
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.push('0');
        }
        s
    }

    #[test]
    fn push_trimmed_matches_format_then_trim() {
        let mut vals = vec![
            0.0, -0.0, 1.0, -1.0, 0.5, 10.0, 100.0, 1e-5, -1e-5, 0.00005, 0.00004999,
            123456.78901234, -98765.4321, 1e15, f64::MAX, f64::MIN, f64::NAN, f64::INFINITY,
            f64::NEG_INFINITY, f64::EPSILON,
        ];
        // Deterministic pseudo-random spread across magnitudes and signs.
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        for _ in 0..20_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let mag = 10f64.powi((x % 12) as i32 - 6);
            let frac = (x >> 11) as f64 / (1u64 << 53) as f64;
            vals.push((frac - 0.5) * 2.0 * mag);
        }
        for prec in [2, 4, 9] {
            let mut out = String::from("x=");
            for &v in &vals {
                out.truncate(2);
                push_trimmed(&mut out, v, prec);
                assert_eq!(&out[2..], trimmed_seq(v, prec), "v={v:e} prec={prec}");
            }
        }
    }

    /// #278 benchmark on a saved site (e.g. the perf-site fixture). Best of 5,
    /// sequential reference versus the parallel exporter, bytes checked equal.
    /// `ITSJUSTCAD_BENCH_SITE=<site.json> cargo test --release -p
    /// itsjustcad-commands --lib segments::tests::export_bench -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn export_bench() {
        let path = std::env::var("ITSJUSTCAD_BENCH_SITE").expect("set ITSJUSTCAD_BENCH_SITE");
        let mut s = crate::io::load_file(std::path::Path::new(&path)).unwrap();
        for line in ["sheet bench a1", "sheetview bench top 5000"] {
            s.run(crate::parse(line).unwrap()).unwrap();
        }
        let sheet = s.doc.sheet("bench").unwrap().clone();
        fn best<T: PartialEq>(f: impl Fn() -> T) -> (std::time::Duration, T) {
            let mut out = None;
            let mut t_best = std::time::Duration::MAX;
            for _ in 0..5 {
                let t = std::time::Instant::now();
                let r = f();
                t_best = t_best.min(t.elapsed());
                out = Some(r);
            }
            (t_best, out.unwrap())
        }
        let doc = &s.doc;
        let (t, _) = best(|| crate::svg::project_scene(doc).total_segs);
        eprintln!("svg project_scene (par): {t:.2?}");
        let meshes: Vec<&kernel_mesh::Mesh> = doc
            .objects()
            .filter_map(|o| match &o.geometry {
                itsjustcad_doc::Geometry::Mesh(m)
                | itsjustcad_doc::Geometry::Frame { mesh: m, .. }
                | itsjustcad_doc::Geometry::Area { mesh: m, .. }
                | itsjustcad_doc::Geometry::Parametric { mesh: m, .. } => Some(m),
                _ => None,
            })
            .collect();
        let (t, n) = best(|| meshes.iter().map(|m| kernel_mesh::feature_edges(m).len()).sum::<usize>());
        eprintln!("feature_edges, {} meshes, {n} edges, one core: {t:.2?}", meshes.len());
        let (a, x) = best(|| crate::svg::seq_ref::export_svg_seq(doc).0);
        let (b, y) = best(|| crate::svg::export_svg(doc).0);
        assert!(x == y);
        eprintln!("svg: seq {a:.2?} par {b:.2?} ({} bytes)", y.len());
        let (a, x) = best(|| sequential(|| crate::pdf::sheet_pdf(doc, &sheet).0));
        let (b, y) = best(|| crate::pdf::sheet_pdf(doc, &sheet).0);
        assert!(x == y);
        eprintln!("pdf: seq {a:.2?} par {b:.2?} ({} bytes)", y.len());
        let (a, x) = best(|| crate::dxf::seq_ref::document_dxf_seq(doc).0);
        let (b, y) = best(|| crate::dxf::document_dxf(doc).0);
        assert!(x == y);
        eprintln!("dxf: seq {a:.2?} par {b:.2?} ({} bytes)", y.len());
    }
}
