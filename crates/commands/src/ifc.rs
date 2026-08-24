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
use itsjustcad_doc::{Document, Geometry, Units, METERS_PER_FOOT, METERS_PER_INCH};

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

/// Collect mesh objects in document order. Curves and annotations are dropped
/// (IFC has no lightweight place for our wireframe primitives).
fn collect(doc: &Document) -> Vec<Part> {
    let mut parts = Vec::new();
    for obj in doc.objects() {
        if let Geometry::Mesh(m) = &obj.geometry {
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

    let file = assemble(path, &body, unit_note);
    let detail = format!(
        "{} element{}, {total_tris} triangles",
        parts.len(),
        if parts.len() == 1 { "" } else { "s" }
    );
    Ok((file.into_bytes(), detail))
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
        &format!("IFCUNITASSIGNMENT((#{length_unit},#{sqm},#{cbm}))"),
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
}



