use mydrafter_doc::{Document, Geometry};

pub use mydrafter_render::{snapshot, Theme};

/// Compact scene description for the LLM system prompt.
/// Selected objects are marked `[SELECTED]` and receive a full detail block
/// (size, centroid) so prompts like "make THIS taller" always resolve correctly.
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

    let sel_count = doc.selection.len();
    if sel_count > 0 {
        out.push_str(&format!("selection: {sel_count} object(s)\n"));
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
        let hidden = if obj.visible { "" } else { " (hidden)" };
        let selected = doc.selection.contains(&obj.id);
        let sel_tag = if selected { " [SELECTED]" } else { "" };

        if selected {
            // Full detail for selected objects: size + centroid so the model
            // can emit precise move/scale/extrude targets.
            let size = bb.max - bb.min;
            let center = (bb.min + bb.max) * 0.5;
            out.push_str(&format!(
                "- {kind} {id}{name}{layer}{hidden}{sel_tag} bbox [{:.3},{:.3},{:.3}]..[{:.3},{:.3},{:.3}] size {:.3}x{:.3}x{:.3} center {:.3},{:.3},{:.3}\n",
                bb.min.x, bb.min.y, bb.min.z,
                bb.max.x, bb.max.y, bb.max.z,
                size.x, size.y, size.z,
                center.x, center.y, center.z,
                id = obj.id,
            ));
        } else {
            out.push_str(&format!(
                "- {kind} {id}{name}{layer}{hidden} bbox [{:.1},{:.1},{:.1}]..[{:.1},{:.1},{:.1}]\n",
                bb.min.x, bb.min.y, bb.min.z, bb.max.x, bb.max.y, bb.max.z,
                id = obj.id,
            ));
        }
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

    #[test]
    fn digest_selected_objects_have_full_detail() {
        let s = session_with(&["box 0,0,0 4,3,2", "select last"]);
        let d = digest(&s.doc);
        assert!(d.contains("[SELECTED]"), "selected tag missing\n{d}");
        assert!(d.contains("size "), "size missing\n{d}");
        assert!(d.contains("center "), "center missing\n{d}");
        // Exact size should be 4x3x2.
        assert!(d.contains("size 4.000x3.000x2.000"), "wrong size\n{d}");
    }

    #[test]
    fn digest_selection_count_header() {
        let s = session_with(&["box 0,0,0 1,1,1", "box 5,0,0 1,1,1", "select last 2"]);
        let d = digest(&s.doc);
        assert!(d.contains("selection: 2 object(s)"), "{d}");
    }

    #[test]
    fn digest_unselected_objects_use_compact_format() {
        // Two boxes, only the second selected — first should NOT have full detail.
        let s = session_with(&["box 0,0,0 1,1,1", "box 5,0,0 1,1,1", "select last"]);
        let d = digest(&s.doc);
        // The first box line: bbox with 1 decimal, no [SELECTED].
        let first_line = d.lines().find(|l| l.contains("0.0,0.0,0.0")).unwrap_or("");
        assert!(!first_line.contains("[SELECTED]"), "first box should not be selected\n{d}");
        // The second box line: has [SELECTED] and 3-decimal coords.
        let second_line = d.lines().find(|l| l.contains("[SELECTED]")).unwrap_or("");
        assert!(!second_line.is_empty(), "no selected line found\n{d}");
        assert!(second_line.contains("size "), "{d}");
    }
}
