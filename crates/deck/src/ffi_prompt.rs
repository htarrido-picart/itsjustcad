// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! The system prompt for the embedded (FFI / iOS) cloud deck (#105).
//!
//! The desktop prompts advertise desktop app verbs (`ze`, `zs`, `display`,
//! `panel`, …) that the FFI does not route, so a model that framed its work
//! with `ze` got "unknown command 'ze'". This prompt advertises exactly the
//! view verbs the FFI routes, states the selector forms (never a bare id),
//! carries the exact usage of the everyday core verbs plus the verbs retrieved
//! for THIS request, and lists the whole registry by usage line. Nothing here
//! relies on `help <verb>`: the FFI deck is single-turn, nothing feeds back.

use itsjustcad_commands::{PluginRegistry, registry};

use crate::prompt::{
    CATEGORY_ORDER, CLARIFY_HELP, CODECHECK_HELP, ENVIRO_CRITIQUE_HELP, PLAN_HELP, RAMP_STAIR_HELP,
    TABLE_HELP, YIELD_CRITIQUE_HELP, category_label,
};
use crate::retrieve::{CORE_VERBS, GATED_VERBS, retrieved_block, tokens};

/// How many verbs retrieval may add per request, and their token budget.
pub const RETRIEVE_LIMIT: usize = 6;
pub const RETRIEVE_TOKEN_BUDGET: usize = 900;

/// The view/camera verbs the FFI routes (see `route_line` in the ffi crate).
/// A test there asserts every verb named here is accepted.
pub const FFI_VIEW_HELP: &str = "\
## View verbs (only when the user asks for a view; never needed after drawing — the app frames new geometry itself)
  top | bottom | front | back | left | right | persp     standard view            e.g. top
  view <name>                                             same, by name            e.g. view front
  orbit <dx> <dy>                                         orbit the camera         e.g. orbit 30 0
  pan <dx> <dy>                                           pan the camera           e.g. pan 1 0
  zoom in | zoom out                                      zoom                     e.g. zoom in
  ze                                                      zoom to fit everything   e.g. ze
No other view, display, lighting, panel or window verbs exist here.
";

/// Selector forms, with the bare-id rule (#105).
pub const FFI_SELECTOR_HELP: &str = "\
## Selectors — how a command names its target
Every <selector> argument is exactly one of:
  last          the newest object            e.g. volume last
  last N        the N newest objects         e.g. union last 2
  all           every object                 e.g. delete all
  sel           the current selection        e.g. move sel 0,0,5
  id:<hex>      an id from the scene list    e.g. volume id:59d76a88
  <name>        an object's name             e.g. move tower 0,0,5
NEVER write a bare id: `volume 59d76a88` is rejected — write `volume id:59d76a88`. With one object in the scene, or for the object you just made, `last` is simplest.
";

/// The usage line of each core verb, plus the everyday shape mappings.
fn core_block() -> String {
    let mut out = String::from("## Core verbs (exact syntax)\n");
    for name in CORE_VERBS.iter().chain(std::iter::once(&"geodesic")) {
        if let Some(spec) = registry().iter().find(|s| s.name == *name) {
            out.push_str(&format!("  {}\n", spec.usage));
        }
    }
    out.push_str(
        "Shapes: a sphere or ball is `geodesic 3 <radius> full` (built at the origin), then `move last <center>` if it goes elsewhere; a dome is `geodesic 3 <radius> dome`. A cube is a box. A cylinder, column or disk is `circle <center> <radius>` then `extrude last <height>`. A box is placed by its MIN corner: centered at c with size s → corner c − s/2. A grid of copies is ONE `array`; a ring of copies is `polararray`.\n",
    );
    out
}

/// Every registry verb by usage line, grouped by category. Gated
/// (filesystem / network) verbs are left out: the FFI refuses them.
fn usage_catalog() -> String {
    let mut out = String::new();
    for &cat in CATEGORY_ORDER {
        let mut lines = String::new();
        for spec in registry()
            .iter()
            .filter(|s| s.category == cat && !GATED_VERBS.contains(&s.name))
        {
            lines.push_str(&format!("  {}\n", spec.usage));
        }
        if !lines.is_empty() {
            out.push_str(&format!("### {}\n{}", category_label(cat), lines));
        }
    }
    out
}

/// Workflow guidance sections, included only when the request is about them.
fn guidance_for(request: &str) -> String {
    let words = tokens(request);
    let has = |list: &[&str]| words.iter().any(|w| list.contains(&w.as_str()));
    let mut out = String::new();
    if has(&["ramp", "stair", "staircase", "step", "riser", "tread"]) {
        out.push_str(RAMP_STAIR_HELP);
    }
    if has(&["sun", "sunhour", "shadow", "solar", "radiation", "daylight"]) {
        out.push_str(ENVIRO_CRITIQUE_HELP);
    }
    if has(&[
        "codecheck",
        "code",
        "compliance",
        "comply",
        "ibc",
        "ada",
        "egress",
        "accessible",
    ]) {
        out.push_str(CODECHECK_HELP);
    }
    if has(&[
        "lot",
        "lotreport",
        "yield",
        "far",
        "subdivide",
        "subdivision",
        "site",
    ]) {
        out.push_str(YIELD_CRITIQUE_HELP);
    }
    out
}

/// Host "here" context for the FFI deck (#128): the points the host knows
/// when the user says "here", "there" or "where I'm looking". Set through
/// `ijc_deck_set_context`; every field is optional.
#[derive(Debug, Clone, Copy, Default, PartialEq, serde::Deserialize)]
pub struct HereContext {
    /// The last long-press or tap point (the host drops it after 2 min).
    pub tap: Option<[f64; 3]>,
    /// The centroid of the selection.
    pub selection: Option<[f64; 3]>,
    /// The camera target.
    pub camera: Option<[f64; 3]>,
}

impl HereContext {
    /// Parse `{"here": {"tap": [x,y,z], "selection": [x,y,z], "camera": [x,y,z]}}`.
    /// `Ok(None)` clears: blank text, or no point set. `Err` on anything else
    /// that is not that shape.
    pub fn from_json(json: &str) -> Result<Option<Self>, String> {
        #[derive(serde::Deserialize)]
        struct Wire {
            here: Option<HereContext>,
        }
        if json.trim().is_empty() {
            return Ok(None);
        }
        let v: serde_json::Value = serde_json::from_str(json).map_err(|e| e.to_string())?;
        if !v.is_object() || !(v["here"].is_object() || v["here"].is_null()) {
            return Err("expected {\"here\": {…}}".into());
        }
        let wire: Wire = serde_json::from_value(v).map_err(|e| e.to_string())?;
        Ok(wire.here.filter(|h| *h != Self::default()))
    }

    /// The prompt line, e.g. `here: tap 4.0,2.0,0.0 | camera 0.0,0.0,0.0`.
    fn line(&self) -> String {
        let parts: Vec<String> = [
            ("tap", self.tap),
            ("selection", self.selection),
            ("camera", self.camera),
        ]
        .into_iter()
        .filter_map(|(k, p)| p.map(|[x, y, z]| format!("{k} {x:.1},{y:.1},{z:.1}")))
        .collect();
        format!("here: {}\n", parts.join(" | "))
    }
}

/// The rules for the `here:` line, added to `## Rules` only when it is set.
const HERE_RULES: &str = "\
- \"here\"/\"there\" mean the `here:` point under Current scene; \"where I'm looking\"/\"in front of me\" mean its `camera`. Write its real coordinates, never a `here` token.
- `tap` and `selection` are the candidates: with exactly one, use it; with both within 2 m, use `tap`. With only `camera`, use it, and say in the chat where you placed the object.
- With `tap` and `selection` more than 2 m apart, emit NO commands: reply with one QUESTION naming both options, e.g. `QUESTION: At the point you pressed, or at the selected column?`
";

fn plugin_block(plugins: &PluginRegistry) -> String {
    if plugins.is_empty() {
        return String::new();
    }
    let mut out = String::from("## Plugins (user macros — call by name)\n");
    for p in plugins.iter() {
        let mut usage = crate::digest::sanitize_name(&p.name);
        for param in &p.params {
            usage.push(' ');
            usage.push_str(&crate::digest::sanitize_name(&param.name));
        }
        let summary = if p.description.is_empty() {
            format!("Plugin macro ({} line(s)).", p.body.len())
        } else {
            crate::digest::sanitize_name(&p.description)
        };
        out.push_str(&format!("  {usage:<24} {summary}\n"));
    }
    out
}

/// The FFI deck's system prompt for one request. `here` adds the `here:` line
/// and its rules; with `None` the prompt has neither.
pub fn ffi_system_prompt(
    scene_digest: &str,
    plugins: &PluginRegistry,
    request: &str,
    here: Option<&HereContext>,
) -> String {
    let retrieved = retrieved_block(request, RETRIEVE_LIMIT, RETRIEVE_TOKEN_BUDGET);
    format!(
        r#"You are the drafting companion inside ItsJustCAD, a CAD program for architects. You model by emitting commands — the same commands the human types. Coordinates are meters, Z is up, the ground plane is z=0.

## How to answer
One short sentence of chat, then a ```draft fenced block with ONE command per line — ALL the commands the request needs, in order; they run live as you stream. Text outside the block is chat. Do ONLY what was asked: no extra framing, view, color, naming or verification commands. NEVER claim you drew or changed anything without the ```draft commands that do it. Use only verbs listed below, with exactly the argument order their usage shows. Do not emit `help`: nothing is fed back to you this turn.

{selectors}
{core}
{retrieved}
## Full command list (usage lines)
{catalog}
{plugins}
{view}
{guidance}{clarify}
{plan}
{tables}
## Rules
- Points are x,y,z or x,y (z=0), with no spaces inside a point. Bare numbers are meters; 250cm and 500mm also work.
- After a command that creates an object, that object is `last`; `last N` is the N newest. Create every object BEFORE you reference it.
- Edit objects that are already in the scene by selector (`last`, `id:<hex>`, a name) — never recreate them.
- To make a solid from a profile: draw a closed curve, then `extrude last <height>`.
{here_rules}
## Example
User: a 4x4x3 tower, then a 5 by 2 grid of it 8 apart
```draft
box 0,0,0 4,4,3
array last 5,2,1 8,8,0
```

## Current scene (ids are for `id:<hex>` selectors)
{here_line}{scene}
"#,
        selectors = FFI_SELECTOR_HELP,
        core = core_block(),
        retrieved = retrieved,
        catalog = usage_catalog(),
        plugins = plugin_block(plugins),
        view = FFI_VIEW_HELP,
        guidance = guidance_for(request),
        clarify = CLARIFY_HELP,
        plan = PLAN_HELP,
        tables = TABLE_HELP,
        here_rules = if here.is_some() { HERE_RULES } else { "" },
        here_line = here.map(HereContext::line).unwrap_or_default(),
        scene = if scene_digest.trim().is_empty() {
            "(empty)"
        } else {
            scene_digest
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::retrieve::estimate_tokens;

    fn p(request: &str) -> String {
        ffi_system_prompt(
            "- mesh 59d76a88 bbox [0.0,0.0,0.0]..[10.0,10.0,10.0]\n",
            &PluginRegistry::new(),
            request,
            None,
        )
    }

    #[test]
    fn here_context_parses_clears_and_rejects() {
        let h = HereContext::from_json(r#"{"here":{"tap":[4,2,0],"camera":[0,0,0]}}"#)
            .unwrap()
            .unwrap();
        assert_eq!(h.tap, Some([4.0, 2.0, 0.0]));
        assert_eq!(h.selection, None);
        assert_eq!(h.line(), "here: tap 4.0,2.0,0.0 | camera 0.0,0.0,0.0\n");
        for clear in ["", "  ", "{}", r#"{"here":null}"#, r#"{"here":{}}"#] {
            assert_eq!(HereContext::from_json(clear), Ok(None), "{clear:?}");
        }
        for bad in [
            "{",
            "[]",
            r#"{"here":[[1,2,3]]}"#,
            r#"{"here":{"tap":[1,2]}}"#,
            r#"{"here":{"tap":"4,2,0"}}"#,
        ] {
            assert!(HereContext::from_json(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn here_context_adds_only_its_line_and_rules() {
        let none = p("put a box here");
        assert!(!none.contains("here:") && !none.contains("QUESTION naming"));
        let here = HereContext {
            tap: Some([4.0, 2.0, 0.0]),
            selection: Some([10.0, 3.0, 0.0]),
            camera: Some([0.0, 0.0, 0.0]),
        };
        let with = ffi_system_prompt(
            "- mesh 59d76a88 bbox [0.0,0.0,0.0]..[10.0,10.0,10.0]\n",
            &PluginRegistry::new(),
            "put a box here",
            Some(&here),
        );
        let line = "here: tap 4.0,2.0,0.0 | selection 10.0,3.0,0.0 | camera 0.0,0.0,0.0\n";
        assert!(with.contains(&format!("selectors)\n{line}- mesh 59d76a88")));
        let rules_at = with.find(HERE_RULES).expect("rules");
        assert!(with[..rules_at].ends_with("`extrude last <height>`.\n"));
        assert!(rules_at < with.find("## Example").unwrap());
        // Without context the prompt is byte-identical: removing the two
        // additions gives it back exactly.
        assert_eq!(with.replacen(HERE_RULES, "", 1).replacen(line, "", 1), none);
    }

    #[test]
    fn states_selector_forms_and_bans_bare_ids() {
        let s = p("what's the volume of the box");
        for form in [
            "last N",
            "`all`",
            "sel",
            "id:<hex>",
            "<name>",
            "NEVER write a bare id",
        ] {
            assert!(
                s.contains(form) || s.contains(&form.replace('`', "")),
                "missing {form}"
            );
        }
    }

    #[test]
    fn advertises_no_desktop_only_app_verbs() {
        let s = p("draw a geodesic dome with radius 8");
        for verb in [
            "zs",
            "display",
            "panel",
            "sketchup",
            "dock",
            "meshedges",
            "gumball",
            "light",
        ] {
            assert!(
                !s.lines()
                    .any(|l| l.split_whitespace().next() == Some(verb)),
                "FFI prompt advertises desktop-only `{verb}`"
            );
        }
        assert!(
            !s.contains("help <verb>"),
            "the FFI deck is single-turn; no help round-trip"
        );
    }

    #[test]
    fn lists_every_ungated_registry_usage_and_no_gated_verb() {
        let s = p("anything");
        for spec in registry() {
            if GATED_VERBS.contains(&spec.name) {
                assert!(
                    !s.contains(&format!("  {}\n", spec.usage)),
                    "gated {} advertised",
                    spec.name
                );
            } else {
                assert!(s.contains(spec.usage), "missing usage for {}", spec.name);
            }
        }
    }

    #[test]
    fn retrieved_verbs_ride_with_their_summary() {
        let s = p("a Dieste-style undulating brick vault 6 wide 12 long");
        assert!(s.contains("Verbs for THIS request"));
        let gv = registry().iter().find(|x| x.name == "gaussvault").unwrap();
        assert!(s.contains(gv.summary), "gaussvault summary missing");
        assert!(!p("draw a box 4 by 4 by 3").contains("Verbs for THIS request"));
    }

    #[test]
    fn stays_well_under_the_desktop_full_prompt() {
        let full = crate::prompt::system_prompt("", &PluginRegistry::new());
        let s = p("Candela saddle shell");
        assert!(
            estimate_tokens(&s) < 9_000,
            "ffi prompt ~{} tokens",
            estimate_tokens(&s)
        );
        assert!(
            s.len() * 3 < full.len(),
            "ffi {} vs full {}",
            s.len(),
            full.len()
        );
    }

    #[test]
    fn guidance_sections_follow_the_request() {
        assert!(p("a staircase rising 3 m").contains("Ramp & stair layout"));
        assert!(!p("draw a box").contains("Ramp & stair layout"));
        assert!(p("run a codecheck").contains("Compliance pre-checks"));
    }
}
