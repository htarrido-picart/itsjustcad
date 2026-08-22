use mydrafter_doc::{Document, Geometry};

pub use mydrafter_render::snapshot;

/// Compact scene description for the LLM system prompt.
pub fn digest(doc: &Document) -> String {
    const MAX_LISTED: usize = 40;
    let mut out = String::new();
    for (i, obj) in doc.objects().enumerate() {
        if i == MAX_LISTED {
            out.push_str(&format!("… and {} more objects\n", doc.len() - MAX_LISTED));
            break;
        }
        let bb = obj.geometry.aabb();
        let kind = match &obj.geometry {
            Geometry::Mesh(_) => "mesh",
            Geometry::Curve(_) => "curve",
        };
        let name = obj
            .name
            .as_deref()
            .map(|n| format!(" '{n}'"))
            .unwrap_or_default();
        out.push_str(&format!(
            "- {kind} {id}{name} bbox [{:.1},{:.1},{:.1}]..[{:.1},{:.1},{:.1}]\n",
            bb.min.x, bb.min.y, bb.min.z, bb.max.x, bb.max.y, bb.max.z,
            id = obj.id,
        ));
    }
    out
}
