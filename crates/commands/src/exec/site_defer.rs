// SPDX-License-Identifier: AGPL-3.0-only

//! Deferred site regeneration during history-less replay (#218).
//!
//! A replay that keeps no inverses (a fast-open tail, an open without a
//! checkpoint) only needs the final document, so a run of consecutive edits
//! to one associative link (`streetmove`/`streetnode`/…, `streetset`,
//! `lotsetparams`, `lotrefresh`) regenerates it once, not once per op. Each op
//! of the run still runs and validates as recorded, and stores the link exactly
//! as an eager run would; only the derived objects are left as *stubs*: just
//! the plan's road objects (the ones the next street edit targets by id), with
//! the recorded ids. The run is flushed (the link regenerated with the last
//! op's ids, removing the stubs) before any op that is not one of these edits
//! on the same link, and at the end of the replay, so no other op ever sees a
//! stub. Regeneration is deterministic and appends its objects, so the result
//! equals the eager replay: objects, ids, creation order, link and
//! `generation` (stub inserts are accounted as the eager inserts).
//!
//! Live editing and the history replays never enable this.

use std::cell::RefCell;

use itsjustcad_doc::{Document, ObjectId, SubdivKind, SubdivLink};

use crate::error::ExecError;
use crate::{Command, Selector};

thread_local! {
    /// `Some` while a deferring replay runs on this thread: the link whose
    /// derived objects are stubs, if any, and the verb that left them.
    static DEFER: RefCell<Option<Option<(ObjectId, String)>>> = const { RefCell::new(None) };
}

thread_local! {
    /// While deferring: the last few `thin_blocks` counts by their exact
    /// inputs. A street edit validates its graph after against before, and
    /// its before is the previous edit's after.
    static THIN: RefCell<Vec<(Vec<u64>, usize)>> = const { RefCell::new(Vec::new()) };
}

#[cfg(test)]
thread_local! {
    /// `thin_blocks` computations on this thread.
    pub(crate) static THIN_COMPUTED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Site/lot regenerations run on this thread (`regenerate_by_kind`).
    pub(crate) static REGENERATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Deferral is on until this is dropped.
pub(super) struct Deferring(());

impl Drop for Deferring {
    fn drop(&mut self) {
        DEFER.with(|d| *d.borrow_mut() = None);
        THIN.with(|t| t.borrow_mut().clear());
    }
}

/// `streets::thin_blocks` (`count`), reused while deferring when the site,
/// settings and graph are bit-identical to a recent call.
pub(crate) fn memo_thin_blocks(
    site: &subdivision::Polygon2d,
    graph: &subdivision::StreetGraph,
    settings: &subdivision::SubdivisionSettings,
    count: impl FnOnce() -> usize,
) -> usize {
    #[cfg(test)]
    let count = || {
        THIN_COMPUTED.with(|n| n.set(n.get() + 1));
        count()
    };
    if !DEFER.with(|d| d.borrow().is_some()) {
        return count();
    }
    let bits = |v: glam::DVec2| [v.x.to_bits(), v.y.to_bits()];
    let mut key: Vec<u64> = site.verts().iter().flat_map(|p| bits(*p)).collect();
    key.push(u64::MAX);
    for s in &graph.streets {
        key.extend([u64::from(s.id), s.tier as u64, s.width.to_bits(), s.centerline.len() as u64]);
        key.extend(s.centerline.iter().flat_map(|p| bits(*p)));
    }
    key.extend(serde_json::to_string(settings).unwrap_or_default().bytes().map(u64::from));
    if let Some(n) = THIN.with(|t| t.borrow().iter().find(|(k, _)| *k == key).map(|e| e.1)) {
        return n;
    }
    let n = count();
    THIN.with(|t| {
        let mut t = t.borrow_mut();
        t.insert(0, (key, n));
        t.truncate(3);
    });
    n
}

/// Turn deferral on for this thread (see the module docs).
pub(super) fn begin() -> Deferring {
    DEFER.with(|d| *d.borrow_mut() = Some(None));
    Deferring(())
}

/// Whether `source`'s derived objects are stubs; forgets it if so (its
/// caller regenerates or re-stubs it).
pub(super) fn take_stub_of(source: ObjectId) -> bool {
    DEFER.with(|d| match &mut *d.borrow_mut() {
        Some(stub @ Some(_)) if stub.as_ref().is_some_and(|(s, _)| *s == source) => {
            *stub = None;
            true
        }
        _ => false,
    })
}

/// The link `cmd` re-derives without reading any derived object other than
/// the plan's roads, or `None` when it may read more (or fails anyway): only
/// such ops can run on stubs.
fn deferrable_source(doc: &Document, cmd: &Command) -> Option<ObjectId> {
    match cmd {
        Command::LotRefresh { source, .. } | Command::LotSetParams { source, .. } => Some(*source),
        Command::StreetSet { targets: Selector::Ids { ids }, .. }
        | Command::StreetGeometry { targets: Selector::Ids { ids }, .. } => {
            let mut source = None;
            for id in ids {
                doc.get(*id)?;
                let s = doc.subdiv_source_of(*id).or_else(|| {
                    doc.subdivision_links.contains_key(id).then_some(*id)
                })?;
                if source.is_some_and(|x| x != s) {
                    return None;
                }
                source = Some(s);
            }
            source
        }
        _ => None,
    }
}

/// Flush the stubs before `cmd` unless it continues their run.
pub(super) fn before(doc: &mut Document, cmd: &Command) -> Result<(), ExecError> {
    let stub = DEFER.with(|d| d.borrow().as_ref().and_then(|s| s.as_ref().map(|(src, _)| *src)));
    match stub {
        Some(src) if deferrable_source(doc, cmd) != Some(src) => flush(doc),
        _ => Ok(()),
    }
}

/// Regenerate the stubbed link, if any, with its stored ids. Leaves
/// `generation` as is: the stub op already counted the eager regeneration.
pub(super) fn flush(doc: &mut Document) -> Result<(), ExecError> {
    let Some(Some((source, verb))) = DEFER.with(|d| d.borrow().clone()) else {
        return Ok(());
    };
    // Off while flushing, so the regeneration is real.
    let saved = DEFER.with(|d| d.borrow_mut().take());
    let link = doc.subdivision_links[&source].clone();
    let ids = Some(link.produced.clone());
    let generation = doc.generation;
    let out = super::rederive_link(doc, source, link, ids, &verb);
    doc.generation = generation;
    DEFER.with(|d| *d.borrow_mut() = saved.map(|_| None));
    out.map(|_| ())
}

/// Stand stubs in for regenerating `link` at `source` (`rederive_link`, after
/// it removed the old children): when deferring, with recorded `ids` and a
/// link whose regeneration can neither drop hand overrides nor create a
/// layer, insert only the roads under the leading ids, store the link as the
/// eager regeneration would, and return the ids. `None` = regenerate now.
pub(super) fn stub(
    doc: &mut Document,
    source: ObjectId,
    link: &SubdivLink,
    poly: &subdivision::Polygon2d,
    z: f64,
    ids: Option<&[ObjectId]>,
    verb: &str,
) -> Option<Vec<ObjectId>> {
    use crate::lot;
    let ids = ids?;
    if !DEFER.with(|d| d.borrow().is_some()) {
        return None;
    }
    let has = |l: &str| doc.layers.contains_key(l);
    let (layers_ok, overrides) = match link.kind {
        SubdivKind::Lots => (has(lot::LOTS_LAYER), !link.edge_roles.is_empty()),
        SubdivKind::Site => (has(lot::ROADS_LAYER) && has(lot::BLOCKS_LAYER), false),
        SubdivKind::SitePlan => (
            [lot::ROADS_LAYER, lot::BLOCKS_LAYER, lot::LOTS_LAYER, lot::SETBACKS_LAYER, lot::BUILDINGS_LAYER]
                .iter()
                .all(|l| has(l))
                && (link.settings.open_space_reserve_frac <= 0.0 || has(lot::OPENSPACE_LAYER)),
            !link.edge_roles.is_empty() || !link.lot_overrides.is_empty(),
        ),
    };
    if !layers_ok || overrides {
        return None;
    }
    if link.kind != SubdivKind::Lots {
        let graph = lot::streets::edited_graph(poly, &link.settings, &link.streets);
        let n = graph.streets.len();
        if n == 0 || ids.len() < n {
            return None;
        }
        let generation = doc.generation;
        lot::insert_site(doc, &lot::road_bake(&graph, z, &link.streets), &ids[..n], &[]);
        doc.generation = generation;
    }
    // The eager regeneration's inserts, one per id, and its link bump.
    doc.generation += ids.len() as u64 + 1;
    doc.subdivision_links.insert(
        source,
        SubdivLink {
            produced: ids.to_vec(),
            source_z: z,
            frozen: false,
            ..link.clone()
        },
    );
    DEFER.with(|d| *d.borrow_mut() = Some(Some((source, verb.to_string()))));
    Some(ids.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{ReplayControl, Session};
    use crate::lot::StreetOp;
    use crate::{Command, Selector};
    use itsjustcad_doc::ObjectId;

    fn run(s: &mut Session, line: &str) {
        s.run(crate::parse(line).unwrap()).unwrap();
    }

    fn regenerations() -> usize {
        REGENERATIONS.with(|n| n.get())
    }

    /// The road object of street `street` on site `source`.
    fn road(s: &Session, source: ObjectId, street: u32) -> Option<ObjectId> {
        let link = s.doc.subdiv_link(source)?;
        link.produced.iter().copied().find(|id| {
            s.doc.get(*id).and_then(|o| o.street.as_ref()).is_some_and(|i| i.id == street)
        })
    }

    fn street_ids(s: &Session, source: ObjectId) -> Vec<u32> {
        let link = s.doc.subdiv_link(source).unwrap();
        link.produced
            .iter()
            .filter_map(|id| s.doc.get(*id)?.street.as_ref().map(|i| i.id))
            .collect()
    }

    /// Try a street edit; keep it only when it validates (live editing
    /// rejects the rest, so they never reach a log).
    fn try_edit(s: &mut Session, source: ObjectId, street: u32, op: StreetOp) -> bool {
        let Some(id) = road(s, source, street) else { return false };
        s.run(Command::StreetGeometry { targets: Selector::Ids { ids: vec![id] }, op, ids: None })
            .is_ok()
    }

    /// A site-plan session like iOS #168: two site plans, `edits` street
    /// moves (mostly consecutive on the first site), a param change.
    fn site_session(w: f64, edits: usize) -> (Session, ObjectId, ObjectId) {
        site_session_mix(w, edits, 5)
    }

    /// [`site_session`] with every `b_every`-th edit attempt on the second site.
    fn site_session_mix(w: f64, edits: usize, b_every: usize) -> (Session, ObjectId, ObjectId) {
        let mut s = Session::default();
        run(&mut s, &format!("rect 0,0,0 {w} {}", w * 0.75));
        let a = s.doc.objects().last().unwrap().id;
        run(&mut s, "siteplan last");
        run(&mut s, &format!("rect {},0,0 200 150", w + 50.0));
        let b = s.doc.objects().filter(|o| o.street.is_none()).last().unwrap().id;
        run(&mut s, &format!("siteplan id:{b}"));
        let mut done = 0;
        let mut k = 0usize;
        while done < edits && k < edits * 20 {
            let site = if k % b_every == b_every - 1 { b } else { a };
            let ids = street_ids(&s, site);
            let street = ids[k % ids.len()];
            let d = if (k / ids.len()).is_multiple_of(2) { 2.0 } else { -2.0 };
            let delta = if k.is_multiple_of(2) { glam::DVec2::new(d, 0.0) } else { glam::DVec2::new(0.0, d) };
            if try_edit(&mut s, site, street, StreetOp::Move { delta }) {
                done += 1;
            }
            k += 1;
        }
        assert_eq!(done, edits, "enough valid street edits");
        (s, a, b)
    }

    /// A deferred replay of `log`: the no-checkpoint open path.
    fn deferred(log: &[Command]) -> Session {
        Session::replay_deferred(log.to_vec(), ReplayControl::default()).unwrap()
    }

    /// The deferred replay of `s`'s log equals the eager one: document,
    /// saved log, file and checkpoint bytes. Returns the regenerations
    /// (eager, deferred).
    fn assert_same(s: &Session) -> (usize, usize) {
        let log = s.save_log();
        let r0 = regenerations();
        let eager = Session::replay(log.clone()).unwrap();
        let r1 = regenerations();
        let fast = deferred(&log);
        let r2 = regenerations();
        assert_eq!(fast.doc, eager.doc, "deferred replay equals eager");
        assert_eq!(fast.save_log(), eager.save_log());
        assert_eq!(crate::io::to_json(&fast), crate::io::to_json(&eager));
        assert_eq!(crate::io::checkpoint_json(&fast), crate::io::checkpoint_json(&eager));
        assert!(DEFER.with(|d| d.borrow().is_none()), "deferral off after replay");
        (r1 - r0, r2 - r1)
    }

    /// Any object of `layer` in `source`'s plan, picked by `k`.
    fn child(s: &Session, source: ObjectId, layer: &str, k: usize) -> Option<ObjectId> {
        let link = s.doc.subdiv_link(source)?;
        let ids: Vec<_> = link
            .produced
            .iter()
            .copied()
            .filter(|id| s.doc.get(*id).is_some_and(|o| o.layer == layer))
            .collect();
        (!ids.is_empty()).then(|| ids[k % ids.len()])
    }

    /// A random edit session: street edits (the deferred runs) mixed with
    /// every kind of op that reads or changes the derived objects between
    /// them. Failing ops are not logged, as live.
    fn random_session(seed: u64, steps: usize) -> Session {
        let (mut s, a, b) = site_session(400.0, 2);
        let mut x = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let mut next = move || {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (x >> 33) as usize
        };
        for _ in 0..steps {
            let (r, k) = (next(), next());
            let site = if r % 7 == 0 { b } else { a };
            let road = child(&s, site, crate::lot::ROADS_LAYER, k);
            let block = child(&s, site, crate::lot::BLOCKS_LAYER, k);
            let lot = child(&s, site, crate::lot::LOTS_LAYER, k);
            let d = [-3.0, -1.5, 1.5, 3.0][k % 4];
            let line = match r % 20 {
                0..=6 => road.map(|id| {
                    let delta = if k.is_multiple_of(2) { format!("{d},0") } else { format!("0,{d}") };
                    format!("streetmove id:{id} delta={delta}")
                }),
                7 => road.map(|id| format!("streetset id:{id} width={}", 10 + k % 6)),
                8 => {
                    let _ = s.run(Command::LotRefresh { source: site, ids: None });
                    None
                }
                9 => {
                    let p = [("block_depth", "50"), ("block_depth", "60"), ("lot_width", "16")][k % 3];
                    let _ = s.run(Command::LotSetParams {
                        source: site,
                        params: [(p.0.to_string(), p.1.to_string())].into(),
                        ids: None,
                    });
                    None
                }
                10 => Some(format!("box {},{},0 2,2,2", 900 + k % 50, k % 40)),
                11 => block.map(|id| format!("name id:{id} b{k}")),
                12 => lot.map(|id| format!("move id:{id} 0.5,0,0")),
                13 => lot.map(|id| format!("delete id:{id}")),
                14 => lot.map(|id| format!("edgerole id:{id} edge=0 role=front")),
                15 => lot.map(|id| format!("lotheight id:{id} floors=4")),
                16 => {
                    // Move the boundary, then the app's auto refresh.
                    let _ = s.run(crate::parse(&format!("move id:{site} {d},0,0")).unwrap());
                    let _ = s.run(Command::LotRefresh { source: site, ids: None });
                    None
                }
                17 => block.map(|id| {
                    let _ = s.run(Command::StreetGeometry {
                        targets: Selector::Ids { ids: vec![id] },
                        op: StreetOp::Reset { all: true },
                        ids: None,
                    });
                    "move last 1 0,0,1".to_string()
                }),
                18 => {
                    let _ = s.run(crate::parse("layerlock roads on").unwrap());
                    let _ = road.map(|id| s.run(crate::parse(&format!("streetmove id:{id} delta=1,0")).unwrap()));
                    Some("layerlock roads off".into())
                }
                _ => road.map(|id| format!("streetnode id:{id} insert at={},{}", 100 + k % 200, 50 + k % 100)),
            };
            if let Some(line) = line {
                let _ = s.run(crate::parse(&line).unwrap());
            }
        }
        s
    }

    #[test]
    fn deferred_replay_equals_eager_on_street_edit_runs() {
        let (s, _, _) = site_session(400.0, 9);
        let (eager, fast) = assert_same(&s);
        // The site of each street edit, in log order.
        let mut r = Session::default();
        let mut sites = Vec::new();
        for c in s.save_log() {
            if let Command::StreetGeometry { targets: Selector::Ids { ids }, .. } = &c {
                sites.push(r.doc.subdiv_source_of(ids[0]).unwrap());
            }
            r.run(c).unwrap();
        }
        assert_eq!(sites.len(), 9);
        sites.dedup();
        assert!(sites.len() < 9, "some edits follow on the same site");
        assert_eq!(fast, eager - 9 + sites.len(), "one regeneration per run of edits to a site");
        // Each edit validates after against before; a run reuses the count of
        // the previous edit's after.
        let thin = || THIN_COMPUTED.with(|n| n.get());
        let t0 = thin();
        Session::replay(s.save_log()).unwrap();
        let t1 = thin();
        deferred(&s.save_log());
        let t2 = thin();
        assert_eq!(t1 - t0, 18);
        assert_eq!(t2 - t1, 18 - (9 - sites.len()));
    }

    #[test]
    fn deferred_replay_equals_eager_on_random_logs() {
        for seed in 0..12 {
            let s = random_session(seed, 40);
            assert_same(&s);
        }
    }

    type Reader = dyn Fn(&Session, ObjectId) -> Option<String>;

    /// Each derived-state reader right after a street edit forces the flush
    /// (or runs on materialized objects) and replays identically.
    #[test]
    fn readers_mid_run_flush_first() {
        let readers: [&Reader; 9] = [
            &|s, a| child(s, a, crate::lot::BLOCKS_LAYER, 1).map(|id| format!("name id:{id} park")),
            &|s, a| child(s, a, crate::lot::LOTS_LAYER, 2).map(|id| format!("delete id:{id}")),
            &|s, a| child(s, a, crate::lot::LOTS_LAYER, 0).map(|id| format!("edgerole id:{id} edge=0 role=front")),
            &|s, a| child(s, a, crate::lot::LOTS_LAYER, 3).map(|id| format!("lotheight id:{id} floors=5")),
            &|s, a| child(s, a, crate::lot::BUILDINGS_LAYER, 0).map(|id| format!("copy id:{id} 0,0,10")),
            &|s, a| child(s, a, crate::lot::ROADS_LAYER, 0).map(|id| format!("name id:{id} main")),
            &|_, _| Some("move last 2 0,0,1".into()),
            &|_, a| Some(format!("move id:{a} 1,0,0")),
            &|_, _| Some("layerlock lots on".into()),
        ];
        for (i, reader) in readers.iter().enumerate() {
            let (mut s, a, _) = site_session(400.0, 2);
            let line = reader(&s, a).unwrap_or_else(|| panic!("reader {i} has a target"));
            s.run(crate::parse(&line).unwrap()).unwrap_or_else(|e| panic!("reader {i}: {e}"));
            // More street edits after the reader, then a stubbed end.
            for street in street_ids(&s, a).into_iter().take(3) {
                try_edit(&mut s, a, street, StreetOp::Move { delta: glam::DVec2::new(1.5, 0.0) });
            }
            assert_same(&s);
        }
    }

    /// A checkpoint taken mid-run: the fast-open tail defers too.
    #[test]
    fn snapshot_tail_defers_and_equals_eager() {
        let (s, _, _) = site_session(400.0, 9);
        let log = s.save_log();
        for k in [3, log.len() - 4, log.len() - 1] {
            let snap = Session::replay(log[..k].to_vec()).unwrap().doc;
            let tail = Session::from_snapshot_tail(snap, log.clone(), k, ReplayControl::default()).unwrap();
            assert_eq!(tail.doc, s.doc, "tail from op {k}");
            assert_eq!(tail.save_log(), log);
        }
    }

    /// Undo after a deferred open rebuilds the history (debug builds also
    /// check the rebuilt document equals the opened one) and matches eager.
    #[test]
    fn undo_after_deferred_open_matches_eager() {
        let (s, _, _) = site_session(400.0, 6);
        let log = s.save_log();
        let mut fast = deferred(&log);
        assert!(fast.history_pending());
        let mut eager = Session::replay(log).unwrap();
        for _ in 0..4 {
            run(&mut fast, "undo");
            run(&mut eager, "undo");
            assert_eq!(fast.doc, eager.doc);
        }
        run(&mut fast, "redo");
        run(&mut eager, "redo");
        assert_eq!(fast.doc, eager.doc);
    }

    /// A no-checkpoint open (#341) leaves the history pending like a fast
    /// open (#224): an edit does not replay, undo within new edits needs no
    /// rebuild, and undo past the open waits for the background rebuild,
    /// then walks the pre-open states.
    #[test]
    fn edit_after_deferred_open_does_not_replay() {
        let rebuilds = || crate::exec::HISTORY_REBUILDS.with(std::cell::Cell::get);
        let (s, _, _) = site_session(400.0, 6);
        let log = s.save_log();
        let json = crate::io::to_json(&s);
        let mut fast = crate::io::open_json(&json, None, ReplayControl::default()).unwrap().session;
        fast.set_background_history(true);
        let mut eager = Session::replay(log.clone()).unwrap();
        let n0 = rebuilds();
        for line in ["location 40.7 -74.0 10", "undo"] {
            run(&mut fast, line);
            run(&mut eager, line);
        }
        assert_eq!(rebuilds(), n0, "no history replay on the edit or its undo");
        assert!(fast.history_pending());
        assert_eq!(fast.doc, eager.doc, "undo within new edits");
        assert_eq!(fast.run(Command::Undo).unwrap_err(), crate::ExecError::HistoryPending);
        assert!(fast.history_requested());
        let job = fast.history_job().unwrap();
        for line in ["redo", "box 900,0,0 1,1,1"] {
            run(&mut fast, line);
            run(&mut eager, line);
        }
        assert!(fast.install_history(job.run(ReplayControl::default()).unwrap()));
        assert_eq!(rebuilds(), n0 + 1);
        assert_eq!(fast.save_log().len(), log.len() + 2);
        assert_eq!(&fast.save_log()[..log.len()], &log[..]);
        for _ in 0..6 {
            run(&mut fast, "undo");
            run(&mut eager, "undo");
            assert_eq!(fast.doc, eager.doc);
        }
    }

    /// Live edits never stub: every produced child exists after each edit,
    /// and each edit regenerates once, as before.
    #[test]
    fn live_editing_is_unchanged() {
        let (mut s, a, _) = site_session(400.0, 0);
        for street in street_ids(&s, a).into_iter().take(4) {
            let r0 = regenerations();
            if try_edit(&mut s, a, street, StreetOp::Move { delta: glam::DVec2::new(1.5, 0.0) }) {
                assert_eq!(regenerations() - r0, 1);
            }
            let link = s.doc.subdiv_link(a).unwrap();
            assert!(link.produced.iter().all(|id| s.doc.get(*id).is_some()));
        }
        assert!(!s.history_pending());
    }

    /// A saved log with two same-source `lotrefresh` in a row (an undone edit
    /// between them) saves the same merged log either way.
    #[test]
    fn lot_refresh_merge_matches_eager() {
        let (mut s, a, _) = site_session(400.0, 0);
        s.run(Command::LotRefresh { source: a, ids: None }).unwrap();
        run(&mut s, "box 900,0,0 1,1,1");
        run(&mut s, "undo");
        s.run(Command::LotRefresh { source: a, ids: None }).unwrap();
        let log = s.save_log();
        let eager = Session::replay(log.clone()).unwrap();
        assert!(eager.save_log().len() < log.len(), "eager replay merges the refreshes");
        assert_eq!(deferred(&log).save_log(), eager.save_log());
        assert_same(&s);
    }

    /// Replay/open time and regeneration count, eager vs deferred (#218).
    /// `BENCH_SITE_W=3000 [BENCH_B_EVERY=5] cargo test --release -p itsjustcad-commands --lib site_defer::tests::bench -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_replay_street_edits() {
        let w = std::env::var("BENCH_SITE_W").ok().and_then(|v| v.parse().ok()).unwrap_or(1500.0);
        let mix = std::env::var("BENCH_B_EVERY").ok().and_then(|v| v.parse().ok()).unwrap_or(5);
        let (s, _, _) = site_session_mix(w, 9, mix);
        let log = s.save_log();
        let n_street = log.iter().filter(|c| matches!(c, Command::StreetGeometry { .. })).count();
        println!("log: {} ops, {n_street} street_geometry", log.len());
        let best = |f: &dyn Fn() -> Session| {
            let mut best = f64::MAX;
            let mut regen = 0;
            for _ in 0..5 {
                let r0 = regenerations();
                let t = std::time::Instant::now();
                let out = f();
                best = best.min(t.elapsed().as_secs_f64());
                regen = regenerations() - r0;
                assert_eq!(out.doc, s.doc);
            }
            (best, regen)
        };
        let (t, r) = best(&|| Session::replay(log.clone()).unwrap());
        println!("eager replay:    {:.3} s, {r} regenerations", t);
        let json = crate::io::to_json(&s);
        let (t, r) = best(&|| crate::io::open_json(&json, None, ReplayControl::default()).unwrap().session);
        println!("open (no checkpoint, deferred): {:.3} s, {r} regenerations", t);
        // First edit after the open (#224): a pending history is not rebuilt.
        // The rebuild is what a first edit used to cost before #225.
        let (mut edit, mut rebuild) = (f64::MAX, f64::MAX);
        for _ in 0..5 {
            let mut fast = crate::io::open_json(&json, None, ReplayControl::default()).unwrap().session;
            let t = std::time::Instant::now();
            run(&mut fast, "location 40.7 -74.0 10");
            edit = edit.min(t.elapsed().as_secs_f64());
            assert!(fast.history_pending());
            let job = fast.history_job().unwrap();
            let t = std::time::Instant::now();
            assert!(fast.install_history(job.run(ReplayControl::default()).unwrap()));
            rebuild = rebuild.min(t.elapsed().as_secs_f64());
        }
        println!("first edit: {:.6} s; history rebuild it no longer waits for: {:.3} s", edit, rebuild);
    }
}
