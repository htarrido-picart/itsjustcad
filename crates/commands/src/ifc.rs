// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! IFC4 export + import — the openBIM bridge to Revit / BlenderBIM / IfcOpenShell.
//!
//! Hand-written and dependency-free, in the spirit of [`crate::dxf`] and
//! [`crate::mesh_export`]. The wire format is ISO 10303-21 (STEP Physical File,
//! "SPF"): a HEADER section, then a DATA section of `#id=ENTITY(args);` lines.
//! We target **IFC4** (`FILE_SCHEMA(('IFC4'))`) throughout — it is no harder to
//! emit than IFC2x3 for our needs and `IfcTriangulatedFaceSet` (an IFC4 entity)
//! gives us compact, unambiguous triangle geometry that every modern importer
//! reads.
//!
//! ## Export
//! Emits a minimal but valid spatial tree —
//! `IfcProject > IfcSite > IfcBuilding > IfcBuildingStorey` — with SI metre
//! units (or an `IfcConversionBasedUnit` over a metre `IfcSIUnit` for imperial
//! documents). Each mesh object becomes one `IfcBuildingElementProxy` whose
//! shape is an `IfcTriangulatedFaceSet` under an `IfcShapeRepresentation`
//! (RepresentationType `Tessellation`). The object name is carried; its layer
//! is appended to the element Name as a `[layer]` suffix (pragmatic: no
//! `IfcPresentationLayerAssignment` graph to keep the file small and valid).
//! Curves and annotations are skipped — IFC has no lightweight home for our
//! wireframe primitives and correctness beats completeness.
//!
//! ## Structural analysis model (IFC4)
//! Alongside the physical geometry, `export` also emits an
//! `IfcStructuralAnalysisModel` (dual export: physical proxies **and** the
//! analysis graph, so the same file opens in Revit/BlenderBIM *and* hands off to
//! ETABS / SAP2000 / RFEM via IfcOpenShell). Frame members
//! ([`itsjustcad_doc::Geometry::Frame`]) become `IfcStructuralCurveMember`s and
//! area members ([`itsjustcad_doc::Geometry::Area`]) become
//! `IfcStructuralSurfaceMember`s, each with an `IfcEdge`/`IfcFaceSurface`
//! topology and a placement. Sections map to the `IfcProfileDef` family
//! (`IfcRectangleProfileDef`, `IfcCircleProfileDef`, `IfcIShapeProfileDef`,
//! `IfcCircleHollowProfileDef`) and are attached to members via
//! `IfcMaterialProfileSet` / `IfcRelAssociatesMaterial`. Named materials become
//! `IfcMaterial` with an `IfcMechanicalMaterialProperties` (Young's modulus and
//! density). Loads become `IfcStructuralLoadGroup` → `IfcStructuralPointAction`
//! / `IfcStructuralLinearAction` / `IfcStructuralPlanarAction` carrying an
//! `IfcStructuralLoadSingleForce`. Supports become `IfcStructuralPointConnection`
//! with an `IfcBoundaryNodeCondition`. The model, its members, connections and
//! load groups are grouped via `IfcRelAssignsToGroup` and connected to the
//! spatial structure via `IfcRelServicesBuildings`.
//!
//! **Scope cuts (noted, not bugs):** member/surface topology uses simple
//! straight `IfcEdge`s and a single `IfcFaceSurface` plane rather than fully
//! resolved connectivity (no automatic node merging between members); load cases
//! collapse to one `IfcStructuralLoadGroup`; roller free-axis is recorded but the
//! `IfcBoundaryNodeCondition` uses translational stiffness flags only (no partial
//! stiffness values); the analysis geometry is *not* re-imported by
//! [`import`] (import stays mesh-only). Correctness over completeness.
//!
//! ## Import
//! A tolerant SPF reader: it splits the DATA section into `#id -> (ENTITY, raw
//! args)` records, ignoring the HEADER and any line it cannot parse. From that
//! table it reconstructs meshes from `IfcTriangulatedFaceSet` and
//! `IfcFacetedBrep`, applying the `IfcLocalPlacement`/`IfcAxis2Placement3D`
//! transform chain so placed geometry lands correctly. `IfcExtrudedAreaSolid`
//! is **scope-cut** (noted): extrusions require full profile + swept-solid
//! math that would dwarf this slice; unknown entities are skipped silently.

use glam::{DMat4, DVec3};
use kernel_mesh::Mesh;
use itsjustcad_doc::{
    AreaKind, Document, FrameKind, Geometry, LoadGeometry, RestraintKind, Section, StructLoad,
    StructSupport, Units, METERS_PER_FOOT, METERS_PER_INCH,
};

// ============================================================================
// EXPORT
// ============================================================================

/// A triangle mesh flattened from one document object, with its layer.
struct Part {
    name: String,
    layer: String,
    positions: Vec<DVec3>,
    faces: Vec<[u32; 3]>,
}

/// Collect plain mesh objects in document order. Curves and annotations are
/// dropped (IFC has no lightweight place for our wireframe primitives). Frame
/// and area members are **skipped** here: they are exported instead as *typed*
/// physical products (`IfcBeam`/`IfcColumn`/`IfcSlab`/`IfcWall`) by
/// [`write_typed_members`], so a semantic importer can reconstruct them.
fn collect(doc: &Document) -> Vec<Part> {
    let mut parts = Vec::new();
    for obj in doc.objects() {
        // Typed structural members get a dedicated typed product; do not also
        // flatten them to an anonymous proxy mesh.
        if matches!(obj.geometry, Geometry::Frame { .. } | Geometry::Area { .. }) {
            continue;
        }
        if let Some(m) = obj.geometry.mesh() {
            if m.faces().is_empty() {
                continue;
            }
            let name = obj
                .name
                .clone()
                .unwrap_or_else(|| format!("object_{}", parts.len()));
            parts.push(Part {
                name,
                layer: obj.layer.clone(),
                positions: m.positions().to_vec(),
                faces: m.faces().to_vec(),
            });
        }
    }
    parts
}

/// One frame member captured for the structural analysis model.
struct FrameMember {
    name: String,
    kind: FrameKind,
    a: DVec3,
    b: DVec3,
    section: Section,
    material: Option<String>,
}

/// One area (slab/wall) member captured for the structural analysis model.
struct AreaMember {
    name: String,
    kind: AreaKind,
    boundary: Vec<DVec3>,
    thickness: f64,
    material: Option<String>,
}

/// Collect structural frame + area members from the document, in creation
/// order, so the analysis model mirrors the physical one.
fn collect_structural(doc: &Document) -> (Vec<FrameMember>, Vec<AreaMember>) {
    let mut frames = Vec::new();
    let mut areas = Vec::new();
    for obj in doc.objects() {
        match &obj.geometry {
            Geometry::Frame { kind, a, b, section, material, .. } => {
                let name = obj
                    .name
                    .clone()
                    .unwrap_or_else(|| format!("{}_{}", kind.label(), frames.len()));
                frames.push(FrameMember {
                    name,
                    kind: *kind,
                    a: *a,
                    b: *b,
                    section: *section,
                    material: material.clone(),
                });
            }
            Geometry::Area { kind, boundary, thickness, material, .. } => {
                let name = obj
                    .name
                    .clone()
                    .unwrap_or_else(|| format!("{}_{}", kind.label(), areas.len()));
                areas.push(AreaMember {
                    name,
                    kind: *kind,
                    boundary: boundary.clone(),
                    thickness: *thickness,
                    material: material.clone(),
                });
            }
            _ => {}
        }
    }
    (frames, areas)
}

/// A monotonically increasing STEP id allocator. Ids are written back into the
/// entity references, so allocation order defines the file's `#n` numbering.
struct Ids(u64);
impl Ids {
    fn next(&mut self) -> u64 {
        self.0 += 1;
        self.0
    }
}

/// Export the document as an IFC4 SPF file. Returns the bytes and a short human
/// count string for the command echo (`"3 elements, 44 triangles"`).
pub fn export(doc: &Document, path: &str) -> Result<(Vec<u8>, String), String> {
    let parts = collect(doc);
    let total_tris: usize = parts.iter().map(|p| p.faces.len()).sum();

    let mut ids = Ids(0);
    let mut body = String::new();

    // --- owner history (minimal) ---
    let person = ids.next();
    let org = ids.next();
    let person_and_org = ids.next();
    let app_dev = ids.next();
    let application = ids.next();
    let owner_history = ids.next();
    line(&mut body, person, "IFCPERSON($,$,'',$,$,$,$,$)");
    line(&mut body, org, "IFCORGANIZATION($,'ItsJustCAD',$,$,$)");
    line(
        &mut body,
        person_and_org,
        &format!("IFCPERSONANDORGANIZATION(#{person},#{org},$)"),
    );
    line(&mut body, app_dev, "IFCORGANIZATION($,'ItsJustCAD',$,$,$)");
    line(
        &mut body,
        application,
        &format!("IFCAPPLICATION(#{app_dev},'0.1','ItsJustCAD','ItsJustCAD')"),
    );
    line(
        &mut body,
        owner_history,
        &format!(
            "IFCOWNERHISTORY(#{person_and_org},#{application},$,.ADDED.,$,$,$,0)"
        ),
    );

    // --- units ---
    let (unit_assignment, unit_note) = write_units(&mut body, &mut ids, doc.units);

    // --- geometric representation context (3D, model) ---
    let world_origin = ids.next();
    line(&mut body, world_origin, "IFCCARTESIANPOINT((0.,0.,0.))");
    let context = ids.next();
    line(
        &mut body,
        context,
        &format!(
            "IFCGEOMETRICREPRESENTATIONCONTEXT($,'Model',3,1.E-05,#{world_origin},$)"
        ),
    );

    // --- spatial tree: project > site > building > storey ---
    let project = ids.next();
    line(
        &mut body,
        project,
        &format!(
            "IFCPROJECT('{}',#{owner_history},'ItsJustCAD project',$,$,$,$,(#{context}),#{unit_assignment})",
            guid(project)
        ),
    );
    let (site_place, site) = write_spatial(
        &mut body,
        &mut ids,
        owner_history,
        world_origin,
        "IFCSITE",
        "Site",
        None,
        Some(".ELEMENT."),
    );
    let (bldg_place, building) = write_spatial(
        &mut body,
        &mut ids,
        owner_history,
        world_origin,
        "IFCBUILDING",
        "Building",
        Some(site_place),
        Some(".ELEMENT."),
    );
    let (storey_place, storey) = write_spatial(
        &mut body,
        &mut ids,
        owner_history,
        world_origin,
        "IFCBUILDINGSTOREY",
        "Ground Floor",
        Some(bldg_place),
        Some(".ELEMENT."),
    );
    let _ = storey_place;

    // --- aggregation relationships (project contains site, etc.) ---
    let rel = ids.next();
    line(
        &mut body,
        rel,
        &format!(
            "IFCRELAGGREGATES('{}',#{owner_history},$,$,#{project},(#{site}))",
            guid(rel)
        ),
    );
    let rel = ids.next();
    line(
        &mut body,
        rel,
        &format!(
            "IFCRELAGGREGATES('{}',#{owner_history},$,$,#{site},(#{building}))",
            guid(rel)
        ),
    );
    let rel = ids.next();
    line(
        &mut body,
        rel,
        &format!(
            "IFCRELAGGREGATES('{}',#{owner_history},$,$,#{building},(#{storey}))",
            guid(rel)
        ),
    );

    // --- one IfcBuildingElementProxy per mesh, all contained in the storey ---
    let mut element_refs = Vec::new();
    for part in &parts {
        let element = write_element(
            &mut body,
            &mut ids,
            owner_history,
            context,
            world_origin,
            storey_place,
            part,
        );
        element_refs.push(element);
    }
    if !element_refs.is_empty() {
        let rel = ids.next();
        let refs = element_refs
            .iter()
            .map(|e| format!("#{e}"))
            .collect::<Vec<_>>()
            .join(",");
        line(
            &mut body,
            rel,
            &format!(
                "IFCRELCONTAINEDINSPATIALSTRUCTURE('{}',#{owner_history},$,$,({refs}),#{storey})",
                guid(rel)
            ),
        );
    }

    // --- typed physical structural members (IfcBeam/Column/Slab/Wall) ---
    // These are the semantic products a BIM importer reconstructs into typed
    // members. Each is placed on its story (by elevation), carries an
    // IfcExtrudedAreaSolid body (so it renders in viewers) and a profile +
    // material so the shape/material round-trip.
    write_typed_members(
        &mut body,
        &mut ids,
        doc,
        owner_history,
        context,
        world_origin,
        building,
        bldg_place,
        storey,
        storey_place,
    );

    // --- structural analysis model (IfcStructuralAnalysisModel) ---
    let struct_counts = write_structural_model(
        &mut body,
        &mut ids,
        doc,
        owner_history,
        context,
        world_origin,
        building,
    );

    let file = assemble(path, &body, unit_note);
    let mut detail = format!(
        "{} element{}, {total_tris} triangles",
        parts.len(),
        if parts.len() == 1 { "" } else { "s" }
    );
    if struct_counts.any() {
        detail.push_str(&format!(
            "; analysis: {} member{}, {} surface{}, {} load{}, {} support{}",
            struct_counts.members,
            plural(struct_counts.members),
            struct_counts.surfaces,
            plural(struct_counts.surfaces),
            struct_counts.loads,
            plural(struct_counts.loads),
            struct_counts.supports,
            plural(struct_counts.supports),
        ));
    }
    Ok((file.into_bytes(), detail))
}

/// Tally of what the analysis model emitted, for the command echo.
#[derive(Default)]
struct StructCounts {
    members: usize,
    surfaces: usize,
    loads: usize,
    supports: usize,
}
impl StructCounts {
    fn any(&self) -> bool {
        self.members + self.surfaces + self.loads + self.supports > 0
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// Emit the *typed physical products* — `IfcBeam`, `IfcColumn`, `IfcSlab`,
/// `IfcWall` — one per structural frame/area member, each with a swept-solid
/// body (`IfcExtrudedAreaSolid`), a profile, an associated material, and a
/// placement on the member's story. These are what a semantic importer maps
/// back to typed members (as opposed to anonymous proxy meshes).
///
/// Members are contained in the story whose elevation is closest to the
/// member's base Z. Defined `doc.stories` are exported as extra
/// `IfcBuildingStorey`s (aggregated under the building); a member with no
/// matching story falls back to the default "Ground Floor" story.
#[allow(clippy::too_many_arguments)]
fn write_typed_members(
    body: &mut String,
    ids: &mut Ids,
    doc: &Document,
    owner_history: u64,
    context: u64,
    world_origin: u64,
    building: u64,
    bldg_place: u64,
    default_storey: u64,
    default_storey_place: u64,
) {
    let (frames, areas) = collect_structural(doc);
    if frames.is_empty() && areas.is_empty() {
        return;
    }

    // Shared materials: one IfcMaterial per named material, emitted once.
    let mut material_ids: std::collections::HashMap<String, u64> =
        std::collections::HashMap::new();
    for name in doc.materials.keys() {
        let m = ids.next();
        line(body, m, &format!("IFCMATERIAL('{}',$,$)", step_string(name)));
        material_ids.insert(name.clone(), m);
    }

    // Additional stories (beyond the default "Ground Floor"). Each is placed
    // relative to the building at its elevation and aggregated under it.
    // We keep a list of (elevation, storey_id, storey_place) for member
    // assignment. When the document defines its own stories, members assign to
    // *those* (the default "Ground Floor" is used only as a fallback when no
    // story is defined).
    let mut story_slots: Vec<(f64, u64, u64)> = Vec::new();
    if doc.stories.is_empty() {
        story_slots.push((0.0, default_storey, default_storey_place));
    }
    let mut extra_story_refs: Vec<u64> = Vec::new();
    for st in &doc.stories {
        let axis_pt = ids.next();
        line(
            body,
            axis_pt,
            &format!("IFCCARTESIANPOINT((0.,0.,{}))", num(st.elevation)),
        );
        let axis = ids.next();
        line(body, axis, &format!("IFCAXIS2PLACEMENT3D(#{axis_pt},$,$)"));
        let place = ids.next();
        line(body, place, &format!("IFCLOCALPLACEMENT(#{bldg_place},#{axis})"));
        let storey = ids.next();
        line(
            body,
            storey,
            &format!(
                "IFCBUILDINGSTOREY('{}',#{owner_history},'{}',$,$,#{place},$,$,.ELEMENT.,{})",
                guid(storey),
                step_string(&st.name),
                num(st.elevation)
            ),
        );
        story_slots.push((st.elevation, storey, place));
        extra_story_refs.push(storey);
    }
    if !extra_story_refs.is_empty() {
        let rel = ids.next();
        let refs = extra_story_refs
            .iter()
            .map(|s| format!("#{s}"))
            .collect::<Vec<_>>()
            .join(",");
        line(
            body,
            rel,
            &format!(
                "IFCRELAGGREGATES('{}',#{owner_history},$,$,#{building},({refs}))",
                guid(rel)
            ),
        );
    }

    // Pick the story whose elevation is nearest a given Z.
    let story_for = |z: f64| -> (u64, u64) {
        let mut best = (story_slots[0].1, story_slots[0].2);
        let mut best_d = f64::INFINITY;
        for (elev, sid, splace) in &story_slots {
            let d = (z - elev).abs();
            if d < best_d {
                best_d = d;
                best = (*sid, *splace);
            }
        }
        best
    };

    // Group members by story so containment relationships are per-story.
    let mut by_story: std::collections::BTreeMap<u64, Vec<u64>> =
        std::collections::BTreeMap::new();

    for fm in &frames {
        let base_z = fm.a.z.min(fm.b.z);
        let (storey, storey_place) = story_for(base_z);
        let element = write_typed_frame(
            body, ids, owner_history, context, world_origin, storey_place, fm,
            &material_ids,
        );
        by_story.entry(storey).or_default().push(element);
    }
    for am in &areas {
        let base_z = am.boundary.iter().map(|p| p.z).fold(f64::INFINITY, f64::min);
        let (storey, storey_place) = story_for(base_z);
        let element = write_typed_area(
            body, ids, owner_history, context, world_origin, storey_place, am,
            &material_ids,
        );
        by_story.entry(storey).or_default().push(element);
    }

    for (storey, elements) in by_story {
        if elements.is_empty() {
            continue;
        }
        let rel = ids.next();
        let refs = elements
            .iter()
            .map(|e| format!("#{e}"))
            .collect::<Vec<_>>()
            .join(",");
        line(
            body,
            rel,
            &format!(
                "IFCRELCONTAINEDINSPATIALSTRUCTURE('{}',#{owner_history},$,$,({refs}),#{storey})",
                guid(rel)
            ),
        );
    }
}

/// Emit one typed frame member (`IfcBeam` or `IfcColumn`). The body is an
/// `IfcExtrudedAreaSolid`: the section profile swept from endpoint `a` along the
/// member axis for its length. Placement carries `a` and the axis so the
/// importer recovers both endpoints. Profile + material round-trip via
/// `IfcRelAssociatesMaterial(IfcMaterialProfileSet)`.
#[allow(clippy::too_many_arguments)]
fn write_typed_frame(
    body: &mut String,
    ids: &mut Ids,
    owner_history: u64,
    context: u64,
    world_origin: u64,
    storey_place: u64,
    fm: &FrameMember,
    material_ids: &std::collections::HashMap<String, u64>,
) -> u64 {
    let dir = fm.b - fm.a;
    let length = dir.length().max(1e-9);
    let z = (dir / length).normalize_or_zero();
    let z = if z.length_squared() < 1e-12 { DVec3::Z } else { z };
    // Choose an X reference orthogonal to the axis (deterministic).
    let seed = if z.x.abs() < 0.9 { DVec3::X } else { DVec3::Y };
    let x = (seed - z * seed.dot(z)).normalize_or_zero();
    let x = if x.length_squared() < 1e-12 { DVec3::X } else { x };

    // Object placement: origin at `a`, local Z along the axis, X the reference.
    let loc = ids.next();
    line(
        body,
        loc,
        &format!(
            "IFCCARTESIANPOINT(({},{},{}))",
            num(fm.a.x),
            num(fm.a.y),
            num(fm.a.z)
        ),
    );
    let zdir = ids.next();
    line(body, zdir, &format!("IFCDIRECTION(({},{},{}))", num(z.x), num(z.y), num(z.z)));
    let xdir = ids.next();
    line(body, xdir, &format!("IFCDIRECTION(({},{},{}))", num(x.x), num(x.y), num(x.z)));
    let axis = ids.next();
    line(body, axis, &format!("IFCAXIS2PLACEMENT3D(#{loc},#{zdir},#{xdir})"));
    // Placement carries absolute world coords; it is *not* chained under the
    // storey placement (containment is a semantic relationship, not a
    // transform), so member geometry lands where the endpoints say.
    let _ = storey_place;
    let placement = ids.next();
    line(body, placement, &format!("IFCLOCALPLACEMENT($,#{axis})"));

    // Swept-solid body: profile at the local origin, extruded +Z for `length`.
    let profile = write_profile(body, ids, &fm.section, &fm.name);
    let solid_pos = ids.next();
    line(body, solid_pos, &format!("IFCAXIS2PLACEMENT3D(#{world_origin},$,$)"));
    let extrude_dir = ids.next();
    line(body, extrude_dir, "IFCDIRECTION((0.,0.,1.))");
    let solid = ids.next();
    line(
        body,
        solid,
        &format!(
            "IFCEXTRUDEDAREASOLID(#{profile},#{solid_pos},#{extrude_dir},{})",
            num(length)
        ),
    );
    let shape = ids.next();
    line(
        body,
        shape,
        &format!("IFCSHAPEREPRESENTATION(#{context},'Body','SweptSolid',(#{solid}))"),
    );
    let prod_shape = ids.next();
    line(body, prod_shape, &format!("IFCPRODUCTDEFINITIONSHAPE($,$,(#{shape}))"));

    let element = ids.next();
    let (entity, predef) = match fm.kind {
        FrameKind::Beam => ("IFCBEAM", ".BEAM."),
        FrameKind::Column => ("IFCCOLUMN", ".COLUMN."),
    };
    line(
        body,
        element,
        &format!(
            "{entity}('{}',#{owner_history},'{}',$,$,#{placement},#{prod_shape},$,{predef})",
            guid(element),
            step_string(&fm.name)
        ),
    );

    // Profile + material association (same shape as the analysis members).
    let mat_id = fm.material.as_ref().and_then(|m| material_ids.get(m)).copied();
    let mat_ref = mat_id.map(|m| format!("#{m}")).unwrap_or_else(|| "$".to_string());
    let mat_profile = ids.next();
    line(
        body,
        mat_profile,
        &format!(
            "IFCMATERIALPROFILE('{}',$,{mat_ref},#{profile},$,$)",
            step_string(&fm.name)
        ),
    );
    let mat_profile_set = ids.next();
    line(
        body,
        mat_profile_set,
        &format!(
            "IFCMATERIALPROFILESET('{}',$,(#{mat_profile}),$)",
            step_string(&fm.name)
        ),
    );
    let rel = ids.next();
    line(
        body,
        rel,
        &format!(
            "IFCRELASSOCIATESMATERIAL('{}',#{owner_history},$,$,(#{element}),#{mat_profile_set})",
            guid(rel)
        ),
    );

    element
}

/// Emit one typed area member (`IfcSlab` or `IfcWall`). The body is an
/// `IfcExtrudedAreaSolid` over an `IfcArbitraryClosedProfileDef` (the boundary
/// polyline), extruded by the member thickness. Placement is identity at the
/// world origin (the boundary already carries world coords). Material
/// round-trips via a direct `IfcRelAssociatesMaterial`.
#[allow(clippy::too_many_arguments)]
fn write_typed_area(
    body: &mut String,
    ids: &mut Ids,
    owner_history: u64,
    context: u64,
    world_origin: u64,
    storey_place: u64,
    am: &AreaMember,
    material_ids: &std::collections::HashMap<String, u64>,
) -> u64 {
    // Boundary as an IfcPolyline of cartesian points (closed).
    let mut pt_refs = Vec::new();
    for p in &am.boundary {
        let pt = ids.next();
        line(
            body,
            pt,
            &format!("IFCCARTESIANPOINT(({},{},{}))", num(p.x), num(p.y), num(p.z)),
        );
        pt_refs.push(pt);
    }
    // Close the polyline by repeating the first point.
    if let Some(first) = pt_refs.first().copied() {
        pt_refs.push(first);
    }
    let polyline = ids.next();
    let pts = pt_refs.iter().map(|p| format!("#{p}")).collect::<Vec<_>>().join(",");
    line(body, polyline, &format!("IFCPOLYLINE(({pts}))"));
    let profile = ids.next();
    line(
        body,
        profile,
        &format!("IFCARBITRARYCLOSEDPROFILEDEF(.AREA.,'{}',#{polyline})", step_string(&am.name)),
    );

    let solid_pos = ids.next();
    line(body, solid_pos, &format!("IFCAXIS2PLACEMENT3D(#{world_origin},$,$)"));
    let extrude_dir = ids.next();
    line(body, extrude_dir, "IFCDIRECTION((0.,0.,1.))");
    let solid = ids.next();
    line(
        body,
        solid,
        &format!(
            "IFCEXTRUDEDAREASOLID(#{profile},#{solid_pos},#{extrude_dir},{})",
            num(am.thickness)
        ),
    );
    let shape = ids.next();
    line(
        body,
        shape,
        &format!("IFCSHAPEREPRESENTATION(#{context},'Body','SweptSolid',(#{solid}))"),
    );
    let prod_shape = ids.next();
    line(body, prod_shape, &format!("IFCPRODUCTDEFINITIONSHAPE($,$,(#{shape}))"));

    let axis = ids.next();
    line(body, axis, &format!("IFCAXIS2PLACEMENT3D(#{world_origin},$,$)"));
    // Identity world placement (boundary already carries world coords); not
    // chained under the storey (see write_typed_frame).
    let _ = storey_place;
    let placement = ids.next();
    line(body, placement, &format!("IFCLOCALPLACEMENT($,#{axis})"));

    let element = ids.next();
    let (entity, predef) = match am.kind {
        AreaKind::Slab => ("IFCSLAB", ".FLOOR."),
        AreaKind::Wall => ("IFCWALL", ".STANDARD."),
    };
    line(
        body,
        element,
        &format!(
            "{entity}('{}',#{owner_history},'{}',$,$,#{placement},#{prod_shape},$,{predef})",
            guid(element),
            step_string(&am.name)
        ),
    );

    if let Some(mat_id) = am.material.as_ref().and_then(|m| material_ids.get(m)).copied() {
        let rel = ids.next();
        line(
            body,
            rel,
            &format!(
                "IFCRELASSOCIATESMATERIAL('{}',#{owner_history},$,$,(#{element}),#{mat_id})",
                guid(rel)
            ),
        );
    }

    element
}

/// Emit the full `IfcStructuralAnalysisModel` graph — members, surfaces,
/// profiles, materials, loads and supports — grouped and wired to the building.
/// Returns nothing beyond the counts; ids are appended into `body`.
#[allow(clippy::too_many_arguments)]
fn write_structural_model(
    body: &mut String,
    ids: &mut Ids,
    doc: &Document,
    owner_history: u64,
    context: u64,
    world_origin: u64,
    building: u64,
) -> StructCounts {
    let (frames, areas) = collect_structural(doc);
    let mut counts = StructCounts::default();

    // Nothing structural at all → emit no analysis model (keep files lean and
    // avoid an empty group that some importers flag).
    if frames.is_empty()
        && areas.is_empty()
        && doc.loads.is_empty()
        && doc.supports.is_empty()
    {
        return counts;
    }

    // --- materials: IfcMaterial + IfcMechanicalMaterialProperties (E, rho) ---
    // Map material name -> IfcMaterial id, emitted once and shared.
    let mut material_ids: std::collections::HashMap<String, u64> =
        std::collections::HashMap::new();
    for (name, mat) in &doc.materials {
        let m = ids.next();
        line(body, m, &format!("IFCMATERIAL('{}',$,$)", step_string(name)));
        // IfcMechanicalMaterialProperties(Material, DynamicViscosity,
        // YoungModulus, ShearModulus, PoissonRatio, ThermalExpansionCoefficient)
        let props = ids.next();
        line(
            body,
            props,
            &format!(
                "IFCMECHANICALMATERIALPROPERTIES(#{m},$,{},$,$,$)",
                num(mat.elastic_modulus_e)
            ),
        );
        // Density is a general material property (mass density measure).
        let dens_prop = ids.next();
        line(
            body,
            dens_prop,
            &format!(
                "IFCPROPERTYSINGLEVALUE('MassDensity',$,IFCMASSDENSITYMEASURE({}),$)",
                num(mat.density)
            ),
        );
        let dens_set = ids.next();
        line(
            body,
            dens_set,
            &format!(
                "IFCMATERIALPROPERTIES('Density',$,(#{dens_prop}),#{m})"
            ),
        );
        material_ids.insert(name.clone(), m);
    }

    // --- the analysis model itself ---
    let model_place_axis = ids.next();
    line(
        body,
        model_place_axis,
        &format!("IFCAXIS2PLACEMENT3D(#{world_origin},$,$)"),
    );
    let model = ids.next();
    // IfcStructuralAnalysisModel(GlobalId, Owner, Name, Desc, ObjectType,
    //   PredefinedType, OrientationOf2DPlane, LoadedBy, HasResults,
    //   SharedPlacement)
    line(
        body,
        model,
        &format!(
            "IFCSTRUCTURALANALYSISMODEL('{}',#{owner_history},'Analysis Model',$,$,.LOADING_3D.,$,$,$,#{model_place_axis})",
            guid(model)
        ),
    );
    // Service the building (spatial wiring).
    let rel = ids.next();
    line(
        body,
        rel,
        &format!(
            "IFCRELSERVICESBUILDINGS('{}',#{owner_history},$,$,#{model},(#{building}))",
            guid(rel)
        ),
    );

    // Members and connections are gathered so we can group them all under the
    // model with a single IfcRelAssignsToGroup.
    let mut grouped: Vec<u64> = Vec::new();

    // --- frame members: IfcStructuralCurveMember ---
    for fm in &frames {
        let member = write_curve_member(
            body, ids, owner_history, context, world_origin, fm, &material_ids,
        );
        grouped.push(member);
        counts.members += 1;
    }

    // --- area members: IfcStructuralSurfaceMember ---
    for am in &areas {
        let surface = write_surface_member(
            body, ids, owner_history, context, world_origin, am, &material_ids,
        );
        grouped.push(surface);
        counts.surfaces += 1;
    }

    // --- supports: IfcStructuralPointConnection + boundary conditions ---
    for sup in &doc.supports {
        let conn = write_support(body, ids, owner_history, context, world_origin, sup);
        grouped.push(conn);
        counts.supports += 1;
    }

    // --- loads: one IfcStructuralLoadGroup holding the actions ---
    if !doc.loads.is_empty() {
        let load_group = ids.next();
        // IfcStructuralLoadGroup(GlobalId, Owner, Name, Desc, ObjectType,
        //   PredefinedType, ActionType, ActionSource, Coefficient, Purpose)
        line(
            body,
            load_group,
            &format!(
                "IFCSTRUCTURALLOADGROUP('{}',#{owner_history},'Loads',$,$,.LOAD_GROUP.,.NOTDEFINED.,.NOTDEFINED.,$,$)",
                guid(load_group)
            ),
        );
        let mut actions: Vec<u64> = Vec::new();
        for load in &doc.loads {
            let action =
                write_load(body, ids, owner_history, context, world_origin, load);
            actions.push(action);
            counts.loads += 1;
        }
        // Group the actions under the load group.
        if !actions.is_empty() {
            let rel = ids.next();
            let refs = actions
                .iter()
                .map(|a| format!("#{a}"))
                .collect::<Vec<_>>()
                .join(",");
            line(
                body,
                rel,
                &format!(
                    "IFCRELASSIGNSTOGROUP('{}',#{owner_history},$,$,({refs}),$,#{load_group})",
                    guid(rel)
                ),
            );
        }
        // The load group is loaded_by the model → assign it into the model too.
        grouped.push(load_group);
    }

    // --- assign every member/connection/loadgroup into the analysis model ---
    if !grouped.is_empty() {
        let rel = ids.next();
        let refs = grouped
            .iter()
            .map(|g| format!("#{g}"))
            .collect::<Vec<_>>()
            .join(",");
        line(
            body,
            rel,
            &format!(
                "IFCRELASSIGNSTOGROUP('{}',#{owner_history},$,$,({refs}),$,#{model})",
                guid(rel)
            ),
        );
    }

    counts
}

/// Emit one `IfcStructuralCurveMember` for a frame member: a topology edge
/// between its two endpoints, a profile (via material-profile association) and
/// an optional material.
#[allow(clippy::too_many_arguments)]
fn write_curve_member(
    body: &mut String,
    ids: &mut Ids,
    owner_history: u64,
    context: u64,
    world_origin: u64,
    fm: &FrameMember,
    material_ids: &std::collections::HashMap<String, u64>,
) -> u64 {
    // Endpoints as vertex points.
    let pa = ids.next();
    line(body, pa, &format!("IFCCARTESIANPOINT(({},{},{}))", num(fm.a.x), num(fm.a.y), num(fm.a.z)));
    let pb = ids.next();
    line(body, pb, &format!("IFCCARTESIANPOINT(({},{},{}))", num(fm.b.x), num(fm.b.y), num(fm.b.z)));
    let va = ids.next();
    line(body, va, &format!("IFCVERTEXPOINT(#{pa})"));
    let vb = ids.next();
    line(body, vb, &format!("IFCVERTEXPOINT(#{pb})"));
    let edge = ids.next();
    line(body, edge, &format!("IFCEDGE(#{va},#{vb})"));
    let topo = ids.next();
    line(
        body,
        topo,
        &format!("IFCTOPOLOGYREPRESENTATION(#{context},'Reference','Edge',(#{edge}))"),
    );
    let prod_shape = ids.next();
    line(body, prod_shape, &format!("IFCPRODUCTDEFINITIONSHAPE($,$,(#{topo}))"));

    // Local placement at the world origin (topology already carries world coords).
    let axis = ids.next();
    line(body, axis, &format!("IFCAXIS2PLACEMENT3D(#{world_origin},$,$)"));
    let placement = ids.next();
    line(body, placement, &format!("IFCLOCALPLACEMENT($,#{axis})"));

    let member = ids.next();
    // IfcStructuralCurveMember(GlobalId, Owner, Name, Desc, ObjectType,
    //   ObjectPlacement, Representation, PredefinedType, Axis)
    let predefined = match fm.kind {
        FrameKind::Beam | FrameKind::Column => ".RIGID_JOINED_MEMBER.",
    };
    // Axis: a reference direction for the local z of the section (roll axis).
    let up = ids.next();
    line(body, up, "IFCDIRECTION((0.,0.,1.))");
    line(
        body,
        member,
        &format!(
            "IFCSTRUCTURALCURVEMEMBER('{}',#{owner_history},'{}',$,$,#{placement},#{prod_shape},{predefined},#{up})",
            guid(member),
            step_string(&fm.name)
        ),
    );

    // Section profile + material association.
    let profile = write_profile(body, ids, &fm.section, &fm.name);
    let mat_id = fm.material.as_ref().and_then(|m| material_ids.get(m)).copied();
    // IfcMaterialProfile(Name, Desc, Material, Profile, Priority, Category)
    let mat_ref = mat_id.map(|m| format!("#{m}")).unwrap_or_else(|| "$".to_string());
    let mat_profile = ids.next();
    line(
        body,
        mat_profile,
        &format!(
            "IFCMATERIALPROFILE('{}',$,{mat_ref},#{profile},$,$)",
            step_string(&fm.name)
        ),
    );
    let mat_profile_set = ids.next();
    line(
        body,
        mat_profile_set,
        &format!(
            "IFCMATERIALPROFILESET('{}',$,(#{mat_profile}),$)",
            step_string(&fm.name)
        ),
    );
    let rel = ids.next();
    line(
        body,
        rel,
        &format!(
            "IFCRELASSOCIATESMATERIAL('{}',#{owner_history},$,$,(#{member}),#{mat_profile_set})",
            guid(rel)
        ),
    );

    member
}

/// Emit one `IfcStructuralSurfaceMember` for an area member: a planar face over
/// its boundary polygon, plus thickness and optional material.
#[allow(clippy::too_many_arguments)]
fn write_surface_member(
    body: &mut String,
    ids: &mut Ids,
    owner_history: u64,
    context: u64,
    world_origin: u64,
    am: &AreaMember,
    material_ids: &std::collections::HashMap<String, u64>,
) -> u64 {
    // Boundary as a poly loop of cartesian points.
    let mut pt_refs = Vec::new();
    for p in &am.boundary {
        let pt = ids.next();
        line(body, pt, &format!("IFCCARTESIANPOINT(({},{},{}))", num(p.x), num(p.y), num(p.z)));
        pt_refs.push(pt);
    }
    let poly = ids.next();
    let pts = pt_refs.iter().map(|p| format!("#{p}")).collect::<Vec<_>>().join(",");
    line(body, poly, &format!("IFCPOLYLOOP(({pts}))"));
    let bound = ids.next();
    line(body, bound, &format!("IFCFACEOUTERBOUND(#{poly},.T.)"));
    // Plane surface at the boundary's first point.
    let origin = am.boundary.first().copied().unwrap_or(DVec3::ZERO);
    let plane_pt = ids.next();
    line(body, plane_pt, &format!("IFCCARTESIANPOINT(({},{},{}))", num(origin.x), num(origin.y), num(origin.z)));
    let plane_axis = ids.next();
    line(body, plane_axis, &format!("IFCAXIS2PLACEMENT3D(#{plane_pt},$,$)"));
    let plane = ids.next();
    line(body, plane, &format!("IFCPLANE(#{plane_axis})"));
    let face = ids.next();
    line(body, face, &format!("IFCFACESURFACE((#{bound}),#{plane},.T.)"));
    let topo = ids.next();
    line(
        body,
        topo,
        &format!("IFCTOPOLOGYREPRESENTATION(#{context},'Reference','Face',(#{face}))"),
    );
    let prod_shape = ids.next();
    line(body, prod_shape, &format!("IFCPRODUCTDEFINITIONSHAPE($,$,(#{topo}))"));

    let axis = ids.next();
    line(body, axis, &format!("IFCAXIS2PLACEMENT3D(#{world_origin},$,$)"));
    let placement = ids.next();
    line(body, placement, &format!("IFCLOCALPLACEMENT($,#{axis})"));

    let member = ids.next();
    // IfcStructuralSurfaceMember(GlobalId, Owner, Name, Desc, ObjectType,
    //   ObjectPlacement, Representation, PredefinedType, Thickness)
    line(
        body,
        member,
        &format!(
            "IFCSTRUCTURALSURFACEMEMBER('{}',#{owner_history},'{}',$,$,#{placement},#{prod_shape},.SHELL.,{})",
            guid(member),
            step_string(&am.name),
            num(am.thickness)
        ),
    );

    // Material association (plain material — surfaces use a layer set in strict
    // IFC, but a direct IfcRelAssociatesMaterial to the IfcMaterial is valid and
    // widely read; kept simple, noted).
    if let Some(mat_id) = am.material.as_ref().and_then(|m| material_ids.get(m)).copied() {
        let rel = ids.next();
        line(
            body,
            rel,
            &format!(
                "IFCRELASSOCIATESMATERIAL('{}',#{owner_history},$,$,(#{member}),#{mat_id})",
                guid(rel)
            ),
        );
    }

    member
}

/// Map a [`Section`] to the matching `IfcProfileDef` and emit it. Returns the
/// profile id. Dimensions are in metres (matching the model length unit).
fn write_profile(body: &mut String, ids: &mut Ids, section: &Section, name: &str) -> u64 {
    let profile = ids.next();
    let label = step_string(name);
    let entity = match *section {
        // IfcRectangleProfileDef(ProfileType, ProfileName, Position, XDim, YDim)
        Section::Rectangular { w, h } => format!(
            "IFCRECTANGLEPROFILEDEF(.AREA.,'{label}',$,{},{})",
            num(w),
            num(h)
        ),
        // IfcCircleProfileDef(ProfileType, ProfileName, Position, Radius)
        Section::Circular { d } => format!(
            "IFCCIRCLEPROFILEDEF(.AREA.,'{label}',$,{})",
            num(d * 0.5)
        ),
        // IfcCircleHollowProfileDef(ProfileType, Name, Position, Radius, WallThickness)
        Section::Pipe { d, t } => format!(
            "IFCCIRCLEHOLLOWPROFILEDEF(.AREA.,'{label}',$,{},{})",
            num(d * 0.5),
            num(t)
        ),
        // IfcIShapeProfileDef(ProfileType, Name, Position, OverallWidth,
        //   OverallDepth, WebThickness, FlangeThickness, FilletRadius,
        //   FlangeEdgeRadius, FlangeSlope)
        Section::IWideFlange { d, bf, tf, tw } => format!(
            "IFCISHAPEPROFILEDEF(.AREA.,'{label}',$,{},{},{},{},$,$,$)",
            num(bf),
            num(d),
            num(tw),
            num(tf)
        ),
        // Timber glulam/CLT edge → rectangle profile (material carries "timber").
        Section::Timber { w, h } => format!(
            "IFCRECTANGLEPROFILEDEF(.AREA.,'{label}',$,{},{})",
            num(w),
            num(h)
        ),
        // Guadua/bamboo culm → hollow circle profile.
        Section::Guadua { d, t } => format!(
            "IFCCIRCLEHOLLOWPROFILEDEF(.AREA.,'{label}',$,{},{})",
            num(d * 0.5),
            num(t)
        ),
    };
    line(body, profile, &entity);
    profile
}

/// Emit an `IfcStructuralPointConnection` with an `IfcBoundaryNodeCondition`
/// expressing the restraint (pinned/fixed/roller). Returns the connection id.
fn write_support(
    body: &mut String,
    ids: &mut Ids,
    owner_history: u64,
    context: u64,
    world_origin: u64,
    sup: &StructSupport,
) -> u64 {
    let pt = ids.next();
    line(
        body,
        pt,
        &format!(
            "IFCCARTESIANPOINT(({},{},{}))",
            num(sup.position.x),
            num(sup.position.y),
            num(sup.position.z)
        ),
    );
    let vertex = ids.next();
    line(body, vertex, &format!("IFCVERTEXPOINT(#{pt})"));
    let topo = ids.next();
    line(
        body,
        topo,
        &format!("IFCTOPOLOGYREPRESENTATION(#{context},'Reference','Vertex',(#{vertex}))"),
    );
    let prod_shape = ids.next();
    line(body, prod_shape, &format!("IFCPRODUCTDEFINITIONSHAPE($,$,(#{topo}))"));

    let axis = ids.next();
    line(body, axis, &format!("IFCAXIS2PLACEMENT3D(#{world_origin},$,$)"));
    let placement = ids.next();
    line(body, placement, &format!("IFCLOCALPLACEMENT($,#{axis})"));

    // Boundary condition: translational DOF flags. Pinned = all translations
    // fixed, rotations free; Fixed = all 6 fixed; Roller = translations fixed
    // except the free axis (approximated: Z free when no axis given).
    // IfcBoundaryNodeCondition(Name, TranslationalStiffnessX/Y/Z,
    //   RotationalStiffnessX/Y/Z). We use IfcBoolLogical .T. for "fixed" and
    //   $ for "free" — a widely-read convention for rigid supports.
    let (tx, ty, tz, rx, ry, rz) = match sup.kind {
        RestraintKind::Fixed => (".T.", ".T.", ".T.", ".T.", ".T.", ".T."),
        RestraintKind::Pinned => (".T.", ".T.", ".T.", "$", "$", "$"),
        RestraintKind::Roller => {
            // Free along the roller axis's dominant component; default Z free.
            let ax = sup.roller_axis.unwrap_or(DVec3::Z);
            let (ux, uy, uz) = (ax.x.abs(), ax.y.abs(), ax.z.abs());
            if ux >= uy && ux >= uz {
                ("$", ".T.", ".T.", "$", "$", "$")
            } else if uy >= ux && uy >= uz {
                (".T.", "$", ".T.", "$", "$", "$")
            } else {
                (".T.", ".T.", "$", "$", "$", "$")
            }
        }
    };
    let bcond = ids.next();
    line(
        body,
        bcond,
        &format!(
            "IFCBOUNDARYNODECONDITION('{}',{tx},{ty},{tz},{rx},{ry},{rz})",
            sup.kind.label()
        ),
    );

    let conn = ids.next();
    // IfcStructuralPointConnection(GlobalId, Owner, Name, Desc, ObjectType,
    //   ObjectPlacement, Representation, AppliedCondition, ConditionCoordinateSystem)
    line(
        body,
        conn,
        &format!(
            "IFCSTRUCTURALPOINTCONNECTION('{}',#{owner_history},'{}',$,$,#{placement},#{prod_shape},#{bcond},$)",
            guid(conn),
            sup.kind.label()
        ),
    );
    conn
}

/// Emit one structural action (point/linear/planar) carrying an
/// `IfcStructuralLoadSingleForce`. Returns the action id.
fn write_load(
    body: &mut String,
    ids: &mut Ids,
    owner_history: u64,
    context: u64,
    world_origin: u64,
    load: &StructLoad,
) -> u64 {
    // The load value: a single force with the magnitude spread onto the world
    // direction. Point → N; Line → N/m; Area → Pa (kept as a force measure —
    // IfcStructuralLoadSingleForce carries force components regardless).
    let f = load.direction.normalize_or_zero() * load.magnitude;
    let load_def = ids.next();
    // IfcStructuralLoadSingleForce(Name, ForceX, ForceY, ForceZ,
    //   MomentX, MomentY, MomentZ)
    line(
        body,
        load_def,
        &format!(
            "IFCSTRUCTURALLOADSINGLEFORCE('{}',{},{},{},$,$,$)",
            step_string(&load.name),
            num(f.x),
            num(f.y),
            num(f.z)
        ),
    );

    // Topology + placement for where the action applies.
    let (topo_kind, topo_items) = match &load.geometry {
        LoadGeometry::Point { position } => {
            let pt = ids.next();
            line(body, pt, &format!("IFCCARTESIANPOINT(({},{},{}))", num(position.x), num(position.y), num(position.z)));
            let v = ids.next();
            line(body, v, &format!("IFCVERTEXPOINT(#{pt})"));
            ("Vertex", format!("#{v}"))
        }
        LoadGeometry::Line { a, b } => {
            let pa = ids.next();
            line(body, pa, &format!("IFCCARTESIANPOINT(({},{},{}))", num(a.x), num(a.y), num(a.z)));
            let pb = ids.next();
            line(body, pb, &format!("IFCCARTESIANPOINT(({},{},{}))", num(b.x), num(b.y), num(b.z)));
            let va = ids.next();
            line(body, va, &format!("IFCVERTEXPOINT(#{pa})"));
            let vb = ids.next();
            line(body, vb, &format!("IFCVERTEXPOINT(#{pb})"));
            let edge = ids.next();
            line(body, edge, &format!("IFCEDGE(#{va},#{vb})"));
            ("Edge", format!("#{edge}"))
        }
        LoadGeometry::Area { boundary } => {
            let mut refs = Vec::new();
            for p in boundary {
                let pt = ids.next();
                line(body, pt, &format!("IFCCARTESIANPOINT(({},{},{}))", num(p.x), num(p.y), num(p.z)));
                refs.push(pt);
            }
            let poly = ids.next();
            let pts = refs.iter().map(|p| format!("#{p}")).collect::<Vec<_>>().join(",");
            line(body, poly, &format!("IFCPOLYLOOP(({pts}))"));
            let bound = ids.next();
            line(body, bound, &format!("IFCFACEOUTERBOUND(#{poly},.T.)"));
            let face = ids.next();
            line(body, face, &format!("IFCFACE((#{bound}))"));
            ("Face", format!("#{face}"))
        }
    };
    let topo = ids.next();
    line(
        body,
        topo,
        &format!("IFCTOPOLOGYREPRESENTATION(#{context},'Reference','{topo_kind}',({topo_items}))"),
    );
    let prod_shape = ids.next();
    line(body, prod_shape, &format!("IFCPRODUCTDEFINITIONSHAPE($,$,(#{topo}))"));
    let axis = ids.next();
    line(body, axis, &format!("IFCAXIS2PLACEMENT3D(#{world_origin},$,$)"));
    let placement = ids.next();
    line(body, placement, &format!("IFCLOCALPLACEMENT($,#{axis})"));

    let action = ids.next();
    // Choose the action entity by geometry kind. Common attribute tail:
    // (GlobalId, Owner, Name, Desc, ObjectType, ObjectPlacement, Representation,
    //  AppliedLoad, GlobalOrLocal, DestabilizingLoad[, ProjectedOrTrue|extra])
    let entity = match &load.geometry {
        LoadGeometry::Point { .. } => format!(
            "IFCSTRUCTURALPOINTACTION('{}',#{owner_history},'{}',$,$,#{placement},#{prod_shape},#{load_def},.GLOBAL_COORDS.,.F.)",
            guid(action),
            step_string(&load.name)
        ),
        LoadGeometry::Line { .. } => format!(
            "IFCSTRUCTURALLINEARACTION('{}',#{owner_history},'{}',$,$,#{placement},#{prod_shape},#{load_def},.GLOBAL_COORDS.,.F.,.GLOBAL_COORDS.)",
            guid(action),
            step_string(&load.name)
        ),
        LoadGeometry::Area { .. } => format!(
            "IFCSTRUCTURALPLANARACTION('{}',#{owner_history},'{}',$,$,#{placement},#{prod_shape},#{load_def},.GLOBAL_COORDS.,.F.,.GLOBAL_COORDS.)",
            guid(action),
            step_string(&load.name)
        ),
    };
    line(body, action, &entity);
    action
}

/// Emit an SI-metre unit assignment (length/area/volume), or a
/// conversion-based imperial length unit when the document is imperial. Returns
/// the `IfcUnitAssignment` id and a note describing the choice for the header.
fn write_units(body: &mut String, ids: &mut Ids, units: Units) -> (u64, &'static str) {
    // Base SI units: metre, square metre, cubic metre.
    let metre = ids.next();
    line(body, metre, "IFCSIUNIT(*,.LENGTHUNIT.,$,.METRE.)");
    let sqm = ids.next();
    line(body, sqm, "IFCSIUNIT(*,.AREAUNIT.,$,.SQUARE_METRE.)");
    let cbm = ids.next();
    line(body, cbm, "IFCSIUNIT(*,.VOLUMEUNIT.,$,.CUBIC_METRE.)");
    // Force (newton) and pressure (pascal) so structural load values carry
    // interpretable units for analysis-tool round-trips.
    let newton = ids.next();
    line(body, newton, "IFCSIUNIT(*,.FORCEUNIT.,$,.NEWTON.)");
    let pascal = ids.next();
    line(body, pascal, "IFCSIUNIT(*,.PRESSUREUNIT.,$,.PASCAL.)");

    let (length_unit, note): (u64, &'static str) = match units {
        // Metric families all keep the SI metre as the model length unit
        // (geometry is stored in metres regardless of display unit).
        Units::M | Units::Cm | Units::Mm => (metre, "SI metre"),
        // Imperial: an IfcConversionBasedUnit expressing feet/inches over the
        // SI metre. Coordinates stay in metres; the unit records the intent.
        Units::Ft | Units::FtIn | Units::In => {
            let (factor, name) = match units {
                Units::In => (METERS_PER_INCH, "INCH"),
                _ => (METERS_PER_FOOT, "FOOT"),
            };
            let dims = ids.next();
            line(
                body,
                dims,
                "IFCDIMENSIONALEXPONENTS(1,0,0,0,0,0,0)",
            );
            let ratio = ids.next();
            line(
                body,
                ratio,
                &format!(
                    "IFCMEASUREWITHUNIT(IFCLENGTHMEASURE({}),#{metre})",
                    num(factor)
                ),
            );
            let conv = ids.next();
            line(
                body,
                conv,
                &format!(
                    "IFCCONVERSIONBASEDUNIT(#{dims},.LENGTHUNIT.,'{name}',#{ratio})"
                ),
            );
            (conv, "imperial (IfcConversionBasedUnit over SI metre)")
        }
    };

    let assign = ids.next();
    line(
        body,
        assign,
        &format!("IFCUNITASSIGNMENT((#{length_unit},#{sqm},#{cbm},#{newton},#{pascal}))"),
    );
    (assign, note)
}

/// Emit a spatial structure element (site/building/storey) with its own
/// `IfcLocalPlacement` at the world origin. Returns `(placement_id,
/// element_id)`.
#[allow(clippy::too_many_arguments)]
fn write_spatial(
    body: &mut String,
    ids: &mut Ids,
    owner_history: u64,
    world_origin: u64,
    entity: &str,
    name: &str,
    parent_placement: Option<u64>,
    composition: Option<&str>,
) -> (u64, u64) {
    let axis = ids.next();
    line(
        body,
        axis,
        &format!("IFCAXIS2PLACEMENT3D(#{world_origin},$,$)"),
    );
    let placement = ids.next();
    let rel = match parent_placement {
        Some(p) => format!("#{p}"),
        None => "$".to_string(),
    };
    line(
        body,
        placement,
        &format!("IFCLOCALPLACEMENT({rel},#{axis})"),
    );
    let element = ids.next();
    let comp = composition.unwrap_or("$");
    // IfcSite has extra trailing attributes (ref latitude/longitude/elevation/
    // land title/address); IfcBuilding/IfcBuildingStorey share a simpler tail.
    let tail = match entity {
        "IFCSITE" => format!(",{comp},$,$,$,$,$"),
        "IFCBUILDINGSTOREY" => format!(",{comp},$"),
        _ => format!(",{comp},$"),
    };
    line(
        body,
        element,
        &format!(
            "{entity}('{}',#{owner_history},'{name}',$,$,#{placement},$,${tail})",
            guid(element)
        ),
    );
    (placement, element)
}

/// Emit one `IfcBuildingElementProxy` with an `IfcTriangulatedFaceSet` body.
/// Returns the element id (for the spatial-containment relationship).
fn write_element(
    body: &mut String,
    ids: &mut Ids,
    owner_history: u64,
    context: u64,
    world_origin: u64,
    storey_placement: u64,
    part: &Part,
) -> u64 {
    // --- point list ---
    let coords = part
        .positions
        .iter()
        .map(|p| format!("({},{},{})", num(p.x), num(p.y), num(p.z)))
        .collect::<Vec<_>>()
        .join(",");
    let point_list = ids.next();
    line(
        body,
        point_list,
        &format!("IFCCARTESIANPOINTLIST3D(({coords}))"),
    );

    // --- triangulated face set (1-based vertex indices) ---
    let index_list = part
        .faces
        .iter()
        .map(|f| format!("({},{},{})", f[0] + 1, f[1] + 1, f[2] + 1))
        .collect::<Vec<_>>()
        .join(",");
    let face_set = ids.next();
    line(
        body,
        face_set,
        &format!(
            "IFCTRIANGULATEDFACESET(#{point_list},$,.T.,({index_list}),$)"
        ),
    );

    // --- shape representation ---
    let shape = ids.next();
    line(
        body,
        shape,
        &format!(
            "IFCSHAPEREPRESENTATION(#{context},'Body','Tessellation',(#{face_set}))"
        ),
    );
    let product_shape = ids.next();
    line(
        body,
        product_shape,
        &format!("IFCPRODUCTDEFINITIONSHAPE($,$,(#{shape}))"),
    );

    // --- placement (identity, relative to the storey) ---
    let axis = ids.next();
    line(
        body,
        axis,
        &format!("IFCAXIS2PLACEMENT3D(#{world_origin},$,$)"),
    );
    let placement = ids.next();
    line(
        body,
        placement,
        &format!("IFCLOCALPLACEMENT(#{storey_placement},#{axis})"),
    );

    // --- the element itself; layer carried as a [layer] Name suffix ---
    let element = ids.next();
    let label = if part.layer.is_empty() {
        part.name.clone()
    } else {
        format!("{} [{}]", part.name, part.layer)
    };
    line(
        body,
        element,
        &format!(
            "IFCBUILDINGELEMENTPROXY('{}',#{owner_history},'{}',$,$,#{placement},#{product_shape},$,$)",
            guid(element),
            step_string(&label)
        ),
    );
    element
}

// ---- SPF text helpers ----

/// Append one `#id=BODY;` line.
fn line(out: &mut String, id: u64, entity: &str) {
    out.push('#');
    out.push_str(&id.to_string());
    out.push('=');
    out.push_str(entity);
    out.push_str(";\n");
}

/// STEP REAL literal: always carries a decimal point (`1.` not `1`), which the
/// grammar requires. Non-finite values clamp to zero.
fn num(v: f64) -> String {
    if !v.is_finite() {
        return "0.".to_string();
    }
    if v == v.trunc() && v.abs() < 1e15 {
        // Whole number → "N." form.
        return format!("{}.", v as i64);
    }
    let mut s = format!("{v:.9}");
    while s.ends_with('0') {
        s.pop();
    }
    s
}

/// Escape a string for a STEP single-quoted literal: `'` doubles to `''`.
/// Non-ASCII is passed through (importers tolerate UTF-8 in practice; the strict
/// `\X2\` encoding is out of scope for this slice).
fn step_string(s: &str) -> String {
    s.replace('\'', "''")
}

/// Deterministic 22-char IFC GUID (base64 of a value derived from the entity
/// id). Real IFC GUIDs are compressed 128-bit UUIDs; a stable per-id token is
/// valid enough for importers and keeps exports reproducible.
fn guid(id: u64) -> String {
    // IFC base64 alphabet (RFC-differs: 0-9, A-Z, a-z, _, $).
    const ALPHABET: &[u8; 64] =
        b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz_$";
    // Spread the id across 128 bits deterministically.
    let mut x = id.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(0x1234_5678);
    let mut chars = [b'0'; 22];
    for c in chars.iter_mut() {
        *c = ALPHABET[(x & 0x3f) as usize];
        x = x.rotate_left(6).wrapping_add(0x5851_F42D_4C95_7F2D);
    }
    String::from_utf8(chars.to_vec()).unwrap()
}

/// Wrap the DATA body in a full ISO-10303-21 file with an IFC4 HEADER.
fn assemble(path: &str, body: &str, unit_note: &str) -> String {
    let file_name = path.rsplit('/').next().unwrap_or(path);
    format!(
        "ISO-10303-21;\n\
HEADER;\n\
FILE_DESCRIPTION(('ViewDefinition [CoordinationView]'),'2;1');\n\
FILE_NAME('{name}','',(''),(''),'ItsJustCAD','ItsJustCAD','{note}');\n\
FILE_SCHEMA(('IFC4'));\n\
ENDSEC;\n\
DATA;\n\
{body}ENDSEC;\n\
END-ISO-10303-21;\n",
        name = step_string(file_name),
        note = step_string(unit_note),
    )
}

// ============================================================================
// IMPORT
// ============================================================================

/// One parsed STEP record: the ENTITY keyword (upper-cased) and its raw,
/// unparsed argument string (the text between the outermost parentheses).
struct Record {
    entity: String,
    args: String,
}

/// Tolerant SPF reader: id → record for every `#id=ENTITY(...);` in the DATA
/// section. Lines that do not match are skipped silently, so junk, comments and
/// HEADER content are ignored.
fn parse_spf(text: &str) -> std::collections::HashMap<u64, Record> {
    let mut map = std::collections::HashMap::new();
    // Join the whole file, then split on ';' — entities may span lines.
    // Strip /* ... */ comments first (rare but legal).
    let cleaned = strip_comments(text);
    for stmt in cleaned.split(';') {
        let stmt = stmt.trim();
        if !stmt.starts_with('#') {
            continue;
        }
        let Some(eq) = stmt.find('=') else { continue };
        let id_str = stmt[1..eq].trim();
        let Ok(id) = id_str.parse::<u64>() else {
            continue;
        };
        let rhs = stmt[eq + 1..].trim();
        // ENTITY( args )  — split at the first '('.
        let Some(open) = rhs.find('(') else { continue };
        let entity = rhs[..open].trim().to_ascii_uppercase();
        if entity.is_empty() {
            continue;
        }
        // Matched closing paren for the outermost '('.
        let Some(args) = outer_parens(&rhs[open..]) else {
            continue;
        };
        map.insert(id, Record { entity, args });
    }
    map
}

/// Remove `/* ... */` block comments.
fn strip_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i += 2;
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
}

/// Given a string beginning with '(', return the contents between it and its
/// matching ')', respecting nested parens and single-quoted strings.
fn outer_parens(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    if bytes.first() != Some(&b'(') {
        return None;
    }
    let mut depth = 0i32;
    let mut in_str = false;
    for (i, &b) in bytes.iter().enumerate() {
        if in_str {
            if b == b'\'' {
                in_str = false;
            }
            continue;
        }
        match b {
            b'\'' => in_str = true,
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(s[1..i].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

/// Split an argument string on top-level commas (ignoring commas inside nested
/// parens or single-quoted strings).
fn split_args(args: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut in_str = false;
    let mut cur = String::new();
    for c in args.chars() {
        if in_str {
            cur.push(c);
            if c == '\'' {
                in_str = false;
            }
            continue;
        }
        match c {
            '\'' => {
                in_str = true;
                cur.push(c);
            }
            '(' => {
                depth += 1;
                cur.push(c);
            }
            ')' => {
                depth -= 1;
                cur.push(c);
            }
            ',' if depth == 0 => {
                out.push(cur.trim().to_string());
                cur = String::new();
            }
            _ => cur.push(c),
        }
    }
    if !cur.trim().is_empty() || !out.is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

/// Parse a `#nnn` entity reference to its id.
fn as_ref(tok: &str) -> Option<u64> {
    tok.trim().strip_prefix('#')?.trim().parse().ok()
}

/// Parse a bare STEP REAL/INT token to f64.
fn as_num(tok: &str) -> Option<f64> {
    tok.trim().parse().ok()
}

type Records = std::collections::HashMap<u64, Record>;

/// Import an IFC file's mesh geometry. Returns named parts, each a welded-free
/// triangle mesh in world coordinates (placement chain applied).
pub fn import(bytes: &[u8]) -> Result<Vec<(String, Mesh)>, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "IFC file is not valid UTF-8".to_string())?;
    let records = parse_spf(text);

    let mut out: Vec<(String, Mesh)> = Vec::new();

    // Products carry both a name and a placement + shape; walk them so we can
    // name meshes and apply placement. A product is any entity with the
    // (GlobalId, Owner, Name, Desc, ObjectType, ObjectPlacement, Representation)
    // shape — we detect the ones that matter: IfcBuildingElementProxy and other
    // proxies. But to stay tolerant we also sweep *all* face sets/breps and, if
    // not already emitted via a product, add them at identity placement.
    let mut emitted: std::collections::HashSet<u64> = std::collections::HashSet::new();

    // Iterate ids in ascending order so import output is deterministic
    // (HashMap iteration order is not).
    let mut ids: Vec<u64> = records.keys().copied().collect();
    ids.sort_unstable();

    for id in &ids {
        let rec = &records[id];
        // Products with a representation: attrs[5]=placement, attrs[6]=repr,
        // attrs[2]=name.
        if !is_product(&rec.entity) {
            continue;
        }
        let args = split_args(&rec.args);
        if args.len() < 7 {
            continue;
        }
        let name = strip_step_string(&args[2]).unwrap_or_else(|| rec.entity.clone());
        let placement = as_ref(&args[5])
            .map(|p| placement_matrix(&records, p, 0))
            .unwrap_or(DMat4::IDENTITY);
        let repr = as_ref(&args[6]);
        let Some(repr) = repr else { continue };
        let geom_ids = shape_geometry_ids(&records, repr);
        for gid in geom_ids {
            if let Some(mesh) = mesh_from_geometry(&records, gid, placement) {
                emitted.insert(gid);
                out.push((name.clone(), mesh));
            }
        }
    }

    // Orphan geometry (face sets / breps not reached through a product) — emit
    // at identity so a bare geometry file still yields meshes.
    for id in &ids {
        if emitted.contains(id) {
            continue;
        }
        let rec = &records[id];
        if (rec.entity == "IFCTRIANGULATEDFACESET" || rec.entity == "IFCFACETEDBREP")
            && let Some(mesh) = mesh_from_geometry(&records, *id, DMat4::IDENTITY)
        {
            out.push(("ifc".to_string(), mesh));
        }
    }

    Ok(out)
}

// ============================================================================
// SEMANTIC IMPORT — reconstruct typed structural members (not just meshes)
// ============================================================================

/// A recovered material: its name and mechanical properties (defaults when the
/// file only names a material without `IfcMechanicalMaterialProperties`).
#[derive(Clone, Debug, PartialEq)]
pub struct ImportedMaterial {
    pub name: String,
    pub elastic_modulus_e: f64,
    pub density: f64,
}

/// A recovered building story/level.
#[derive(Clone, Debug, PartialEq)]
pub struct ImportedStory {
    pub name: String,
    pub elevation: f64,
}

/// One element recovered from an IFC file. Typed structural products
/// (`IfcBeam`/`IfcColumn`/`IfcSlab`/`IfcWall`) reconstruct into `Frame`/`Area`;
/// everything else falls back to `Mesh` (on the `ifc` layer, current behavior).
#[derive(Clone, Debug, PartialEq)]
pub enum ImportedElement {
    Frame {
        name: String,
        kind: FrameKind,
        a: DVec3,
        b: DVec3,
        /// Synthesized section name (matches an entry in [`SemanticImport::sections`]).
        section: String,
        material: Option<String>,
        story: Option<String>,
    },
    Area {
        name: String,
        kind: AreaKind,
        boundary: Vec<DVec3>,
        thickness: f64,
        material: Option<String>,
        story: Option<String>,
    },
    Mesh {
        name: String,
        mesh: Mesh,
    },
}

/// The whole semantic import: definitions to declare first (materials, sections,
/// stories) plus the ordered elements. `sections` maps a synthesized section
/// name → `Section` so the caller can emit `DefSection` before members.
#[derive(Clone, Debug, Default)]
pub struct SemanticImport {
    pub materials: Vec<ImportedMaterial>,
    pub stories: Vec<ImportedStory>,
    /// (synthesized name, section) pairs, deduplicated by geometry.
    pub sections: Vec<(String, Section)>,
    pub elements: Vec<ImportedElement>,
}

/// Semantic IFC import: reconstruct typed structural members with their
/// sections, materials and stories, falling back to meshes for anything else.
///
/// This is the replay-safe path: the caller ([`crate::exec`]) turns each result
/// into a *logged substrate command* (`DefMaterial`/`DefSection`/`DefStory`/
/// `FrameMember`/`AreaMember`/`MeshLiteral`), so the whole import is history.
pub fn import_semantic(bytes: &[u8]) -> Result<SemanticImport, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "IFC file is not valid UTF-8".to_string())?;
    let records = parse_spf(text);
    let mut out = SemanticImport::default();

    let mut ids: Vec<u64> = records.keys().copied().collect();
    ids.sort_unstable();

    // --- materials: name → properties, recovered from IfcMaterial (+ optional
    // IfcMechanicalMaterialProperties / density property). ---
    let materials = collect_materials(&records, &ids);
    // Which material an element associates, via IfcRelAssociatesMaterial.
    let elem_material = collect_element_materials(&records, &ids);
    // Story elevations + which storey contains each element.
    let stories = collect_stories(&records, &ids);
    let elem_story = collect_element_stories(&records, &ids);

    // Track section geometry → synthesized name so identical profiles share a
    // single DefSection.
    let mut section_names: Vec<(Section, String)> = Vec::new();
    let mut section_for = |sec: Section, out: &mut SemanticImport| -> String {
        for (s, n) in &section_names {
            if sections_equal(s, &sec) {
                return n.clone();
            }
        }
        let name = format!("S{}", section_names.len() + 1);
        section_names.push((sec, name.clone()));
        out.sections.push((name.clone(), sec));
        name
    };

    // Only surface materials/stories that are actually referenced by a typed
    // member (keeps the import lean and deterministic).
    let mut used_materials: std::collections::BTreeSet<String> =
        std::collections::BTreeSet::new();
    let mut used_stories: std::collections::BTreeSet<String> =
        std::collections::BTreeSet::new();
    let mut emitted_geom: std::collections::HashSet<u64> = std::collections::HashSet::new();

    for id in &ids {
        let rec = &records[id];
        let (kind_frame, kind_area) = match rec.entity.as_str() {
            "IFCBEAM" => (Some(FrameKind::Beam), None),
            "IFCCOLUMN" => (Some(FrameKind::Column), None),
            "IFCSLAB" => (None, Some(AreaKind::Slab)),
            "IFCWALL" | "IFCWALLSTANDARDCASE" => (None, Some(AreaKind::Wall)),
            _ => (None, None),
        };
        if kind_frame.is_none() && kind_area.is_none() {
            continue;
        }
        let args = split_args(&rec.args);
        if args.len() < 7 {
            continue;
        }
        let name = strip_step_string(&args[2]).unwrap_or_else(|| rec.entity.clone());
        let placement = as_ref(&args[5])
            .map(|p| placement_matrix(&records, p, 0))
            .unwrap_or(DMat4::IDENTITY);
        let Some(repr) = as_ref(&args[6]) else { continue };
        let solid = shape_geometry_ids(&records, repr)
            .into_iter()
            .find(|g| records.get(g).map(|r| r.entity == "IFCEXTRUDEDAREASOLID").unwrap_or(false));

        let material = elem_material.get(id).and_then(|m| materials.get(m)).map(|m| m.name.clone());
        let story = elem_story.get(id).cloned();

        if let Some(kind) = kind_frame {
            // Frame: recover endpoints from placement + extruded solid depth.
            let Some(solid) = solid else {
                // No swept solid → fall back to any mesh under the repr.
                push_mesh_fallback(&records, repr, placement, &name, &mut out, &mut emitted_geom);
                continue;
            };
            let Some((section, depth)) = extruded_frame(&records, solid) else {
                push_mesh_fallback(&records, repr, placement, &name, &mut out, &mut emitted_geom);
                continue;
            };
            // The extrusion is along local +Z for `depth`; placement maps local
            // origin → a and local +Z → member axis.
            let a = placement.transform_point3(DVec3::ZERO);
            let b = placement.transform_point3(DVec3::Z * depth);
            let sec_name = section_for(section, &mut out);
            if let Some(m) = &material {
                used_materials.insert(m.clone());
            }
            if let Some(st) = &story {
                used_stories.insert(st.clone());
            }
            out.elements.push(ImportedElement::Frame {
                name,
                kind,
                a,
                b,
                section: sec_name,
                material,
                story,
            });
        } else if let Some(kind) = kind_area {
            let Some(solid) = solid else {
                push_mesh_fallback(&records, repr, placement, &name, &mut out, &mut emitted_geom);
                continue;
            };
            let Some((boundary, thickness)) = extruded_area(&records, solid, placement) else {
                push_mesh_fallback(&records, repr, placement, &name, &mut out, &mut emitted_geom);
                continue;
            };
            if let Some(m) = &material {
                used_materials.insert(m.clone());
            }
            if let Some(st) = &story {
                used_stories.insert(st.clone());
            }
            out.elements.push(ImportedElement::Area {
                name,
                kind,
                boundary,
                thickness,
                material,
                story,
            });
        }
    }

    // --- fallback meshes for every other product / orphan geometry (unchanged
    // behavior: unknown entities land on the 'ifc' layer as MeshLiteral). ---
    for id in &ids {
        let rec = &records[id];
        if matches!(
            rec.entity.as_str(),
            "IFCBEAM" | "IFCCOLUMN" | "IFCSLAB" | "IFCWALL" | "IFCWALLSTANDARDCASE"
        ) {
            continue; // handled above
        }
        if !is_product(&rec.entity) {
            continue;
        }
        let args = split_args(&rec.args);
        if args.len() < 7 {
            continue;
        }
        let name = strip_step_string(&args[2]).unwrap_or_else(|| rec.entity.clone());
        let placement = as_ref(&args[5])
            .map(|p| placement_matrix(&records, p, 0))
            .unwrap_or(DMat4::IDENTITY);
        let Some(repr) = as_ref(&args[6]) else { continue };
        for gid in shape_geometry_ids(&records, repr) {
            if emitted_geom.contains(&gid) {
                continue;
            }
            if let Some(mesh) = mesh_from_geometry(&records, gid, placement) {
                emitted_geom.insert(gid);
                out.elements.push(ImportedElement::Mesh { name: name.clone(), mesh });
            }
        }
    }
    for id in &ids {
        if emitted_geom.contains(id) {
            continue;
        }
        let rec = &records[id];
        if (rec.entity == "IFCTRIANGULATEDFACESET" || rec.entity == "IFCFACETEDBREP")
            && let Some(mesh) = mesh_from_geometry(&records, *id, DMat4::IDENTITY)
        {
            out.elements.push(ImportedElement::Mesh { name: "ifc".to_string(), mesh });
        }
    }

    // Keep only referenced materials/stories, in a stable order.
    out.materials = materials
        .into_values()
        .filter(|m| used_materials.contains(&m.name))
        .collect();
    out.materials.sort_by(|a, b| a.name.cmp(&b.name));
    out.stories = stories.into_iter().filter(|s| used_stories.contains(&s.name)).collect();

    Ok(out)
}

/// True when two sections are geometrically identical (same variant + dims,
/// within a tight tolerance so float round-trips still match).
fn sections_equal(a: &Section, b: &Section) -> bool {
    let eq = |x: f64, y: f64| (x - y).abs() < 1e-9;
    match (a, b) {
        (Section::Rectangular { w: w1, h: h1 }, Section::Rectangular { w: w2, h: h2 }) => {
            eq(*w1, *w2) && eq(*h1, *h2)
        }
        (Section::Circular { d: d1 }, Section::Circular { d: d2 }) => eq(*d1, *d2),
        (Section::Pipe { d: d1, t: t1 }, Section::Pipe { d: d2, t: t2 }) => {
            eq(*d1, *d2) && eq(*t1, *t2)
        }
        (
            Section::IWideFlange { d: d1, bf: bf1, tf: tf1, tw: tw1 },
            Section::IWideFlange { d: d2, bf: bf2, tf: tf2, tw: tw2 },
        ) => eq(*d1, *d2) && eq(*bf1, *bf2) && eq(*tf1, *tf2) && eq(*tw1, *tw2),
        (Section::Timber { w: w1, h: h1 }, Section::Timber { w: w2, h: h2 }) => {
            eq(*w1, *w2) && eq(*h1, *h2)
        }
        (Section::Guadua { d: d1, t: t1 }, Section::Guadua { d: d2, t: t2 }) => {
            eq(*d1, *d2) && eq(*t1, *t2)
        }
        _ => false,
    }
}

/// Recover named materials with their mechanical properties. Missing E/density
/// default to steel-ish values so members still import when a file names a
/// material without properties.
fn collect_materials(
    records: &Records,
    ids: &[u64],
) -> std::collections::HashMap<u64, ImportedMaterial> {
    let mut out = std::collections::HashMap::new();
    // First pass: IfcMaterial id → name.
    for id in ids {
        let rec = &records[id];
        if rec.entity != "IFCMATERIAL" {
            continue;
        }
        let args = split_args(&rec.args);
        let name = args.first().and_then(|a| strip_step_string(a)).unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        out.insert(
            *id,
            ImportedMaterial { name, elastic_modulus_e: 0.0, density: 0.0 },
        );
    }
    // Second pass: IfcMechanicalMaterialProperties(Material, ..., YoungModulus,..)
    for id in ids {
        let rec = &records[id];
        if rec.entity == "IFCMECHANICALMATERIALPROPERTIES" {
            let args = split_args(&rec.args);
            if let Some(mat_id) = args.first().and_then(|a| as_ref(a))
                && let Some(mat) = out.get_mut(&mat_id)
            {
                // args: Material, DynamicViscosity, YoungModulus, ...
                if let Some(e) = args.get(2).and_then(|a| as_num(a)) {
                    mat.elastic_modulus_e = e;
                }
            }
        }
    }
    // Third pass: density carried as IfcMaterialProperties → IfcPropertySingleValue
    // ('MassDensity', ..., IFCMASSDENSITYMEASURE(x)). Scan property sets whose
    // last arg references a material.
    for id in ids {
        let rec = &records[id];
        if rec.entity != "IFCMATERIALPROPERTIES" {
            continue;
        }
        let args = split_args(&rec.args);
        // IfcMaterialProperties(Name, Desc, Properties=[..], Material)
        let Some(mat_id) = args.last().and_then(|a| as_ref(a)) else { continue };
        let Some(mat) = out.get_mut(&mat_id) else { continue };
        // Properties list is the arg before the material.
        if args.len() >= 4 {
            for prop_id in ref_list(&args[2]) {
                if let Some(prop) = records.get(&prop_id)
                    && prop.entity == "IFCPROPERTYSINGLEVALUE"
                {
                    let pa = split_args(&prop.args);
                    if let Some(v) = pa.get(2)
                        && let Some(inner) = v.find('(').and_then(|o| outer_parens(&v[o..]))
                        && let Some(d) = as_num(&inner)
                    {
                        mat.density = d;
                    }
                }
            }
        }
    }
    // Apply defaults for any property left at zero.
    for mat in out.values_mut() {
        if mat.elastic_modulus_e <= 0.0 {
            mat.elastic_modulus_e = 2.0e11;
        }
        if mat.density <= 0.0 {
            mat.density = 7850.0;
        }
    }
    out
}

/// Map element id → the IfcMaterial id it associates (walking
/// IfcRelAssociatesMaterial, resolving IfcMaterialProfileSet →
/// IfcMaterialProfile → IfcMaterial when needed).
fn collect_element_materials(records: &Records, ids: &[u64]) -> std::collections::HashMap<u64, u64> {
    let mut out = std::collections::HashMap::new();
    for id in ids {
        let rec = &records[id];
        if rec.entity != "IFCRELASSOCIATESMATERIAL" {
            continue;
        }
        // IfcRelAssociatesMaterial(GlobalId, Owner, Name, Desc, RelatedObjects=[..],
        //   RelatingMaterial)
        let args = split_args(&rec.args);
        if args.len() < 6 {
            continue;
        }
        let objects = ref_list(&args[4]);
        let Some(mat_ref) = as_ref(&args[5]) else { continue };
        let mat_id = resolve_material(records, mat_ref);
        let Some(mat_id) = mat_id else { continue };
        for obj in objects {
            out.insert(obj, mat_id);
        }
    }
    out
}

/// Resolve a RelatingMaterial reference to an IfcMaterial id: direct, or via
/// IfcMaterialProfileSet → IfcMaterialProfile.Material.
fn resolve_material(records: &Records, id: u64) -> Option<u64> {
    let rec = records.get(&id)?;
    match rec.entity.as_str() {
        "IFCMATERIAL" => Some(id),
        "IFCMATERIALPROFILESET" => {
            let args = split_args(&rec.args);
            // (Name, Desc, MaterialProfiles=[..], CompositeProfile)
            let profiles = args.get(2).map(|a| ref_list(a)).unwrap_or_default();
            for p in profiles {
                if let Some(mp) = records.get(&p)
                    && mp.entity == "IFCMATERIALPROFILE"
                {
                    let pa = split_args(&mp.args);
                    // (Name, Desc, Material, Profile, Priority, Category)
                    if let Some(m) = pa.get(2).and_then(|a| as_ref(a)) {
                        return Some(m);
                    }
                }
            }
            None
        }
        _ => None,
    }
}

/// Recover all IfcBuildingStorey names + elevations.
fn collect_stories(records: &Records, ids: &[u64]) -> Vec<ImportedStory> {
    let mut out = Vec::new();
    for id in ids {
        let rec = &records[id];
        if rec.entity != "IFCBUILDINGSTOREY" {
            continue;
        }
        let args = split_args(&rec.args);
        let name = args.get(2).and_then(|a| strip_step_string(a)).unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        // IfcBuildingStorey(...,CompositionType, Elevation) — elevation is last.
        let elevation = args.last().and_then(|a| as_num(a)).unwrap_or(0.0);
        out.push(ImportedStory { name, elevation });
    }
    out
}

/// Map element id → the storey name that contains it (via
/// IfcRelContainedInSpatialStructure).
fn collect_element_stories(records: &Records, ids: &[u64]) -> std::collections::HashMap<u64, String> {
    // storey id → name.
    let mut storey_name: std::collections::HashMap<u64, String> = std::collections::HashMap::new();
    for id in ids {
        let rec = &records[id];
        if rec.entity == "IFCBUILDINGSTOREY" {
            let args = split_args(&rec.args);
            if let Some(n) = args.get(2).and_then(|a| strip_step_string(a)) {
                storey_name.insert(*id, n);
            }
        }
    }
    let mut out = std::collections::HashMap::new();
    for id in ids {
        let rec = &records[id];
        if rec.entity != "IFCRELCONTAINEDINSPATIALSTRUCTURE" {
            continue;
        }
        // (GlobalId, Owner, Name, Desc, RelatedElements=[..], RelatingStructure)
        let args = split_args(&rec.args);
        if args.len() < 6 {
            continue;
        }
        let elements = ref_list(&args[4]);
        let Some(structure) = as_ref(&args[5]) else { continue };
        let Some(name) = storey_name.get(&structure) else { continue };
        for e in elements {
            out.insert(e, name.clone());
        }
    }
    out
}

/// From an `IfcExtrudedAreaSolid` used as a *frame* body, recover the swept
/// `Section` and the extrusion depth (member length). The profile is one of the
/// parametric `IfcProfileDef`s written by [`write_profile`].
fn extruded_frame(records: &Records, solid_id: u64) -> Option<(Section, f64)> {
    let rec = records.get(&solid_id)?;
    if rec.entity != "IFCEXTRUDEDAREASOLID" {
        return None;
    }
    // IfcExtrudedAreaSolid(SweptArea, Position, ExtrudedDirection, Depth)
    let args = split_args(&rec.args);
    if args.len() < 4 {
        return None;
    }
    let profile_id = as_ref(&args[0])?;
    let section = profile_to_section(records, profile_id)?;
    let depth = as_num(&args[3])?;
    Some((section, depth))
}

/// Map a parametric `IfcProfileDef` back to a [`Section`].
fn profile_to_section(records: &Records, id: u64) -> Option<Section> {
    let rec = records.get(&id)?;
    let args = split_args(&rec.args);
    match rec.entity.as_str() {
        // (ProfileType, Name, Position, XDim, YDim)
        "IFCRECTANGLEPROFILEDEF" => {
            let w = as_num(args.get(3)?)?;
            let h = as_num(args.get(4)?)?;
            Some(Section::Rectangular { w, h })
        }
        // (ProfileType, Name, Position, Radius)
        "IFCCIRCLEPROFILEDEF" => {
            let r = as_num(args.get(3)?)?;
            Some(Section::Circular { d: r * 2.0 })
        }
        // (ProfileType, Name, Position, Radius, WallThickness)
        "IFCCIRCLEHOLLOWPROFILEDEF" => {
            let r = as_num(args.get(3)?)?;
            let t = as_num(args.get(4)?)?;
            Some(Section::Pipe { d: r * 2.0, t })
        }
        // (ProfileType, Name, Position, OverallWidth, OverallDepth,
        //  WebThickness, FlangeThickness, ...)
        "IFCISHAPEPROFILEDEF" => {
            let bf = as_num(args.get(3)?)?;
            let d = as_num(args.get(4)?)?;
            let tw = as_num(args.get(5)?)?;
            let tf = as_num(args.get(6)?)?;
            Some(Section::IWideFlange { d, bf, tf, tw })
        }
        _ => None,
    }
}

/// From an `IfcExtrudedAreaSolid` used as an *area* body, recover the boundary
/// polygon (world coords, via `placement`) and the thickness (extrusion depth).
fn extruded_area(records: &Records, solid_id: u64, placement: DMat4) -> Option<(Vec<DVec3>, f64)> {
    let rec = records.get(&solid_id)?;
    if rec.entity != "IFCEXTRUDEDAREASOLID" {
        return None;
    }
    let args = split_args(&rec.args);
    if args.len() < 4 {
        return None;
    }
    let profile_id = as_ref(&args[0])?;
    let depth = as_num(&args[3])?;
    // Profile is an IfcArbitraryClosedProfileDef(ProfileType, Name, OuterCurve).
    let profile = records.get(&profile_id)?;
    if profile.entity != "IFCARBITRARYCLOSEDPROFILEDEF" {
        return None;
    }
    let pargs = split_args(&profile.args);
    let curve_id = as_ref(pargs.get(2)?)?;
    let curve = records.get(&curve_id)?;
    if curve.entity != "IFCPOLYLINE" {
        return None;
    }
    // IfcPolyline(Points=[IfcCartesianPoint..]).
    let ca = split_args(&curve.args);
    let pts = ref_list(ca.first()?);
    let mut boundary: Vec<DVec3> = Vec::new();
    for p in pts {
        if let Some(pt) = cartesian_point(records, p) {
            boundary.push(placement.transform_point3(pt));
        }
    }
    // Drop the closing repeat point if present (first == last).
    if boundary.len() >= 2 {
        let first = boundary[0];
        let last = *boundary.last().unwrap();
        if (first - last).length() < 1e-9 {
            boundary.pop();
        }
    }
    if boundary.len() < 3 {
        return None;
    }
    Some((boundary, depth))
}

/// Emit meshes under a representation as `Mesh` fallback elements (used when a
/// typed member's body cannot be interpreted as a swept solid).
fn push_mesh_fallback(
    records: &Records,
    repr: u64,
    placement: DMat4,
    name: &str,
    out: &mut SemanticImport,
    emitted: &mut std::collections::HashSet<u64>,
) {
    for gid in shape_geometry_ids(records, repr) {
        if emitted.contains(&gid) {
            continue;
        }
        if let Some(mesh) = mesh_from_geometry(records, gid, placement) {
            emitted.insert(gid);
            out.elements.push(ImportedElement::Mesh { name: name.to_string(), mesh });
        }
    }
}

/// Entity keywords we treat as placeable, named products carrying a shape.
fn is_product(entity: &str) -> bool {
    matches!(
        entity,
        "IFCBUILDINGELEMENTPROXY"
            | "IFCWALL"
            | "IFCWALLSTANDARDCASE"
            | "IFCSLAB"
            | "IFCBEAM"
            | "IFCCOLUMN"
            | "IFCROOF"
            | "IFCMEMBER"
            | "IFCPLATE"
            | "IFCFURNISHINGELEMENT"
            | "IFCPROXY"
    )
}

/// Follow an `IfcProductDefinitionShape` → its `IfcShapeRepresentation`s →
/// their representation items (geometry). Returns the geometry ids.
fn shape_geometry_ids(records: &Records, product_shape: u64) -> Vec<u64> {
    let mut ids = Vec::new();
    let Some(rec) = records.get(&product_shape) else {
        return ids;
    };
    // IfcProductDefinitionShape(Name, Desc, Representations=[..])
    // or directly an IfcShapeRepresentation.
    let args = split_args(&rec.args);
    let reprs: Vec<u64> = if rec.entity == "IFCPRODUCTDEFINITIONSHAPE" {
        args.last()
            .map(|a| ref_list(a))
            .unwrap_or_default()
    } else {
        vec![product_shape]
    };
    for repr in reprs {
        let Some(r) = records.get(&repr) else { continue };
        if r.entity != "IFCSHAPEREPRESENTATION" {
            // Might already be a geometry item.
            ids.push(repr);
            continue;
        }
        // IfcShapeRepresentation(Context, Id, Type, Items=[..])
        let ra = split_args(&r.args);
        if let Some(items) = ra.last() {
            ids.extend(ref_list(items));
        }
    }
    ids
}

/// Build a mesh from a geometry entity id, transformed by `xform`.
fn mesh_from_geometry(records: &Records, gid: u64, xform: DMat4) -> Option<Mesh> {
    let rec = records.get(&gid)?;
    match rec.entity.as_str() {
        "IFCTRIANGULATEDFACESET" => triangulated_face_set(records, rec, xform),
        "IFCFACETEDBREP" => faceted_brep(records, rec, xform),
        _ => None,
    }
}

/// IfcTriangulatedFaceSet(Coordinates, Normals, Closed, CoordIndex, PnIndex).
fn triangulated_face_set(records: &Records, rec: &Record, xform: DMat4) -> Option<Mesh> {
    let args = split_args(&rec.args);
    if args.len() < 4 {
        return None;
    }
    let coords_ref = as_ref(&args[0])?;
    let positions = point_list_3d(records, coords_ref)?;
    // CoordIndex is a list of index triples: ((1,2,3),(1,3,4),...).
    let inner = outer_parens(args[3].trim())?; // strip the outer '(...)'
    let mut faces = Vec::new();
    for tri in split_args(&inner) {
        let t = tri.trim();
        let t = t.strip_prefix('(').unwrap_or(t);
        let t = t.strip_suffix(')').unwrap_or(t);
        let idx: Vec<u32> = t
            .split(',')
            .filter_map(|n| n.trim().parse::<u32>().ok())
            .collect();
        if idx.len() == 3 {
            // IFC indices are 1-based. H-4: reject any index that is 0 (would
            // underflow on subtraction) or that exceeds the positions count.
            let n = positions.len() as u32;
            if idx.iter().any(|&v| v == 0 || v > n) {
                continue;
            }
            faces.push([idx[0] - 1, idx[1] - 1, idx[2] - 1]);
        }
    }
    if faces.is_empty() {
        return None;
    }
    let positions: Vec<DVec3> = positions.iter().map(|p| xform.transform_point3(*p)).collect();
    Some(Mesh::new(positions, faces))
}

/// IfcFacetedBrep(Outer=IfcClosedShell). The shell holds IfcFace →
/// IfcFaceOuterBound → IfcPolyLoop → IfcCartesianPoint list; polygons are
/// fan-triangulated.
fn faceted_brep(records: &Records, rec: &Record, xform: DMat4) -> Option<Mesh> {
    let args = split_args(&rec.args);
    let shell_ref = as_ref(args.first()?)?;
    let shell = records.get(&shell_ref)?;
    // IfcClosedShell(CfsFaces=[..])
    let shell_args = split_args(&shell.args);
    let face_refs = ref_list(shell_args.first()?);

    let mut positions: Vec<DVec3> = Vec::new();
    let mut faces: Vec<[u32; 3]> = Vec::new();
    let mut cache: std::collections::HashMap<u64, u32> = std::collections::HashMap::new();

    for face_id in face_refs {
        let Some(face) = records.get(&face_id) else { continue };
        if face.entity != "IFCFACE" {
            continue;
        }
        // IfcFace(Bounds=[IfcFaceBound|IfcFaceOuterBound]).
        let fa = split_args(&face.args);
        let Some(bound_refs) = fa.first().map(|a| ref_list(a)) else {
            continue;
        };
        // Use the first (outer) bound only — inner bounds (holes) are dropped.
        let Some(&bound_id) = bound_refs.first() else { continue };
        let Some(bound) = records.get(&bound_id) else { continue };
        // IfcFaceBound(Bound=IfcPolyLoop, Orientation).
        let ba = split_args(&bound.args);
        let Some(loop_id) = ba.first().and_then(|a| as_ref(a)) else {
            continue;
        };
        let Some(poly) = records.get(&loop_id) else { continue };
        if poly.entity != "IFCPOLYLOOP" {
            continue;
        }
        // IfcPolyLoop(Polygon=[IfcCartesianPoint..]).
        let pa = split_args(&poly.args);
        let Some(pt_refs) = pa.first().map(|a| ref_list(a)) else { continue };

        let mut loop_indices = Vec::new();
        for pt_id in pt_refs {
            let local = *cache.entry(pt_id).or_insert_with(|| {
                let p = cartesian_point(records, pt_id).unwrap_or(DVec3::ZERO);
                let idx = positions.len() as u32;
                positions.push(xform.transform_point3(p));
                idx
            });
            loop_indices.push(local);
        }
        // Fan-triangulate the polygon loop.
        for i in 1..loop_indices.len().saturating_sub(1) {
            faces.push([loop_indices[0], loop_indices[i], loop_indices[i + 1]]);
        }
    }

    if faces.is_empty() {
        return None;
    }
    // H-5: drop any face whose indices exceed the positions array (defensive).
    let pos_len = positions.len() as u32;
    let faces: Vec<[u32; 3]> = faces
        .into_iter()
        .filter(|f| f.iter().all(|&v| v < pos_len))
        .collect();
    if faces.is_empty() {
        return None;
    }
    Some(Mesh::new(positions, faces))
}

/// Read an `IfcCartesianPointList3D` into world-space points (0-based order).
fn point_list_3d(records: &Records, id: u64) -> Option<Vec<DVec3>> {
    let rec = records.get(&id)?;
    if rec.entity != "IFCCARTESIANPOINTLIST3D" {
        return None;
    }
    // IfcCartesianPointList3D(CoordList=((x,y,z),...)).
    let args = split_args(&rec.args);
    let inner = outer_parens(args.first()?.trim())?;
    let mut pts = Vec::new();
    for tup in split_args(&inner) {
        let t = tup.trim();
        let t = t.strip_prefix('(').unwrap_or(t);
        let t = t.strip_suffix(')').unwrap_or(t);
        let c: Vec<f64> = t.split(',').filter_map(as_num).collect();
        if c.len() >= 3 {
            pts.push(DVec3::new(c[0], c[1], c[2]));
        } else {
            pts.push(DVec3::ZERO); // keep indexing aligned
        }
    }
    Some(pts)
}

/// Read an `IfcCartesianPoint((x,y,z))`.
fn cartesian_point(records: &Records, id: u64) -> Option<DVec3> {
    let rec = records.get(&id)?;
    if rec.entity != "IFCCARTESIANPOINT" {
        return None;
    }
    let args = split_args(&rec.args);
    let inner = args.first()?.trim();
    let inner = inner.strip_prefix('(').unwrap_or(inner);
    let inner = inner.strip_suffix(')').unwrap_or(inner);
    let c: Vec<f64> = inner.split(',').filter_map(as_num).collect();
    if c.len() >= 3 {
        Some(DVec3::new(c[0], c[1], c[2]))
    } else if c.len() == 2 {
        Some(DVec3::new(c[0], c[1], 0.0))
    } else {
        None
    }
}

/// Parse a `(#a,#b,#c)` reference list.
fn ref_list(tok: &str) -> Vec<u64> {
    let t = tok.trim();
    let inner = t.strip_prefix('(').and_then(|s| s.strip_suffix(')')).unwrap_or(t);
    split_args(inner).iter().filter_map(|r| as_ref(r)).collect()
}

/// Strip a STEP single-quoted string literal (unescaping `''` → `'`), or None
/// if the token is `$`/`*`/not a string.
fn strip_step_string(tok: &str) -> Option<String> {
    let t = tok.trim();
    if !t.starts_with('\'') || !t.ends_with('\'') || t.len() < 2 {
        return None;
    }
    Some(t[1..t.len() - 1].replace("''", "'"))
}

/// Resolve an `IfcLocalPlacement` chain into a world transform. `depth` guards
/// against cyclic placements. `RelativePlacement` is an `IfcAxis2Placement3D`;
/// `PlacementRelTo` is the parent `IfcLocalPlacement`.
fn placement_matrix(records: &Records, id: u64, depth: u32) -> DMat4 {
    if depth > 64 {
        return DMat4::IDENTITY;
    }
    let Some(rec) = records.get(&id) else {
        return DMat4::IDENTITY;
    };
    if rec.entity != "IFCLOCALPLACEMENT" {
        return DMat4::IDENTITY;
    }
    // IfcLocalPlacement(PlacementRelTo, RelativePlacement).
    let args = split_args(&rec.args);
    if args.len() < 2 {
        return DMat4::IDENTITY;
    }
    let parent = as_ref(&args[0])
        .map(|p| placement_matrix(records, p, depth + 1))
        .unwrap_or(DMat4::IDENTITY);
    let local = as_ref(&args[1])
        .map(|a| axis2placement_matrix(records, a))
        .unwrap_or(DMat4::IDENTITY);
    parent * local
}

/// Turn an `IfcAxis2Placement3D(Location, Axis, RefDirection)` into a rigid
/// transform. Axis is the local Z; RefDirection seeds local X; both are
/// optional (default to world axes).
fn axis2placement_matrix(records: &Records, id: u64) -> DMat4 {
    let Some(rec) = records.get(&id) else {
        return DMat4::IDENTITY;
    };
    if rec.entity != "IFCAXIS2PLACEMENT3D" {
        return DMat4::IDENTITY;
    }
    let args = split_args(&rec.args);
    let location = args
        .first()
        .and_then(|a| as_ref(a))
        .and_then(|p| cartesian_point(records, p))
        .unwrap_or(DVec3::ZERO);
    let axis = args
        .get(1)
        .and_then(|a| as_ref(a))
        .and_then(|d| direction(records, d));
    let ref_dir = args
        .get(2)
        .and_then(|a| as_ref(a))
        .and_then(|d| direction(records, d));

    let z = axis.unwrap_or(DVec3::Z).normalize_or_zero();
    let z = if z.length_squared() < 1e-12 { DVec3::Z } else { z };
    // Project ref_dir onto the plane orthogonal to z to get x.
    let x_seed = ref_dir.unwrap_or(DVec3::X);
    let x = (x_seed - z * x_seed.dot(z)).normalize_or_zero();
    let x = if x.length_squared() < 1e-12 {
        // ref_dir parallel to z: pick any orthogonal axis.
        let alt = if z.x.abs() < 0.9 { DVec3::X } else { DVec3::Y };
        (alt - z * alt.dot(z)).normalize()
    } else {
        x
    };
    let y = z.cross(x);
    DMat4::from_cols(
        x.extend(0.0),
        y.extend(0.0),
        z.extend(0.0),
        location.extend(1.0),
    )
}

/// Read an `IfcDirection((x,y,z))` as a vector.
fn direction(records: &Records, id: u64) -> Option<DVec3> {
    let rec = records.get(&id)?;
    if rec.entity != "IFCDIRECTION" {
        return None;
    }
    let args = split_args(&rec.args);
    let inner = args.first()?.trim();
    let inner = inner.strip_prefix('(').unwrap_or(inner);
    let inner = inner.strip_suffix(')').unwrap_or(inner);
    let c: Vec<f64> = inner.split(',').filter_map(as_num).collect();
    if c.len() >= 3 {
        Some(DVec3::new(c[0], c[1], c[2]))
    } else if c.len() == 2 {
        Some(DVec3::new(c[0], c[1], 0.0))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{parse, Session};
    use kernel_mesh::signed_volume;

    fn run(s: &mut Session, line: &str) {
        s.run(parse(line).unwrap()).unwrap();
    }

    /// A little courtyard: four wall-ish boxes (each `box <corner> <size>`)
    /// around an open centre. Volumes: south/north = 10×1×3 = 30 each;
    /// west/east = 1×8×3 = 24 each.
    fn courtyard() -> Session {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 10,1,3"); // south wall: corner (0,0,0) size 10×1×3
        run(&mut s, "box 0,9,0 10,1,3"); // north wall
        run(&mut s, "box 0,1,0 1,8,3"); // west wall
        run(&mut s, "box 9,1,0 1,8,3"); // east wall
        s
    }

    #[test]
    fn export_has_ifc4_schema_and_structure() {
        let s = courtyard();
        let (bytes, detail) = export(&s.doc, "/tmp/courtyard.ifc").unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("FILE_SCHEMA(('IFC4'))"), "schema line missing");
        assert!(text.starts_with("ISO-10303-21;"));
        assert!(text.trim_end().ends_with("END-ISO-10303-21;"));
        // Spatial tree present.
        assert!(text.contains("IFCPROJECT("));
        assert!(text.contains("IFCSITE("));
        assert!(text.contains("IFCBUILDING("));
        assert!(text.contains("IFCBUILDINGSTOREY("));
        // Four elements, four triangulated face sets.
        assert_eq!(text.matches("IFCBUILDINGELEMENTPROXY(").count(), 4);
        assert_eq!(text.matches("IFCTRIANGULATEDFACESET(").count(), 4);
        assert_eq!(detail, "4 elements, 48 triangles");
    }

    #[test]
    fn export_carries_name_and_layer_suffix() {
        let mut s = Session::default();
        run(&mut s, "layer walls");
        run(&mut s, "box 0,0,0 2,2,2");
        run(&mut s, "name last tower");
        let (bytes, _) = export(&s.doc, "/tmp/named.ifc").unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("'tower [walls]'"), "name+layer suffix missing:\n{text}");
    }

    #[test]
    fn round_trip_mesh_count_and_volume() {
        let s = courtyard();
        let (bytes, _) = export(&s.doc, "/tmp/rt.ifc").unwrap();
        let parts = import(&bytes).unwrap();
        assert_eq!(parts.len(), 4, "four meshes round-trip");
        // Each wall box volume: south/north = 10*1*3 = 30; west/east = 1*8*3 = 24.
        let total: f64 = parts.iter().map(|(_, m)| signed_volume(m).abs()).sum();
        let expected = 30.0 + 30.0 + 24.0 + 24.0;
        assert!((total - expected).abs() < 1e-6, "volume {total} ≈ {expected}");
    }

    #[test]
    fn round_trip_preserves_placement() {
        // A box away from the origin must land back at the same place. Uses
        // `box <corner> <size>`: corner (100,50,10), size 2 → x spans 100..102,
        // centroid x = 101.
        let mut s = Session::default();
        run(&mut s, "box 100,50,10 2,2,2");
        let (bytes, _) = export(&s.doc, "/tmp/far.ifc").unwrap();
        let parts = import(&bytes).unwrap();
        assert_eq!(parts.len(), 1);
        let (_, mesh) = &parts[0];
        let cx = mesh.positions().iter().map(|p| p.x).sum::<f64>() / mesh.positions().len() as f64;
        assert!((cx - 101.0).abs() < 1e-6, "centroid x {cx} ≈ 101");
    }

    #[test]
    fn import_tolerates_junk_lines() {
        let mut ifc = String::from(
            "ISO-10303-21;\nHEADER;\nFILE_SCHEMA(('IFC4'));\nENDSEC;\nDATA;\n",
        );
        // Junk the parser must skip.
        ifc.push_str("garbage not an entity\n");
        ifc.push_str("#nope = broken;\n");
        ifc.push_str("/* a block comment spanning\n multiple lines */\n");
        // One valid face set (a single triangle) referenced by nothing.
        ifc.push_str("#1=IFCCARTESIANPOINTLIST3D(((0.,0.,0.),(1.,0.,0.),(0.,1.,0.)));\n");
        ifc.push_str("#2=IFCTRIANGULATEDFACESET(#1,$,.T.,((1,2,3)),$);\n");
        ifc.push_str("ENDSEC;\nEND-ISO-10303-21;\n");
        let parts = import(ifc.as_bytes()).unwrap();
        assert_eq!(parts.len(), 1, "one orphan triangle recovered");
        assert_eq!(parts[0].1.faces().len(), 1);
    }

    #[test]
    fn import_faceted_brep() {
        // A single triangular face expressed as an IfcFacetedBrep.
        let ifc = "\
ISO-10303-21;\nHEADER;\nFILE_SCHEMA(('IFC4'));\nENDSEC;\nDATA;\n\
#1=IFCCARTESIANPOINT((0.,0.,0.));\n\
#2=IFCCARTESIANPOINT((2.,0.,0.));\n\
#3=IFCCARTESIANPOINT((0.,2.,0.));\n\
#4=IFCPOLYLOOP((#1,#2,#3));\n\
#5=IFCFACEOUTERBOUND(#4,.T.);\n\
#6=IFCFACE((#5));\n\
#7=IFCCLOSEDSHELL((#6));\n\
#8=IFCFACETEDBREP(#7);\n\
ENDSEC;\nEND-ISO-10303-21;\n";
        let parts = import(ifc.as_bytes()).unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].1.faces().len(), 1);
        // The lone triangle: (0,0)-(2,0)-(0,2), area 2.
        let p = parts[0].1.positions();
        assert_eq!(p.len(), 3);
    }

    #[test]
    fn empty_document_exports_valid_shell() {
        let (bytes, detail) = export(&Document::default(), "/tmp/empty.ifc").unwrap();
        let text = String::from_utf8(bytes.clone()).unwrap();
        assert!(text.contains("FILE_SCHEMA(('IFC4'))"));
        assert!(text.contains("IFCPROJECT("));
        assert_eq!(text.matches("IFCBUILDINGELEMENTPROXY(").count(), 0);
        assert_eq!(detail, "0 elements, 0 triangles");
        // A no-element file has no containment relationship.
        assert!(!text.contains("IFCRELCONTAINEDINSPATIALSTRUCTURE"));
        // Re-importing yields no meshes but does not error.
        assert!(import(&bytes).unwrap().is_empty());
    }

    #[test]
    fn imperial_units_use_conversion_based_unit() {
        let mut s = Session::default();
        run(&mut s, "units ft");
        run(&mut s, "box 0,0,0 1,1,1");
        let (bytes, _) = export(&s.doc, "/tmp/imp.ifc").unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("IFCCONVERSIONBASEDUNIT"), "imperial unit missing");
        assert!(text.contains("'FOOT'"));
    }

    #[test]
    fn guid_is_deterministic_and_22_chars() {
        assert_eq!(guid(1).len(), 22);
        assert_eq!(guid(42), guid(42));
        assert_ne!(guid(1), guid(2));
    }

    #[test]
    fn num_always_has_decimal_point() {
        assert_eq!(num(1.0), "1.");
        assert_eq!(num(0.0), "0.");
        assert_eq!(num(-3.0), "-3.");
        assert!(num(1.5).contains('.'));
        assert_eq!(num(f64::NAN), "0.");
    }

    // ---- H-4: IFC zero-index underflow ----
    // CoordIndex containing (0,1,2) is invalid (IFC is 1-based); before the fix
    // `0u32 - 1` wraps to u32::MAX causing an OOB panic in Mesh.  After the fix
    // the offending face is silently skipped.
    #[test]
    fn ifc_zero_coord_index_does_not_panic() {
        // Three points, but CoordIndex references index 0 (invalid, 1-based).
        let ifc = "\
ISO-10303-21;\nHEADER;\nFILE_SCHEMA(('IFC4'));\nENDSEC;\nDATA;\n\
#1=IFCCARTESIANPOINTLIST3D(((0.,0.,0.),(1.,0.,0.),(0.,1.,0.)));\n\
#2=IFCTRIANGULATEDFACESET(#1,$,.T.,((0,1,2)),$);\n\
ENDSEC;\nEND-ISO-10303-21;\n";
        // The zero-indexed face must be dropped, yielding either empty or Err —
        // crucially it must NOT panic.
        let result = import(ifc.as_bytes()).unwrap();
        // The bad face was filtered; no valid mesh should survive.
        assert!(result.is_empty(), "bad face should be dropped, got {:?}", result.len());
    }

    // ---- H-4: IFC out-of-range index ----
    // CoordIndex (1,2,99) where there are only 3 points (max valid = 3).
    #[test]
    fn ifc_out_of_range_coord_index_does_not_panic() {
        let ifc = "\
ISO-10303-21;\nHEADER;\nFILE_SCHEMA(('IFC4'));\nENDSEC;\nDATA;\n\
#1=IFCCARTESIANPOINTLIST3D(((0.,0.,0.),(1.,0.,0.),(0.,1.,0.)));\n\
#2=IFCTRIANGULATEDFACESET(#1,$,.T.,((1,2,99)),$);\n\
ENDSEC;\nEND-ISO-10303-21;\n";
        let result = import(ifc.as_bytes()).unwrap();
        assert!(result.is_empty(), "OOB face should be dropped");
    }

    // ========================================================================
    // Structural analysis model
    // ========================================================================

    /// A small steel portal frame: two columns + a beam (I-section), a slab,
    /// a steel material, a point load and two supports. Enough to exercise
    /// every structural entity family.
    fn portal() -> Session {
        let mut s = Session::default();
        run(&mut s, "material steel 2.0e11 7850");
        run(&mut s, "section W12 iwf 0.3 0.2 0.015 0.01");
        run(&mut s, "section COL rect 0.3 0.3");
        run(&mut s, "section PIP pipe 0.2 0.01");
        // Two columns (0..3 in z), a beam across the top.
        run(&mut s, "column 0,0,0 0,0,3 COL material steel");
        run(&mut s, "column 5,0,0 5,0,3 COL material steel");
        run(&mut s, "beam 0,0,3 5,0,3 W12 material steel");
        // A floor slab boundary at z=3.
        run(&mut s, "slab 0,0,3 5,0,3 5,4,3 0,4,3 thick 0.2 material steel");
        // A downward point load at midspan.
        run(&mut s, "load point 2.5,0,3 10000 0,0,-1");
        // Supports at each column base.
        run(&mut s, "support 0,0,0 fixed");
        run(&mut s, "support 5,0,0 pinned");
        s
    }

    #[test]
    fn structural_model_has_analysis_entities() {
        let s = portal();
        let (bytes, detail) = export(&s.doc, "/tmp/portal.ifc").unwrap();
        let text = String::from_utf8(bytes).unwrap();

        // The analysis model shell.
        assert_eq!(text.matches("IFCSTRUCTURALANALYSISMODEL(").count(), 1);
        // Three frame members (2 columns + 1 beam).
        assert_eq!(text.matches("IFCSTRUCTURALCURVEMEMBER(").count(), 3);
        // One area surface member (the slab).
        assert_eq!(text.matches("IFCSTRUCTURALSURFACEMEMBER(").count(), 1);
        // Two supports → two point connections + boundary conditions.
        assert_eq!(text.matches("IFCSTRUCTURALPOINTCONNECTION(").count(), 2);
        assert_eq!(text.matches("IFCBOUNDARYNODECONDITION(").count(), 2);
        // One load group + one point action + one single force.
        assert_eq!(text.matches("IFCSTRUCTURALLOADGROUP(").count(), 1);
        assert_eq!(text.matches("IFCSTRUCTURALPOINTACTION(").count(), 1);
        assert_eq!(text.matches("IFCSTRUCTURALLOADSINGLEFORCE(").count(), 1);

        // Physical geometry export now uses *typed* products (IfcBeam/Column/
        // Slab) rather than anonymous proxies, so a semantic importer can
        // reconstruct typed members.
        assert_eq!(text.matches("IFCBEAM(").count(), 1);
        assert_eq!(text.matches("IFCCOLUMN(").count(), 2);
        assert_eq!(text.matches("IFCSLAB(").count(), 1);
        assert!(text.contains("IFCEXTRUDEDAREASOLID("), "typed members carry swept solids");
        assert!(text.contains("FILE_SCHEMA(('IFC4'))"));

        // Detail echo mentions the analysis tally.
        assert!(detail.contains("analysis:"), "detail: {detail}");
        assert!(detail.contains("3 members"), "detail: {detail}");
    }

    #[test]
    fn structural_model_emits_profile_family() {
        let s = portal();
        let (bytes, _) = export(&s.doc, "/tmp/prof.ifc").unwrap();
        let text = String::from_utf8(bytes).unwrap();
        // Each section family maps to its IfcProfileDef. Profiles are emitted
        // per member, once for the typed physical product and once for the
        // analysis member: the beam I-section → 2 IfcIShapeProfileDef, the two
        // columns' rect → 4 IfcRectangleProfileDef.
        assert_eq!(text.matches("IFCISHAPEPROFILEDEF(").count(), 2);
        assert_eq!(text.matches("IFCRECTANGLEPROFILEDEF(").count(), 4);
        // The pipe section is defined but unused by any member; unused sections
        // are not emitted.
        assert_eq!(text.matches("IFCCIRCLEHOLLOWPROFILEDEF(").count(), 0);
    }

    #[test]
    fn structural_model_emits_material_with_properties() {
        let s = portal();
        let (bytes, _) = export(&s.doc, "/tmp/mat.ifc").unwrap();
        let text = String::from_utf8(bytes).unwrap();
        // One IfcMaterial for the analysis model, one for the typed products.
        assert_eq!(text.matches("IFCMATERIAL(").count(), 2);
        assert!(text.contains("'steel'"), "material name missing");
        // Mechanical properties carry Young's modulus.
        assert_eq!(text.matches("IFCMECHANICALMATERIALPROPERTIES(").count(), 1);
        assert!(text.contains("200000000000."), "E not written as 2e11");
        // Density recorded.
        assert!(text.contains("IFCMASSDENSITYMEASURE(7850."), "density missing");
        // Members associate the material.
        assert!(text.contains("IFCRELASSOCIATESMATERIAL("));
        assert!(text.contains("IFCMATERIALPROFILESET("));
    }

    #[test]
    fn boundary_conditions_reflect_restraint_kind() {
        let s = portal();
        let (bytes, _) = export(&s.doc, "/tmp/bc.ifc").unwrap();
        let text = String::from_utf8(bytes).unwrap();
        // Fixed: all six .T.  Pinned: three translational .T., rotations $.
        assert!(
            text.contains("IFCBOUNDARYNODECONDITION('fixed',.T.,.T.,.T.,.T.,.T.,.T.)"),
            "fixed BC missing"
        );
        assert!(
            text.contains("IFCBOUNDARYNODECONDITION('pinned',.T.,.T.,.T.,$,$,$)"),
            "pinned BC missing"
        );
    }

    #[test]
    fn roller_support_frees_axis() {
        let mut s = Session::default();
        run(&mut s, "section COL rect 0.3 0.3");
        run(&mut s, "column 0,0,0 0,0,3 COL");
        run(&mut s, "support 0,0,0 roller 1,0,0");
        let (bytes, _) = export(&s.doc, "/tmp/roll.ifc").unwrap();
        let text = String::from_utf8(bytes).unwrap();
        // X free ($), Y and Z fixed (.T.).
        assert!(
            text.contains("IFCBOUNDARYNODECONDITION('roller',$,.T.,.T.,$,$,$)"),
            "roller X-free BC missing"
        );
    }

    #[test]
    fn load_kinds_map_to_action_entities() {
        let mut s = Session::default();
        run(&mut s, "load point 0,0,0 100 0,0,-1");
        run(&mut s, "load line 0,0,0 1,0,0 50 0,0,-1");
        run(&mut s, "load area 0,0,0 1,0,0 1,1,0 0,1,0 end 25 0,0,-1");
        let (bytes, _) = export(&s.doc, "/tmp/loads.ifc").unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert_eq!(text.matches("IFCSTRUCTURALPOINTACTION(").count(), 1);
        assert_eq!(text.matches("IFCSTRUCTURALLINEARACTION(").count(), 1);
        assert_eq!(text.matches("IFCSTRUCTURALPLANARACTION(").count(), 1);
        assert_eq!(text.matches("IFCSTRUCTURALLOADSINGLEFORCE(").count(), 3);
    }

    #[test]
    fn no_structural_data_emits_no_analysis_model() {
        // The courtyard has only plain boxes: no frames, areas, loads, supports.
        let s = courtyard();
        let (bytes, detail) = export(&s.doc, "/tmp/nostruct.ifc").unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(!text.contains("IFCSTRUCTURALANALYSISMODEL("), "unexpected analysis model");
        assert!(!detail.contains("analysis:"), "detail: {detail}");
        // Physical proxies still there.
        assert_eq!(text.matches("IFCBUILDINGELEMENTPROXY(").count(), 4);
    }

    #[test]
    fn structural_members_carry_topology_and_placement() {
        let s = portal();
        let (bytes, _) = export(&s.doc, "/tmp/topo.ifc").unwrap();
        let text = String::from_utf8(bytes).unwrap();
        // Curve members use edges; surface members use face surfaces.
        assert!(text.contains("IFCEDGE("), "no member edge topology");
        assert!(text.contains("IFCFACESURFACE("), "no surface topology");
        assert!(text.contains("IFCTOPOLOGYREPRESENTATION("));
        // The analysis model is serviced onto the building.
        assert_eq!(text.matches("IFCRELSERVICESBUILDINGS(").count(), 1);
        // Group assignment wires members into the model.
        assert!(text.contains("IFCRELASSIGNSTOGROUP("));
    }

    // ---- Valid IFC with good indices still loads correctly after guard ----
    #[test]
    fn ifc_valid_coord_index_still_loads() {
        let ifc = "\
ISO-10303-21;\nHEADER;\nFILE_SCHEMA(('IFC4'));\nENDSEC;\nDATA;\n\
#1=IFCCARTESIANPOINTLIST3D(((0.,0.,0.),(1.,0.,0.),(0.,1.,0.)));\n\
#2=IFCTRIANGULATEDFACESET(#1,$,.T.,((1,2,3)),$);\n\
ENDSEC;\nEND-ISO-10303-21;\n";
        let result = import(ifc.as_bytes()).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].1.faces().len(), 1);
    }

    // ========================================================================
    // Semantic BIM import — typed member reconstruction
    // ========================================================================

    /// (a) Round-trip: export a structural frame to IFC, import it back, and
    /// assert the SAME typed members, positions, sections, materials and
    /// stories reconstruct — as typed members, not meshes.
    #[test]
    fn semantic_round_trip_reconstructs_typed_members() {
        let mut s = portal();
        // Give the members a story so IfcBuildingStorey round-trips too.
        run(&mut s, "story L1 0");
        run(&mut s, "story Roof 3");
        let (bytes, _) = export(&s.doc, "/tmp/portal_rt.ifc").unwrap();

        let sem = import_semantic(&bytes).unwrap();

        // Frame members: 2 columns + 1 beam.
        let frames: Vec<_> = sem
            .elements
            .iter()
            .filter(|e| matches!(e, ImportedElement::Frame { .. }))
            .collect();
        assert_eq!(frames.len(), 3, "3 frame members reconstruct");
        let beams = frames
            .iter()
            .filter(|e| matches!(e, ImportedElement::Frame { kind: FrameKind::Beam, .. }))
            .count();
        let cols = frames
            .iter()
            .filter(|e| matches!(e, ImportedElement::Frame { kind: FrameKind::Column, .. }))
            .count();
        assert_eq!(beams, 1, "one beam");
        assert_eq!(cols, 2, "two columns");

        // Area member: the slab.
        let areas: Vec<_> = sem
            .elements
            .iter()
            .filter(|e| matches!(e, ImportedElement::Area { kind: AreaKind::Slab, .. }))
            .collect();
        assert_eq!(areas.len(), 1, "one slab");

        // No mesh fallback — everything reconstructed typed.
        assert!(
            !sem.elements.iter().any(|e| matches!(e, ImportedElement::Mesh { .. })),
            "typed members must not fall back to meshes"
        );

        // Positions: find the beam and check its endpoints (0,0,3)-(5,0,3).
        let beam = frames
            .iter()
            .find(|e| matches!(e, ImportedElement::Frame { kind: FrameKind::Beam, .. }))
            .unwrap();
        if let ImportedElement::Frame { a, b, section, material, .. } = beam {
            let (a, b) = (*a, *b);
            let ends_match = ((a - DVec3::new(0., 0., 3.)).length() < 1e-6
                && (b - DVec3::new(5., 0., 3.)).length() < 1e-6)
                || ((b - DVec3::new(0., 0., 3.)).length() < 1e-6
                    && (a - DVec3::new(5., 0., 3.)).length() < 1e-6);
            assert!(ends_match, "beam endpoints wrong: {a:?} {b:?}");
            // Section: the W12 I-section (d=0.3, bf=0.2, tf=0.015, tw=0.01).
            let sec = sem.sections.iter().find(|(n, _)| n == section).unwrap().1;
            assert_eq!(
                sec,
                Section::IWideFlange { d: 0.3, bf: 0.2, tf: 0.015, tw: 0.01 },
                "I-section dims round-trip"
            );
            assert_eq!(material.as_deref(), Some("steel"), "beam material round-trips");
        }

        // Material properties reconstruct (E=2e11, density=7850).
        let steel = sem.materials.iter().find(|m| m.name == "steel").unwrap();
        assert!((steel.elastic_modulus_e - 2.0e11).abs() < 1.0);
        assert!((steel.density - 7850.0).abs() < 1e-6);

        // Stories reconstruct and members are assigned by elevation.
        assert!(sem.stories.iter().any(|st| st.name == "L1"));
        assert!(sem.stories.iter().any(|st| st.name == "Roof"));
        // The beam at z=3 lands on the "Roof" story.
        if let ImportedElement::Frame { story, .. } = beam {
            assert_eq!(story.as_deref(), Some("Roof"), "beam assigned to Roof story");
        }
    }

    /// Full logged-command path: the exec-level IFC import turns typed members
    /// into `FrameMember`/`AreaMember`/`DefSection`/`DefMaterial`/`DefStory`
    /// substrate commands, so the reconstructed document holds real typed
    /// `Geometry::Frame` / `Geometry::Area` objects (replay-safe).
    #[test]
    fn semantic_import_creates_typed_geometry_via_logged_commands() {
        let mut s = portal();
        let path = std::env::temp_dir().join("portal_exec_rt.ifc");
        let (bytes, _) = export(&s.doc, path.to_str().unwrap()).unwrap();
        std::fs::write(&path, &bytes).unwrap();

        // Import into a fresh session via the substrate command.
        let mut t = Session::default();
        t.run(crate::Command::Import { path: path.to_string_lossy().into_owned() })
            .unwrap();

        let frames = t
            .doc
            .objects()
            .filter(|o| matches!(o.geometry, Geometry::Frame { .. }))
            .count();
        let areas = t
            .doc
            .objects()
            .filter(|o| matches!(o.geometry, Geometry::Area { .. }))
            .count();
        assert_eq!(frames, 3, "3 typed frame members created");
        assert_eq!(areas, 1, "1 typed area member created");
        assert!(t.doc.materials.contains_key("steel"), "material defined");
        assert!(!t.doc.sections.is_empty(), "sections defined");
        let _ = &mut s;
    }

    /// (b) An unknown IFC entity (a bare proxy mesh) still imports as a mesh on
    /// the 'ifc' layer.
    #[test]
    fn semantic_unknown_entity_falls_back_to_mesh() {
        // A proxy with a triangulated face set — not a typed structural member.
        let ifc = "\
ISO-10303-21;\nHEADER;\nFILE_SCHEMA(('IFC4'));\nENDSEC;\nDATA;\n\
#1=IFCCARTESIANPOINTLIST3D(((0.,0.,0.),(1.,0.,0.),(0.,1.,0.)));\n\
#2=IFCTRIANGULATEDFACESET(#1,$,.T.,((1,2,3)),$);\n\
#3=IFCSHAPEREPRESENTATION($,'Body','Tessellation',(#2));\n\
#4=IFCPRODUCTDEFINITIONSHAPE($,$,(#3));\n\
#5=IFCBUILDINGELEMENTPROXY('guid',$,'widget',$,$,$,#4,$,$);\n\
ENDSEC;\nEND-ISO-10303-21;\n";
        let sem = import_semantic(ifc.as_bytes()).unwrap();
        assert_eq!(sem.elements.len(), 1);
        assert!(matches!(&sem.elements[0], ImportedElement::Mesh { .. }));

        // And through the exec path it lands on the 'ifc' layer.
        let path = std::env::temp_dir().join("proxy_only.ifc");
        std::fs::write(&path, ifc).unwrap();
        let mut t = Session::default();
        t.run(crate::Command::Import { path: path.to_string_lossy().into_owned() })
            .unwrap();
        assert!(
            t.doc.objects().any(|o| o.layer == "ifc"),
            "unknown geometry lands on the 'ifc' layer"
        );
    }

    /// (c) A malformed IFC is tolerated: no panic, and a file with no geometry
    /// yields a clear error rather than crashing.
    #[test]
    fn semantic_malformed_ifc_is_tolerated() {
        // Truncated / garbage content — must not panic.
        let junk = "ISO-10303-21;\nHEADER;\n#broken=IFCBEAM(oops\nno closing paren";
        let sem = import_semantic(junk.as_bytes()).unwrap();
        assert!(sem.elements.is_empty(), "no elements from garbage");

        // A beam that references a missing profile/placement must degrade
        // gracefully (skip, do not panic).
        let partial = "\
ISO-10303-21;\nHEADER;\nFILE_SCHEMA(('IFC4'));\nENDSEC;\nDATA;\n\
#5=IFCBEAM('g',$,'b',$,$,#99,#98,$,.BEAM.);\n\
ENDSEC;\nEND-ISO-10303-21;\n";
        let sem = import_semantic(partial.as_bytes()).unwrap();
        // The dangling beam has no resolvable body → dropped, no panic.
        assert!(sem.elements.is_empty());

        // Non-UTF-8 bytes give a clear error, not a panic.
        let bad = [0xff, 0xfe, 0x00, 0x01];
        assert!(import_semantic(&bad).is_err());
    }
}



