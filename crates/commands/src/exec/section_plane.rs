// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! `sectionplane` (#359, SketchUp's Section Plane): named planes on the
//! document, one of them the active cut the viewport clips to. Every op
//! but `list` is logged with its names filled in, so a file replays the
//! same planes and cut; undo restores the planes and the active name.

use itsjustcad_doc::{Document, SectionPlane, section_plane_name};

use super::{ApplyOutcome, Inverse};
use crate::error::ExecError;
use crate::{Command, SectionPlaneOp, Selector};

/// Words `sectionplane` takes as sub-commands, which a plane cannot be named.
const SECTIONPLANE_WORDS: [&str; 8] =
    ["activate", "deactivate", "reverse", "delete", "slice", "list", "align", "off"];

fn invalid(msg: impl Into<String>) -> ExecError {
    ExecError::Invalid(format!("sectionplane: {}", msg.into()))
}

/// Whether `name` can name a plane: one word, no comma, not a sub-command.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains(|c: char| c.is_whitespace() || c == ',')
        && !SECTIONPLANE_WORDS.contains(&name.to_ascii_lowercase().as_str())
}

/// `name`, or the active plane's when absent; an error when there is none.
fn named_or_active(doc: &Document, name: Option<String>) -> Result<String, ExecError> {
    let name = match name {
        Some(n) => n,
        None => doc.active_section.clone().ok_or_else(|| invalid("no active section plane; name one"))?,
    };
    if doc.section_plane(&name).is_none() {
        return Err(invalid(format!("no section plane named '{name}'")));
    }
    Ok(name)
}

fn describe(p: &SectionPlane) -> String {
    let f = |v: glam::DVec3| format!("{:.3},{:.3},{:.3}", v.x, v.y, v.z);
    format!("{} at {} facing {}", p.name, f(p.point), f(p.normal))
}

pub(super) fn apply(doc: &mut Document, op: SectionPlaneOp) -> Result<(Command, Inverse, ApplyOutcome), ExecError> {
    let inverse = Inverse::SectionPlanes { planes: doc.section_planes.clone(), active: doc.active_section.clone() };
    let done = |op: SectionPlaneOp, inverse: Inverse, message: String| {
        Ok((Command::SectionPlane(op), inverse, ApplyOutcome { created: Vec::new(), message }))
    };
    match op {
        SectionPlaneOp::Place { name, point, normal } => {
            let name = match name {
                Some(n) if !valid_name(&n) => return Err(invalid(format!("'{n}' cannot name a plane"))),
                Some(n) => n,
                None => (1..)
                    .map(section_plane_name)
                    .find(|n| doc.section_plane(n).is_none())
                    .expect("a free name"),
            };
            let plane = SectionPlane::new(name.clone(), point, normal)
                .ok_or_else(|| invalid("the point and normal must be finite and the normal non-zero"))?;
            let message = format!("section plane {} (active cut)", describe(&plane));
            let normal = plane.normal;
            match doc.section_planes.iter_mut().find(|p| p.name == name) {
                Some(p) => *p = plane,
                None => doc.section_planes.push(plane),
            }
            doc.active_section = Some(name.clone());
            done(SectionPlaneOp::Place { name: Some(name), point, normal }, inverse, message)
        }
        SectionPlaneOp::Activate { name } => {
            if doc.section_plane(&name).is_none() {
                return Err(invalid(format!("no section plane named '{name}'")));
            }
            doc.active_section = Some(name.clone());
            let message = format!("section plane {name} is the active cut");
            done(SectionPlaneOp::Activate { name }, inverse, message)
        }
        SectionPlaneOp::Deactivate => {
            let message = match doc.active_section.take() {
                Some(n) => format!("section plane {n} deactivated: no active cut"),
                None => "no active cut".to_string(),
            };
            done(SectionPlaneOp::Deactivate, inverse, message)
        }
        SectionPlaneOp::Reverse { name } => {
            let name = named_or_active(doc, name)?;
            let p = doc.section_planes.iter_mut().find(|p| p.name == name).expect("checked");
            p.normal = -p.normal;
            let message = format!("section plane {} reversed", describe(p));
            done(SectionPlaneOp::Reverse { name: Some(name) }, inverse, message)
        }
        SectionPlaneOp::Delete { name } => {
            let name = named_or_active(doc, name)?;
            doc.section_planes.retain(|p| p.name != name);
            if doc.active_section.as_deref() == Some(&name) {
                doc.active_section = None;
            }
            done(SectionPlaneOp::Delete { name: Some(name.clone()) }, inverse, format!("section plane {name} deleted"))
        }
        SectionPlaneOp::Slice { name } => {
            let name = named_or_active(doc, name)?;
            let p = doc.section_plane(&name).expect("checked").clone();
            // Logged as the `section` op it runs, so replay needs no plane.
            super::apply_forward(
                doc,
                Command::Section { ids: None, targets: Selector::All, point: p.point, normal: p.normal },
            )
        }
        SectionPlaneOp::List => {
            let lines: Vec<String> = doc
                .section_planes
                .iter()
                .map(|p| {
                    let active = doc.active_section.as_deref() == Some(p.name.as_str());
                    format!("{}{}", describe(p), if active { " (active)" } else { "" })
                })
                .collect();
            let message =
                if lines.is_empty() { "no section planes".to_string() } else { lines.join("\n") };
            done(SectionPlaneOp::List, inverse, message)
        }
    }
}
