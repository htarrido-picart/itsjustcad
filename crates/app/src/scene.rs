use mydrafter_doc::{Document, Geometry};

pub use mydrafter_render::{snapshot, Theme};

/// Compact scene description for the LLM system prompt.
pub fn digest(doc: &Document) -> String {
    const MAX_LISTED: usize = 40;
    let mut out = String::new();
    // Layers line only when there is something beyond the untouched default.
    if doc.layers.len() > 1 || doc.current_layer != mydrafter_doc::DEFAULT_LAYER {
        let list: Vec<String> = doc
            .layers
            .iter()
            .map(|(name, style)| {
                format!("{name}{}", if style.visible { "" } else { " (hidden)" })
            })
            .collect();
        out.push_str(&format!(
            "layers (current '{}'): {}\n",
            doc.current_layer,
            list.join(", ")
        ));
    }
    for (i, obj) in doc.objects().enumerate() {
        if i == MAX_LISTED {
            out.push_str(&format!("… and {} more objects\n", doc.len() - MAX_LISTED));
            break;
        }
        let bb = obj.geometry.aabb();
        let kind = match &obj.geometry {
            Geometry::Mesh(_) => "mesh",
            Geometry::Curve(_) => "curve",
            Geometry::Annotation(a) => match a {
                mydrafter_doc::Annotation::LinearDim { .. } => "dim",
                mydrafter_doc::Annotation::Text { .. } => "text",
                mydrafter_doc::Annotation::Hatch { .. } => "hatch",
            },
        };
        let name = obj
            .name
            .as_deref()
            .map(|n| format!(" '{n}'"))
            .unwrap_or_default();
        let layer = if obj.layer == mydrafter_doc::DEFAULT_LAYER {
            String::new()
        } else {
            format!(" layer '{}'", obj.layer)
        };
        out.push_str(&format!(
            "- {kind} {id}{name}{layer} bbox [{:.1},{:.1},{:.1}]..[{:.1},{:.1},{:.1}]\n",
            bb.min.x, bb.min.y, bb.min.z, bb.max.x, bb.max.y, bb.max.z,
            id = obj.id,
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use mydrafter_commands::{parse, Session};

    fn session_with(lines: &[&str]) -> Session {
        let mut s = Session::default();
        for line in lines {
            s.run(parse(line).unwrap()).unwrap();
        }
        s
    }

    #[test]
    fn digest_omits_layers_line_for_untouched_default() {
        let s = session_with(&["box 0,0,0 1,1,1"]);
        let d = digest(&s.doc);
        assert!(!d.contains("layers"), "{d}");
        assert!(!d.contains("layer '"), "{d}");
    }

    #[test]
    fn digest_mentions_layers_and_hidden_state() {
        let s = session_with(&[
            "box 0,0,0 1,1,1",
            "layer walls",
            "box 5,0,0 1,1,1",
            "hide walls",
        ]);
        let d = digest(&s.doc);
        assert!(d.contains("layers (current 'walls'): default, walls (hidden)"), "{d}");
        assert!(d.contains("layer 'walls'"), "{d}");
    }
}
