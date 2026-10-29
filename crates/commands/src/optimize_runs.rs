// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Saved site optimizer runs (#201): the latest run of each site plan,
//! kept in the document next to its site plan link so it travels with the
//! file.
//!
//! A run is document metadata, like the file's uuid or its branches: it is
//! saved in the file header (`optimize_runs`), never in the op-log, so it is
//! not an undo step and replay never touches it. A finished run arriving
//! mid-edit therefore never lands between the user's edits on the undo
//! stack, and undoing a plan edit only flips the run's stale marker.
//!
//! Each run carries a fingerprint of the plan it searched (boundary,
//! settings, hand edits). When the plan's fingerprint no longer matches, the
//! hosts show "Plan changed since this run"; Apply still works.
//!
//! A run lives while its source has a `SitePlan` link: deleting the site
//! plan (or its boundary) drops it at that edit. A new run replaces it.
//! Older builds ignore the header field; entries this build cannot read
//! (a newer `version`, a bad shape) load as no run.

use std::collections::BTreeMap;

use itsjustcad_doc::{Document, ObjectId, SubdivKind};
use serde::{Deserialize, Serialize};
use subdivision::{Polygon2d, SubdivisionSettings};

use crate::site_optimize::{self, OptimizeRequest, OptimizeResult, PlanEdits, Prepared};

/// The saved run format this build writes and reads.
pub const SAVED_RUN_VERSION: u32 = 1;

/// The latest search of one site plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedRun {
    pub version: u32,
    /// The request the search ran with (the document's location filled in).
    pub request: OptimizeRequest,
    /// Its ranked plans and stats (`stopped_early` when cut short).
    pub result: OptimizeResult,
    /// [`fingerprint`] of the plan the search started from.
    pub fingerprint: String,
    /// When it finished, Unix seconds.
    pub finished_at: u64,
    /// How long it ran, seconds.
    pub elapsed_s: f64,
}

impl SavedRun {
    /// The run of `prepared` that produced `result` in `elapsed_s`, finished
    /// now.
    pub fn new(prepared: &Prepared, result: OptimizeResult, elapsed_s: f64) -> SavedRun {
        SavedRun {
            version: SAVED_RUN_VERSION,
            request: prepared.req.clone(),
            result,
            fingerprint: fingerprint(&prepared.site, &prepared.base, &prepared.edits),
            finished_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs()),
            elapsed_s,
        }
    }

    /// `{"version","request","result","stopped_early","finished_at",
    /// "elapsed_s","evaluated","feasible","budget","fingerprint","stale"}`;
    /// `result` is the `ijc_site_optimize_json` result.
    pub fn to_json(&self, stale: bool) -> serde_json::Value {
        serde_json::json!({
            "version": self.version,
            "request": self.request,
            "result": self.result.to_json(),
            "stopped_early": self.result.stopped_early,
            "finished_at": self.finished_at,
            "elapsed_s": self.elapsed_s,
            "evaluated": self.result.evaluated,
            "feasible": self.result.feasible,
            "budget": self.request.budget,
            "fingerprint": self.fingerprint,
            "stale": stale,
        })
    }
}

/// A stable hash (FNV-1a 64, hex) of what a search plans: the boundary
/// (to the mm), the settings and the hand edits.
pub fn fingerprint(site: &Polygon2d, base: &SubdivisionSettings, edits: &PlanEdits) -> String {
    let mm = |v: f64| (v * 1000.0).round() as i64;
    let site: Vec<(i64, i64)> = site.verts().iter().map(|p| (mm(p.x), mm(p.y))).collect();
    let text = serde_json::json!({
        "site": site,
        "settings": base,
        "streets": edits.streets,
        "edge_roles": edits.edge_roles,
        "lot_overrides": edits.lot_overrides,
    })
    .to_string();
    let hash = text.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3)
    });
    format!("{hash:016x}")
}

/// The fingerprint of `source`'s plan as the document has it now (`None`
/// when it is no longer a closed boundary).
pub fn plan_fingerprint(doc: &Document, source: ObjectId) -> Option<String> {
    let (_, site) = site_optimize::site_boundary(doc, &[source])?;
    let (base, edits) = site_optimize::base_plan(doc, source);
    Some(fingerprint(&site, &base, &edits))
}

/// Whether `source` has a site plan a run can be saved next to.
pub fn has_site_plan(doc: &Document, source: ObjectId) -> bool {
    doc.get(source).is_some()
        && doc
            .subdivision_links
            .get(&source)
            .is_some_and(|l| l.kind == SubdivKind::SitePlan)
}

/// Whether the plan changed since `run` (or is gone).
pub fn is_stale(doc: &Document, source: ObjectId, run: &SavedRun) -> bool {
    plan_fingerprint(doc, source).as_deref() != Some(run.fingerprint.as_str())
}

/// The runs a file header holds, keyed by source id: unreadable entries (a
/// bad id, a newer version, a bad shape) are dropped.
pub fn from_header(map: BTreeMap<String, serde_json::Value>) -> BTreeMap<ObjectId, SavedRun> {
    map.into_iter()
        .filter_map(|(k, v)| {
            let id = ObjectId(k.parse().ok()?);
            let version = v.get("version")?.as_u64()?;
            if version != SAVED_RUN_VERSION as u64 {
                return None;
            }
            Some((id, serde_json::from_value(v).ok()?))
        })
        .collect()
}

/// The file header form of `runs`.
pub fn to_header(runs: &BTreeMap<ObjectId, SavedRun>) -> BTreeMap<String, serde_json::Value> {
    runs.iter()
        .filter_map(|(id, r)| Some((id.0.to_string(), serde_json::to_value(r).ok()?)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::site_optimize::{Control, Objective};
    use crate::{Command, Selector, Session, io, parse};

    /// A multi-street site plan and its boundary.
    fn site() -> (Session, ObjectId) {
        let mut s = Session::default();
        for line in ["rect 0,0,0 200 150", "lotsettings depth=20", "siteplan last"] {
            s.run(parse(line).unwrap()).unwrap();
        }
        let src = *s.doc.subdivision_links.keys().next().unwrap();
        (s, src)
    }

    fn run_of(s: &Session, src: ObjectId) -> SavedRun {
        let req = OptimizeRequest {
            budget: 16,
            objective: Objective::Lots,
            ..OptimizeRequest::default()
        };
        let p = site_optimize::prepare_doc(&s.doc, &[src], &req).unwrap();
        let r = p.run(Control::default()).unwrap();
        SavedRun::new(&p, r, 1.5)
    }

    #[test]
    fn saved_run_round_trips_through_the_file() {
        let (mut s, src) = site();
        let plain = io::to_json(&s);
        assert!(!plain.contains("optimize_runs"), "no runs, no header field");
        let rev = s.meta_revision();
        let ops = s.undo_len();
        assert!(s.set_optimize_run(src, run_of(&s, src)));
        assert_eq!(s.meta_revision(), rev + 1, "metadata changed");
        assert_eq!(s.undo_len(), ops, "not an undo step");

        let back = io::from_json(&io::to_json(&s)).unwrap();
        let (a, b) = (s.optimize_run(src).unwrap(), back.optimize_run(src).unwrap());
        assert_eq!(a.to_json(false), b.to_json(false));
        assert!(!is_stale(&back.doc, src, b));
        assert_eq!(b.to_json(false)["result"]["stopped_early"], false);
        // A new run replaces it.
        let mut later = run_of(&s, src);
        later.finished_at += 60;
        s.set_optimize_run(src, later.clone());
        assert_eq!(s.optimize_run(src).unwrap().finished_at, later.finished_at);
        assert_eq!(s.optimize_runs().len(), 1);
    }

    #[test]
    fn old_and_newer_files_load_with_no_run() {
        let (mut s, src) = site();
        // An old file: no header field.
        assert!(io::from_json(&io::to_json(&s)).unwrap().optimize_run(src).is_none());
        // A newer run version and a bad entry are skipped; the file loads.
        s.set_optimize_run(src, run_of(&s, src));
        let mut v: serde_json::Value = serde_json::from_str(&io::to_json(&s)).unwrap();
        let runs = v["optimize_runs"].as_object_mut().unwrap();
        let entry = runs.values_mut().next().unwrap();
        entry["version"] = 2.into();
        runs.insert("not-an-id".into(), serde_json::json!({"version": 1}));
        let back = io::from_json(&v.to_string()).unwrap();
        assert!(back.optimize_run(src).is_none());
        assert_eq!(back.undo_len(), s.undo_len());
    }

    #[test]
    fn plan_edits_mark_the_run_stale_and_undo_clears_it() {
        let (mut s, src) = site();
        s.set_optimize_run(src, run_of(&s, src));
        let stale = |s: &Session| is_stale(&s.doc, src, s.optimize_run(src).unwrap());
        assert!(!stale(&s));
        s.run(Command::LotSetParams {
            source: src,
            params: [("road_width".to_string(), "14".to_string())].into(),
            ids: None,
        })
        .unwrap();
        assert!(stale(&s), "plan changed since this run");
        s.run(Command::Undo).unwrap();
        assert!(!stale(&s));
    }

    #[test]
    fn deleting_the_site_plan_drops_the_run() {
        let (mut s, src) = site();
        s.set_optimize_run(src, run_of(&s, src));
        s.run(Command::LotRemove {
            targets: Selector::Ids { ids: vec![src] },
        })
        .unwrap();
        assert!(s.optimize_run(src).is_none());
        // Dropped, not hidden: undoing the delete does not bring it back.
        s.run(Command::Undo).unwrap();
        assert!(s.doc.subdivision_links.contains_key(&src));
        assert!(s.optimize_run(src).is_none());
        assert!(!io::to_json(&s).contains("optimize_runs"));
        // No site plan: nothing to save next to.
        let mut bare = Session::default();
        bare.run(parse("rect 0,0,0 200 150").unwrap()).unwrap();
        let id = *bare.doc.all_ids().first().unwrap();
        assert!(!bare.set_optimize_run(id, run_of(&s, src)));
    }
}
