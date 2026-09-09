// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Schema-driven editor for the Parameters tab (M-parametric).
//!
//! The mapping from a generator's [`ParamField`] schema to a list of UI
//! controls is a PURE, testable function ([`field_controls`]); the actual
//! painting ([`render_field`]) is thin egui glue over it. A new generator gets a
//! full editor for free from its schema — no per-generator UI code.
//!
//! Every edit is written back into the caller's live [`ParamMap`]; the caller
//! (in `app.rs`) debounces and commits it as one `paramset` op, so op-log /
//! undo / replay invariants hold.

use itsjustcad_doc::{FieldKind, ParamField, ParamMap, ParamValue, Widget};

/// The control kind the editor renders for a schema field — the pure result of
/// mapping a [`ParamField`] to a widget. Unit-tested so the schema→UI contract
/// can be asserted without a GPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    /// Bounded slider (Float/Int with min+max).
    Slider,
    /// Drag-value / typed numeric (unbounded or precise Float/Int).
    Numeric,
    /// Enum dropdown.
    Dropdown,
    /// Boolean toggle.
    Toggle,
    /// Three numeric fields (x, y, z) for a Vec3/Point.
    Point,
}

/// Pure schema → control mapping. A bounded Float/Int with a `Slider` widget
/// yields a [`Control::Slider`]; an unbounded or `Numeric`-widget number yields
/// [`Control::Numeric`]; enums a dropdown, bools a toggle, vec3 a point. This is
/// the single source the editor paints from and the test asserts against.
pub fn field_control(field: &ParamField) -> Control {
    match field.kind {
        FieldKind::Bool => Control::Toggle,
        FieldKind::Enum => Control::Dropdown,
        FieldKind::Vec3 => Control::Point,
        FieldKind::Float | FieldKind::Int => {
            let bounded = field.min.is_some() && field.max.is_some();
            match field.widget {
                Widget::Slider if bounded => Control::Slider,
                _ => Control::Numeric,
            }
        }
    }
}

/// The ordered control list for a whole schema — one [`Control`] per field.
/// The pure schema→UI contract asserted in tests; `render_field` paints from
/// `field_control` per field, so this convenience wrapper is test-only in the
/// non-test build.
#[allow(dead_code)]
pub fn field_controls(fields: &[ParamField]) -> Vec<Control> {
    fields.iter().map(field_control).collect()
}

/// Serialize a [`ParamValue`] to the `key=value` token the `paramset` verb
/// parses. Mirrors `exec::parse_param_value`.
pub fn value_to_token(v: &ParamValue) -> String {
    match v {
        ParamValue::Float(x) => format!("{x}"),
        ParamValue::Int(x) => x.to_string(),
        ParamValue::Bool(b) => b.to_string(),
        ParamValue::Enum(e) => e.clone(),
        ParamValue::Vec3(v) => format!("{},{},{}", v[0], v[1], v[2]),
    }
}

/// Paint the control for one schema `field`, reading/writing `values`. Returns
/// `(changed, active)`: `changed` = the value moved this frame; `active` = a
/// widget is still being dragged (so the caller keeps the commit debounced).
pub fn render_field(
    ui: &mut egui::Ui,
    field: &ParamField,
    values: &mut ParamMap,
) -> (bool, bool) {
    use crate::i18n::t;
    let label = t(field.label_key);
    let mut changed = false;
    let mut active = false;
    let cur = values.get(field.name).cloned().unwrap_or_else(|| field.default.clone());

    match field_control(field) {
        Control::Slider => {
            // Slider over the declared [min, max] range. Int fields snap to whole
            // steps; float fields use the field's step for the drag granularity.
            let (mn, mx) = (field.min.unwrap_or(0.0), field.max.unwrap_or(1.0));
            if field.kind == FieldKind::Int {
                let mut x = cur.as_i64().unwrap_or(0);
                let resp = ui.add(
                    egui::Slider::new(&mut x, (mn as i64)..=(mx as i64)).text(label),
                );
                if resp.changed() {
                    values.insert(field.name.into(), ParamValue::Int(x));
                    changed = true;
                }
                active |= resp.dragged();
            } else {
                let mut x = cur.as_f64().unwrap_or(0.0);
                let resp = ui.add(
                    egui::Slider::new(&mut x, mn..=mx)
                        .step_by(field.step)
                        .suffix(field.unit.suffix())
                        .text(label),
                );
                if resp.changed() {
                    values.insert(field.name.into(), ParamValue::Float(x));
                    changed = true;
                }
                active |= resp.dragged();
            }
        }
        Control::Numeric => {
            ui.horizontal(|ui| {
                ui.label(label);
                if field.kind == FieldKind::Int {
                    let mut x = cur.as_i64().unwrap_or(0);
                    let resp = ui.add(egui::DragValue::new(&mut x).speed(field.step));
                    if resp.changed() {
                        values.insert(field.name.into(), ParamValue::Int(x));
                        changed = true;
                    }
                    active |= resp.dragged();
                } else {
                    let mut x = cur.as_f64().unwrap_or(0.0);
                    let resp = ui.add(
                        egui::DragValue::new(&mut x)
                            .speed(field.step)
                            .suffix(field.unit.suffix()),
                    );
                    if resp.changed() {
                        values.insert(field.name.into(), ParamValue::Float(x));
                        changed = true;
                    }
                    active |= resp.dragged();
                }
            });
        }
        Control::Dropdown => {
            let cur_s = cur.as_enum().unwrap_or("").to_string();
            ui.horizontal(|ui| {
                ui.label(label);
                egui::ComboBox::from_id_salt(field.name)
                    .selected_text(cur_s.clone())
                    .show_ui(ui, |ui| {
                        for choice in &field.choices {
                            if ui
                                .selectable_label(cur_s == *choice, *choice)
                                .clicked()
                            {
                                values.insert(
                                    field.name.into(),
                                    ParamValue::Enum((*choice).to_string()),
                                );
                                changed = true;
                            }
                        }
                    });
            });
        }
        Control::Toggle => {
            let mut b = cur.as_bool().unwrap_or(false);
            if ui.checkbox(&mut b, label).changed() {
                values.insert(field.name.into(), ParamValue::Bool(b));
                changed = true;
            }
        }
        Control::Point => {
            let mut v = cur.as_vec3().unwrap_or(glam::DVec3::ZERO);
            ui.label(label);
            ui.horizontal(|ui| {
                for (comp, axis) in [(&mut v.x, "x"), (&mut v.y, "y"), (&mut v.z, "z")] {
                    ui.label(axis);
                    let resp = ui.add(egui::DragValue::new(comp).speed(field.step));
                    if resp.changed() {
                        changed = true;
                    }
                    active |= resp.dragged();
                }
            });
            if changed {
                values.insert(field.name.into(), ParamValue::Vec3(v.to_array()));
            }
        }
    }
    (changed, active)
}

#[cfg(test)]
mod tests {
    use super::*;
    use itsjustcad_doc::GeneratorKind;

    #[test]
    fn geodesic_schema_maps_to_slider_numeric_dropdown() {
        let schema = GeneratorKind::Geodesic.schema();
        let controls = field_controls(&schema.fields);
        // frequency (Int, bounded, Slider) → Slider; radius (Numeric) → Numeric;
        // mode (Enum) → Dropdown.
        assert_eq!(controls, vec![Control::Slider, Control::Numeric, Control::Dropdown]);
    }

    #[test]
    fn bool_maps_to_toggle_and_vec3_to_point() {
        let schema = GeneratorKind::Funicular.schema();
        let map: std::collections::BTreeMap<_, _> = schema
            .fields
            .iter()
            .map(|f| (f.name, field_control(f)))
            .collect();
        assert_eq!(map["support_a"], Control::Point);
        assert_eq!(map["invert"], Control::Toggle);
        assert_eq!(map["segments"], Control::Slider);
    }

    #[test]
    fn every_generator_field_maps_to_a_control() {
        for &k in GeneratorKind::ALL {
            let schema = k.schema();
            let controls = field_controls(&schema.fields);
            assert_eq!(controls.len(), schema.fields.len(), "{:?} control count", k);
        }
    }

    #[test]
    fn value_tokens_round_trip_through_paramset_parse_shape() {
        // Tokens must be re-parseable in the same shape the verb expects.
        assert_eq!(value_to_token(&ParamValue::Int(5)), "5");
        assert_eq!(value_to_token(&ParamValue::Bool(true)), "true");
        assert_eq!(value_to_token(&ParamValue::Enum("dome".into())), "dome");
        assert_eq!(value_to_token(&ParamValue::Vec3([1.0, 2.0, 3.0])), "1,2,3");
    }
}
