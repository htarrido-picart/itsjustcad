// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Parametric-generator model + schema (M-parametric).
//!
//! The expressive/form-finding generators (`geodesic`, `hypar`, `gaussvault`,
//! `gridshell`, `funicular`, `tensegrity`, `cablenet`, `spaceframe`) are here
//! turned into **live parametric objects**: an object stores its
//! [`GeneratorKind`] plus a [`ParamMap`] of typed values, and its triangle mesh
//! is a *derived cache* re-baked by [`derive_mesh`] on any param change
//! (mirroring the `Frame`/`Area` derived-mesh precedent). A param change is one
//! logged op and re-derivation is a pure function of `(generator, params)`, so
//! the result is deterministic + replay-stable.
//!
//! Each generator declares a [`ParamSchema`] (via [`GeneratorKind::schema`]) —
//! the *single source* the verb parser, the Parameters editor UI, and the deck
//! catalog all read, so they cannot drift. A completeness test asserts every
//! generator has a schema whose every field round-trips through its default.
//!
//! `minsurf` is intentionally NOT parametric here: it derives from an external
//! boundary curve, not a self-contained numeric parameter set, so it stays a
//! plain `Geometry::Mesh` (see the M-parametric PHASES note).

use glam::DVec3;
use kernel_mesh::Mesh;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The built-in generator behind a parametric object. Serde uses a stable
/// snake_case tag so saved files round-trip; the token also matches the verb.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GeneratorKind {
    Geodesic,
    SpaceFrame,
    Hypar,
    GaussVault,
    Gridshell,
    Funicular,
    Tensegrity,
    Cablenet,
}

impl GeneratorKind {
    /// Every parametric generator, for completeness tests + catalog listing.
    pub const ALL: &'static [GeneratorKind] = &[
        GeneratorKind::Geodesic,
        GeneratorKind::SpaceFrame,
        GeneratorKind::Hypar,
        GeneratorKind::GaussVault,
        GeneratorKind::Gridshell,
        GeneratorKind::Funicular,
        GeneratorKind::Tensegrity,
        GeneratorKind::Cablenet,
    ];

    /// Stable token (matches the creating verb): `geodesic`, `hypar`, …
    pub fn token(self) -> &'static str {
        match self {
            GeneratorKind::Geodesic => "geodesic",
            GeneratorKind::SpaceFrame => "spaceframe",
            GeneratorKind::Hypar => "hypar",
            GeneratorKind::GaussVault => "gaussvault",
            GeneratorKind::Gridshell => "gridshell",
            GeneratorKind::Funicular => "funicular",
            GeneratorKind::Tensegrity => "tensegrity",
            GeneratorKind::Cablenet => "cablenet",
        }
    }

    /// i18n key for the human label of this generator kind.
    pub fn label_key(self) -> &'static str {
        match self {
            GeneratorKind::Geodesic => "param.gen.geodesic",
            GeneratorKind::SpaceFrame => "param.gen.spaceframe",
            GeneratorKind::Hypar => "param.gen.hypar",
            GeneratorKind::GaussVault => "param.gen.gaussvault",
            GeneratorKind::Gridshell => "param.gen.gridshell",
            GeneratorKind::Funicular => "param.gen.funicular",
            GeneratorKind::Tensegrity => "param.gen.tensegrity",
            GeneratorKind::Cablenet => "param.gen.cablenet",
        }
    }

    /// The declared parameter schema for this generator. This is the single
    /// source for the verb parser, the editor UI, and the deck catalog.
    pub fn schema(self) -> ParamSchema {
        match self {
            GeneratorKind::Geodesic => ParamSchema {
                kind: self,
                fields: vec![
                    ParamField::int("frequency", "param.geodesic.frequency", 3, 1, 6, 1, Widget::Slider),
                    ParamField::float("radius", "param.geodesic.radius", 5.0, Some(0.1), Some(50.0), 0.1, Widget::Numeric, Unit::Meter),
                    ParamField::enum_("mode", "param.geodesic.mode", "dome", &["dome", "full"]),
                ],
            },
            GeneratorKind::SpaceFrame => ParamSchema {
                kind: self,
                fields: vec![
                    ParamField::int("nx", "param.spaceframe.nx", 4, 1, 32, 1, Widget::Slider),
                    ParamField::int("ny", "param.spaceframe.ny", 3, 1, 32, 1, Widget::Slider),
                    ParamField::float("bay", "param.spaceframe.bay", 3.0, Some(0.1), Some(20.0), 0.1, Widget::Numeric, Unit::Meter),
                    ParamField::float("depth", "param.spaceframe.depth", 1.5, Some(0.1), Some(20.0), 0.1, Widget::Numeric, Unit::Meter),
                ],
            },
            GeneratorKind::Hypar => ParamSchema {
                kind: self,
                fields: vec![
                    ParamField::float("a", "param.hypar.a", 5.0, Some(0.1), Some(50.0), 0.1, Widget::Slider, Unit::Meter),
                    ParamField::float("b", "param.hypar.b", 5.0, Some(0.1), Some(50.0), 0.1, Widget::Slider, Unit::Meter),
                    ParamField::float("c", "param.hypar.c", 5.0, Some(-50.0), Some(50.0), 0.1, Widget::Slider, Unit::Meter),
                    ParamField::int("nu", "param.hypar.nu", 12, 2, 64, 1, Widget::Slider),
                    ParamField::int("nv", "param.hypar.nv", 12, 2, 64, 1, Widget::Slider),
                ],
            },
            GeneratorKind::GaussVault => ParamSchema {
                kind: self,
                fields: vec![
                    ParamField::float("span", "param.gaussvault.span", 6.0, Some(0.1), Some(50.0), 0.1, Widget::Slider, Unit::Meter),
                    ParamField::float("length", "param.gaussvault.length", 12.0, Some(0.1), Some(100.0), 0.1, Widget::Slider, Unit::Meter),
                    ParamField::float("rise", "param.gaussvault.rise", 3.0, Some(0.1), Some(30.0), 0.1, Widget::Slider, Unit::Meter),
                    ParamField::bool("undulate", "param.gaussvault.undulate", false),
                ],
            },
            GeneratorKind::Gridshell => ParamSchema {
                kind: self,
                // The gridshell wraps a hypar surface (its most common form). The
                // vault variant is reachable from the verb; the parametric editor
                // exposes the hypar surface params, matching the default.
                fields: vec![
                    ParamField::float("a", "param.hypar.a", 5.0, Some(0.1), Some(50.0), 0.1, Widget::Slider, Unit::Meter),
                    ParamField::float("b", "param.hypar.b", 5.0, Some(0.1), Some(50.0), 0.1, Widget::Slider, Unit::Meter),
                    ParamField::float("c", "param.hypar.c", 5.0, Some(-50.0), Some(50.0), 0.1, Widget::Slider, Unit::Meter),
                    ParamField::int("nu", "param.hypar.nu", 8, 2, 48, 1, Widget::Slider),
                    ParamField::int("nv", "param.hypar.nv", 8, 2, 48, 1, Widget::Slider),
                ],
            },
            GeneratorKind::Funicular => ParamSchema {
                kind: self,
                fields: vec![
                    ParamField::vec3("support_a", "param.funicular.support_a", DVec3::new(-5.0, 0.0, 0.0)),
                    ParamField::vec3("support_b", "param.funicular.support_b", DVec3::new(5.0, 0.0, 0.0)),
                    ParamField::int("segments", "param.funicular.segments", 24, 2, 128, 1, Widget::Slider),
                    ParamField::float("load", "param.funicular.load", 1.0, Some(0.0), Some(20.0), 0.1, Widget::Slider, Unit::None),
                    ParamField::float("slack", "param.funicular.slack", 1.4, Some(1.0), Some(4.0), 0.05, Widget::Slider, Unit::None),
                    ParamField::bool("invert", "param.funicular.invert", false),
                ],
            },
            GeneratorKind::Tensegrity => ParamSchema {
                kind: self,
                fields: vec![
                    ParamField::int("struts", "param.tensegrity.struts", 3, 3, 32, 1, Widget::Slider),
                    ParamField::float("radius", "param.tensegrity.radius", 1.0, Some(0.1), Some(20.0), 0.1, Widget::Slider, Unit::Meter),
                    ParamField::float("height", "param.tensegrity.height", 2.0, Some(0.1), Some(20.0), 0.1, Widget::Slider, Unit::Meter),
                    ParamField::float("twist_deg", "param.tensegrity.twist", 60.0, Some(-180.0), Some(180.0), 1.0, Widget::Slider, Unit::Degree),
                ],
            },
            GeneratorKind::Cablenet => ParamSchema {
                kind: self,
                fields: vec![
                    ParamField::vec3("c0", "param.cablenet.c0", DVec3::new(0.0, 0.0, 0.0)),
                    ParamField::vec3("c1", "param.cablenet.c1", DVec3::new(8.0, 0.0, 0.0)),
                    ParamField::vec3("c2", "param.cablenet.c2", DVec3::new(8.0, 8.0, 3.0)),
                    ParamField::vec3("c3", "param.cablenet.c3", DVec3::new(0.0, 8.0, 3.0)),
                    ParamField::int("n", "param.cablenet.n", 8, 2, 48, 1, Widget::Slider),
                    ParamField::float("sag", "param.cablenet.sag", 1.5, Some(0.0), Some(20.0), 0.1, Widget::Slider, Unit::Meter),
                ],
            },
        }
    }

    /// Default parameter map (every schema field at its default value).
    pub fn default_params(self) -> ParamMap {
        self.schema().defaults()
    }
}

/// One typed parameter value. Serde tag keeps saved files self-describing and
/// stable across schema evolution.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", content = "v", rename_all = "snake_case")]
pub enum ParamValue {
    Float(f64),
    Int(i64),
    Bool(bool),
    Vec3([f64; 3]),
    /// Enum choice, stored by its stable variant token.
    Enum(String),
}

impl ParamValue {
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            ParamValue::Float(v) => Some(*v),
            ParamValue::Int(v) => Some(*v as f64),
            _ => None,
        }
    }
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            ParamValue::Int(v) => Some(*v),
            ParamValue::Float(v) => Some(*v as i64),
            _ => None,
        }
    }
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            ParamValue::Bool(v) => Some(*v),
            _ => None,
        }
    }
    pub fn as_vec3(&self) -> Option<DVec3> {
        match self {
            ParamValue::Vec3(v) => Some(DVec3::from_array(*v)),
            _ => None,
        }
    }
    pub fn as_enum(&self) -> Option<&str> {
        match self {
            ParamValue::Enum(s) => Some(s.as_str()),
            _ => None,
        }
    }
}

/// Ordered name -> value map for a parametric object's parameters. `BTreeMap`
/// keeps serialization deterministic (replay-stable byte-identical output).
pub type ParamMap = BTreeMap<String, ParamValue>;

/// Which control the editor UI renders for a field.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Widget {
    /// Bounded slider (min..max, step) — the tactile default.
    Slider,
    /// Drag-value / typed numeric input for precise or unbounded values.
    Numeric,
    /// Enum dropdown.
    Dropdown,
    /// Boolean toggle.
    Toggle,
    /// Three numeric fields (x, y, z).
    Point,
}

/// Display unit hint for a numeric field (drives the editor suffix; geometry is
/// always meters internally).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Unit {
    None,
    Meter,
    Degree,
}

impl Unit {
    pub fn suffix(self) -> &'static str {
        match self {
            Unit::None => "",
            Unit::Meter => " m",
            Unit::Degree => "°",
        }
    }
}

/// The value shape a field carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldKind {
    Float,
    Int,
    Bool,
    Vec3,
    Enum,
}

/// One declared parameter of a generator: name, type, bounds, default, widget,
/// unit and i18n label key. The editor UI is generated from this — a new
/// generator gets its UI for free.
#[derive(Clone, Debug, PartialEq)]
pub struct ParamField {
    pub name: &'static str,
    pub label_key: &'static str,
    pub kind: FieldKind,
    pub widget: Widget,
    pub unit: Unit,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub step: f64,
    pub default: ParamValue,
    /// Enum choices (stable tokens), empty for non-enum fields.
    pub choices: Vec<&'static str>,
}

impl ParamField {
    // Compact builder for a float field; the arg list mirrors the schema
    // columns (name, label, default, min, max, step, widget, unit) — grouping
    // them into a struct would add ceremony without clarity.
    #[allow(clippy::too_many_arguments)]
    fn float(
        name: &'static str,
        label_key: &'static str,
        default: f64,
        min: Option<f64>,
        max: Option<f64>,
        step: f64,
        widget: Widget,
        unit: Unit,
    ) -> Self {
        ParamField {
            name,
            label_key,
            kind: FieldKind::Float,
            widget,
            unit,
            min,
            max,
            step,
            default: ParamValue::Float(default),
            choices: vec![],
        }
    }
    fn int(
        name: &'static str,
        label_key: &'static str,
        default: i64,
        min: i64,
        max: i64,
        step: i64,
        widget: Widget,
    ) -> Self {
        ParamField {
            name,
            label_key,
            kind: FieldKind::Int,
            widget,
            unit: Unit::None,
            min: Some(min as f64),
            max: Some(max as f64),
            step: step as f64,
            default: ParamValue::Int(default),
            choices: vec![],
        }
    }
    fn bool(name: &'static str, label_key: &'static str, default: bool) -> Self {
        ParamField {
            name,
            label_key,
            kind: FieldKind::Bool,
            widget: Widget::Toggle,
            unit: Unit::None,
            min: None,
            max: None,
            step: 1.0,
            default: ParamValue::Bool(default),
            choices: vec![],
        }
    }
    fn vec3(name: &'static str, label_key: &'static str, default: DVec3) -> Self {
        ParamField {
            name,
            label_key,
            kind: FieldKind::Vec3,
            widget: Widget::Point,
            unit: Unit::Meter,
            min: None,
            max: None,
            step: 0.1,
            default: ParamValue::Vec3(default.to_array()),
            choices: vec![],
        }
    }
    fn enum_(
        name: &'static str,
        label_key: &'static str,
        default: &'static str,
        choices: &[&'static str],
    ) -> Self {
        ParamField {
            name,
            label_key,
            kind: FieldKind::Enum,
            widget: Widget::Dropdown,
            unit: Unit::None,
            min: None,
            max: None,
            step: 1.0,
            default: ParamValue::Enum(default.to_string()),
            choices: choices.to_vec(),
        }
    }
}

/// A generator's full parameter schema.
#[derive(Clone, Debug, PartialEq)]
pub struct ParamSchema {
    pub kind: GeneratorKind,
    pub fields: Vec<ParamField>,
}

impl ParamSchema {
    /// Default param map (every field at its default).
    pub fn defaults(&self) -> ParamMap {
        self.fields
            .iter()
            .map(|f| (f.name.to_string(), f.default.clone()))
            .collect()
    }

    /// Look up a field by name.
    pub fn field(&self, name: &str) -> Option<&ParamField> {
        self.fields.iter().find(|f| f.name == name)
    }

    /// Coerce/clamp `params` into a complete, valid map for this schema:
    /// fill missing fields from defaults, clamp numerics into [min, max],
    /// snap enums to a valid choice. Pure; the ground truth for re-derive.
    pub fn sanitize(&self, params: &ParamMap) -> ParamMap {
        let mut out = ParamMap::new();
        for f in &self.fields {
            let v = params.get(f.name).cloned().unwrap_or_else(|| f.default.clone());
            out.insert(f.name.to_string(), self.clamp_field(f, v));
        }
        out
    }

    fn clamp_field(&self, f: &ParamField, v: ParamValue) -> ParamValue {
        match f.kind {
            FieldKind::Float => {
                let mut x = v.as_f64().unwrap_or_else(|| f.default.as_f64().unwrap_or(0.0));
                if !x.is_finite() {
                    x = f.default.as_f64().unwrap_or(0.0);
                }
                if let Some(mn) = f.min {
                    x = x.max(mn);
                }
                if let Some(mx) = f.max {
                    x = x.min(mx);
                }
                ParamValue::Float(x)
            }
            FieldKind::Int => {
                let mut x = v.as_i64().unwrap_or_else(|| f.default.as_i64().unwrap_or(0));
                if let Some(mn) = f.min {
                    x = x.max(mn as i64);
                }
                if let Some(mx) = f.max {
                    x = x.min(mx as i64);
                }
                ParamValue::Int(x)
            }
            FieldKind::Bool => {
                ParamValue::Bool(v.as_bool().unwrap_or_else(|| f.default.as_bool().unwrap_or(false)))
            }
            FieldKind::Vec3 => {
                let d = v.as_vec3().unwrap_or_else(|| f.default.as_vec3().unwrap_or(DVec3::ZERO));
                let d = if d.is_finite() { d } else { f.default.as_vec3().unwrap_or(DVec3::ZERO) };
                ParamValue::Vec3(d.to_array())
            }
            FieldKind::Enum => {
                let cur = v.as_enum().unwrap_or("");
                if f.choices.contains(&cur) {
                    ParamValue::Enum(cur.to_string())
                } else {
                    f.default.clone()
                }
            }
        }
    }
}

/// Errors from deriving a mesh from generator params.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeriveError {
    /// A param value was structurally impossible (e.g. degenerate supports).
    Invalid(String),
}

impl std::fmt::Display for DeriveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeriveError::Invalid(m) => write!(f, "{m}"),
        }
    }
}

fn get_f(p: &ParamMap, k: &str) -> f64 {
    p.get(k).and_then(|v| v.as_f64()).unwrap_or(0.0)
}
fn get_i(p: &ParamMap, k: &str) -> i64 {
    p.get(k).and_then(|v| v.as_i64()).unwrap_or(0)
}
fn get_b(p: &ParamMap, k: &str) -> bool {
    p.get(k).and_then(|v| v.as_bool()).unwrap_or(false)
}
fn get_v(p: &ParamMap, k: &str) -> DVec3 {
    p.get(k).and_then(|v| v.as_vec3()).unwrap_or(DVec3::ZERO)
}

/// Derive the triangle mesh for a parametric object. **Pure** function of
/// `(generator, params)` — same input yields a byte-identical mesh, so a param
/// change replays deterministically. The caller should `schema().sanitize()`
/// the params first (the creating verbs do). This is the SINGLE re-derive path
/// shared by the creating verbs, `paramset`, and op-log replay.
pub fn derive_mesh(kind: GeneratorKind, params: &ParamMap) -> Result<Mesh, DeriveError> {
    let p = kind.schema().sanitize(params);
    match kind {
        GeneratorKind::Geodesic => {
            let frequency = get_i(&p, "frequency").max(1) as u32;
            let radius = get_f(&p, "radius");
            let dome = p.get("mode").and_then(|v| v.as_enum()) != Some("full");
            let (_, segs) = kernel_mesh::geodesic_network(frequency, radius, dome);
            let strut = (radius * 0.02).clamp(0.01, 0.5);
            Ok(kernel_mesh::strut_lattice(&segs, strut))
        }
        GeneratorKind::SpaceFrame => {
            let nx = get_i(&p, "nx").max(1) as u32;
            let ny = get_i(&p, "ny").max(1) as u32;
            let bay = get_f(&p, "bay");
            let depth = get_f(&p, "depth");
            let segs = kernel_mesh::spaceframe_struts(nx, ny, bay, depth);
            let strut = (bay * 0.04).clamp(0.02, 0.3);
            Ok(kernel_mesh::strut_lattice(&segs, strut))
        }
        GeneratorKind::Hypar => {
            let (a, b, c) = (get_f(&p, "a"), get_f(&p, "b"), get_f(&p, "c"));
            let nu = get_i(&p, "nu").max(2) as u32;
            let nv = get_i(&p, "nv").max(2) as u32;
            Ok(kernel_mesh::hypar_surface(a, b, c, nu, nv))
        }
        GeneratorKind::GaussVault => {
            let span = get_f(&p, "span");
            let length = get_f(&p, "length");
            let rise = get_f(&p, "rise");
            let undulate = get_b(&p, "undulate");
            Ok(kernel_mesh::gaussvault_surface(span, length, rise, 24, 24, undulate))
        }
        GeneratorKind::Gridshell => {
            let (a, b, c) = (get_f(&p, "a"), get_f(&p, "b"), get_f(&p, "c"));
            let nu = get_i(&p, "nu").max(2) as u32;
            let nv = get_i(&p, "nv").max(2) as u32;
            let surface = kernel_mesh::GridshellSurface::Hypar { a, b, c };
            Ok(kernel_mesh::gridshell(surface, nu, nv, 0.06))
        }
        GeneratorKind::Funicular => {
            let sa = get_v(&p, "support_a");
            let sb = get_v(&p, "support_b");
            if (sb - sa).length() < 1e-6 {
                return Err(DeriveError::Invalid("funicular supports must be distinct".into()));
            }
            let seg = get_i(&p, "segments").clamp(2, 256) as u32;
            let load = get_f(&p, "load").max(0.0);
            let slack = get_f(&p, "slack");
            let mut pts = kernel_mesh::funicular_chain(sa, sb, seg, load, slack);
            if get_b(&p, "invert") {
                pts = kernel_mesh::invert_funicular(&pts);
            }
            let segsv: Vec<(DVec3, DVec3)> = pts.windows(2).map(|w| (w[0], w[1])).collect();
            let span = (sb - sa).length();
            let strut = (span * 0.02).clamp(0.02, 0.4);
            Ok(kernel_mesh::strut_lattice(&segsv, strut))
        }
        GeneratorKind::Tensegrity => {
            let struts = get_i(&p, "struts").clamp(3, 256) as u32;
            let r = get_f(&p, "radius");
            let h = get_f(&p, "height");
            let tw = get_f(&p, "twist_deg").to_radians();
            let t = kernel_mesh::tensegrity_prism(struts, r, h, tw);
            let mut mesh =
                kernel_mesh::strut_lattice(&t.net.strut_segments(), (r * 0.08).max(0.03));
            let cables = kernel_mesh::strut_lattice(&t.net.cable_segments(), (r * 0.03).max(0.012));
            mesh.merge(&cables);
            Ok(mesh)
        }
        GeneratorKind::Cablenet => {
            let corners = [get_v(&p, "c0"), get_v(&p, "c1"), get_v(&p, "c2"), get_v(&p, "c3")];
            let n = get_i(&p, "n").clamp(2, 256) as u32;
            let sag = get_f(&p, "sag");
            let (_, _, segsv) = kernel_mesh::cable_net(corners, n, sag);
            if segsv.is_empty() {
                return Err(DeriveError::Invalid("cablenet produced no links".into()));
            }
            let span = (corners[1] - corners[0]).length().max(1e-3);
            let strut = (span * 0.01).clamp(0.01, 0.2);
            Ok(kernel_mesh::strut_lattice(&segsv, strut))
        }
    }
}

/// A short one-line summary of the key params for a card ("freq 3, r=5 m").
pub fn param_summary(kind: GeneratorKind, params: &ParamMap) -> String {
    let p = kind.schema().sanitize(params);
    let fmt = |v: &ParamValue| -> String {
        match v {
            ParamValue::Float(x) => {
                let s = format!("{x:.2}");
                s.trim_end_matches('0').trim_end_matches('.').to_string()
            }
            ParamValue::Int(x) => x.to_string(),
            ParamValue::Bool(b) => b.to_string(),
            ParamValue::Enum(e) => e.clone(),
            ParamValue::Vec3(v) => format!("({:.1},{:.1},{:.1})", v[0], v[1], v[2]),
        }
    };
    // Take the first up-to-3 non-vec3 fields for a compact summary.
    kind.schema()
        .fields
        .iter()
        .filter(|f| f.kind != FieldKind::Vec3)
        .take(3)
        .map(|f| format!("{}={}", f.name, fmt(p.get(f.name).unwrap_or(&f.default))))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_generator_has_a_nonempty_schema() {
        for &k in GeneratorKind::ALL {
            let s = k.schema();
            assert_eq!(s.kind, k);
            assert!(!s.fields.is_empty(), "{:?} has no fields", k);
        }
    }

    #[test]
    fn defaults_round_trip_and_derive() {
        for &k in GeneratorKind::ALL {
            let d = k.default_params();
            // Every field present.
            for f in &k.schema().fields {
                assert!(d.contains_key(f.name), "{:?} missing default for {}", k, f.name);
            }
            // Derive succeeds at defaults.
            let m = derive_mesh(k, &d).unwrap_or_else(|e| panic!("{:?} derive failed: {e}", k));
            assert!(!m.positions().is_empty(), "{:?} produced empty mesh", k);
        }
    }

    #[test]
    fn derive_is_deterministic_byte_identical() {
        for &k in GeneratorKind::ALL {
            let d = k.default_params();
            let a = derive_mesh(k, &d).unwrap();
            let b = derive_mesh(k, &d).unwrap();
            assert_eq!(a.positions(), b.positions(), "{:?} positions drift", k);
        }
    }

    #[test]
    fn geodesic_frequency_change_changes_mesh() {
        let mut p = GeneratorKind::Geodesic.default_params();
        let m3 = derive_mesh(GeneratorKind::Geodesic, &p).unwrap();
        p.insert("frequency".into(), ParamValue::Int(5));
        let m5 = derive_mesh(GeneratorKind::Geodesic, &p).unwrap();
        assert!(
            m5.positions().len() > m3.positions().len(),
            "freq 5 should have more geometry than freq 3"
        );
    }

    #[test]
    fn sanitize_clamps_and_fills() {
        let s = GeneratorKind::Geodesic.schema();
        let mut p = ParamMap::new();
        p.insert("frequency".into(), ParamValue::Int(999)); // over max 6
        let out = s.sanitize(&p);
        assert_eq!(out.get("frequency").unwrap().as_i64(), Some(6));
        // radius filled from default
        assert!(out.contains_key("radius"));
        assert!(out.contains_key("mode"));
    }

    #[test]
    fn sanitize_snaps_bad_enum_to_default() {
        let s = GeneratorKind::Geodesic.schema();
        let mut p = ParamMap::new();
        p.insert("mode".into(), ParamValue::Enum("bogus".into()));
        let out = s.sanitize(&p);
        assert_eq!(out.get("mode").unwrap().as_enum(), Some("dome"));
    }

    #[test]
    fn param_value_serde_round_trips() {
        for v in [
            ParamValue::Float(1.5),
            ParamValue::Int(7),
            ParamValue::Bool(true),
            ParamValue::Vec3([1.0, 2.0, 3.0]),
            ParamValue::Enum("dome".into()),
        ] {
            let j = serde_json::to_string(&v).unwrap();
            let back: ParamValue = serde_json::from_str(&j).unwrap();
            assert_eq!(v, back);
        }
    }
}
