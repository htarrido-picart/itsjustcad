// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Session metadata kept outside the op-log: saved optimizer runs and the
//! cutting preset.

use std::collections::BTreeMap;

use itsjustcad_doc::ObjectId;

use super::Session;

impl Session {
    /// The saved optimizer run of the site plan on `source` (#201), while
    /// that site plan exists.
    pub fn optimize_run(&self, source: ObjectId) -> Option<&crate::optimize_runs::SavedRun> {
        self.optimize_runs
            .get(&source)
            .filter(|_| crate::optimize_runs::has_site_plan(&self.doc, source))
    }

    /// Save `run` as the latest run of the site plan on `source`, replacing
    /// the previous one. Not logged (no undo step); bumps
    /// [`Session::meta_revision`]. Refused (false) when `source` has no site
    /// plan.
    pub fn set_optimize_run(
        &mut self,
        source: ObjectId,
        run: crate::optimize_runs::SavedRun,
    ) -> bool {
        if !crate::optimize_runs::has_site_plan(&self.doc, source) {
            return false;
        }
        self.optimize_runs.insert(source, run);
        self.meta_rev += 1;
        true
    }

    /// Drop the saved run of the site plan on `source` (#219: a cancelled
    /// run that checkpointed over none). Not logged; bumps
    /// [`Session::meta_revision`].
    pub fn clear_optimize_run(&mut self, source: ObjectId) {
        if self.optimize_runs.remove(&source).is_some() {
            self.meta_rev += 1;
        }
    }

    /// Every saved run whose site plan exists (the file header's).
    pub fn optimize_runs(&self) -> BTreeMap<ObjectId, crate::optimize_runs::SavedRun> {
        self.optimize_runs
            .iter()
            .filter(|(id, _)| crate::optimize_runs::has_site_plan(&self.doc, **id))
            .map(|(id, r)| (*id, r.clone()))
            .collect()
    }

    /// Seed the saved runs from a loaded file header.
    pub fn set_optimize_runs(
        &mut self,
        runs: BTreeMap<ObjectId, crate::optimize_runs::SavedRun>,
    ) {
        self.optimize_runs = runs;
    }

    /// The document's last cutting preset (#209), `None` when never chosen.
    pub fn cut_preset(&self) -> Option<crate::cut_preset::CutPreset> {
        self.cut_preset
    }

    /// Remember `preset` as the document's cutting preset. Not logged (no
    /// undo step); bumps [`Session::meta_revision`] when it changes.
    pub fn set_cut_preset(&mut self, preset: crate::cut_preset::CutPreset) {
        if self.cut_preset != Some(preset) {
            self.cut_preset = Some(preset);
            self.meta_rev += 1;
        }
    }

    /// Changes to the document metadata that is not in the op-log (saved
    /// optimizer runs, the cutting preset). A host saving the file records it next to its
    /// op-log cursor to tell whether the document is dirty.
    pub fn meta_revision(&self) -> u64 {
        self.meta_rev
    }

    /// A new edit that deleted a site plan (or its boundary) drops its run.
    pub(super) fn prune_optimize_runs(&mut self) {
        if !self.optimize_runs.is_empty() {
            let doc = &self.doc;
            self.optimize_runs
                .retain(|id, _| crate::optimize_runs::has_site_plan(doc, *id));
        }
    }
}
