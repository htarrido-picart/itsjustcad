// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

use itsjustcad_commands::{registry, PluginRegistry, SELECTOR_HELP};

/// The "View & camera commands" section of the deck system prompt.
///
/// These are *app verbs*, not substrate [`registry`] commands: they change what
/// the viewport shows (framing, display mode, lighting, NPR styling, camera
/// projection/lens, site basemap) without mutating the drawing or the op-log,
/// so they have no `itsjustcad_commands::Command` and cannot flow through the
/// registry-driven command list. We advertise them here as a dedicated section
/// so the model knows they exist with correct syntax and one example each. The
/// app dispatches them via `itsjustcad::app_verbs::classify`; a test there
/// asserts every line advertised here actually classifies, so the prompt can
/// never promise syntax the dispatcher rejects.
pub const VIEW_VERB_HELP: &str = "\
## View & camera commands (app verbs — same ```draft block; change what's shown, not the model)
These NEVER modify the drawing or the op-log; they frame/style the active viewport exactly like the human's command line. Emit them inside the ```draft block just like drawing commands. Use them when the user asks to reframe, orbit to a view, zoom, change lens/projection, or restyle the viewport — never to draw geometry.

Framing & standard views:
  ze                                           zoom to fit all geometry (alias: zoomextents)             e.g. ze
  top|bottom|front|back|left|right|persp        set a standard view direction                            e.g. top
  view <name>                                   same, by name (perspective = persp)                      e.g. view front

Display mode (how solids are drawn):
  display shaded|wireframe|xray|ghosted|pencil  viewport display mode                                    e.g. display pencil
  sketchup                                      SketchUp look preset (working light + profile edges +    e.g. sketchup
                                                gradient background; alias: su)

Lighting:
  light working|sun|presentation                lighting model (alias: lightmode)                        e.g. light sun

Transform gizmo:
  gumball on|off|toggle                         show/hide the transform gumball (G hotkey = bare toggle) e.g. gumball on

Non-photoreal (NPR) styling:
  sketchy on|off                                hand-drawn 'sketchy edges' character                     e.g. sketchy on
  edgefx jitter=.. extension=.. depthcue=..     tune the sketchy edge effect (key=value tokens)          e.g. edgefx jitter=.05 extension=.1
        endpoints=.. passes=..
  profiles on|off                               thick SketchUp-style profile edges (alias: profileedges) e.g. profiles on

Camera projection & lens:
  camera 2point                                 two-point perspective (verticals stay vertical)          e.g. camera 2point
  camera persp                                  ordinary perspective                                     e.g. camera persp
  camera pano                                   360° equirectangular panorama                            e.g. camera pano
  camera fisheye [fov]                          fisheye projection, optional field of view in degrees    e.g. camera fisheye 120
  camera <n>mm                                  lens by focal length: 15mm 24mm 35mm 50mm 85mm           e.g. camera 35mm
  camera phone <lens>                           phone-camera sim; lenses: iphone-main iphone-ultrawide   e.g. camera phone iphone-ultrawide
                                                iphone-tele pixel-main pixel-ultrawide pixel-tele
                                                galaxy-main galaxy-ultrawide galaxy-tele (bare = iphone-main)

Site context:
  basemap [osm|sat] [span_m] [opacity]          georeferenced satellite/OSM underlay (basemap off        e.g. basemap sat 800 0.6
                                                clears)
";

/// Build the system prompt from the command registry (single source of truth)
/// plus a compact scene digest. Regenerated every turn so the model always
/// sees current geometry.
///
/// `plugins` are user/LLM-authored macros — the LLM must see them so it can
/// call plugin verbs directly (`<pluginname> args...`) and author new ones via
/// the `plugin define` command.
pub fn system_prompt(scene_digest: &str, plugins: &PluginRegistry) -> String {
    let mut commands = String::new();
    for spec in registry() {
        commands.push_str(&format!("  {:<44} {}\n", spec.usage, spec.summary));
    }

    // Plugin macros (if any) as callable verbs, plus the authoring commands.
    let mut plugin_block = String::new();
    if !plugins.is_empty() {
        plugin_block.push_str("\n## Plugins (user macros — call by name)\n");
        for p in plugins.iter() {
            plugin_block.push_str(&format!("  {:<44} {}\n", p.usage(), p.summary()));
        }
    }
    plugin_block.push_str(
        "\nYou can author a reusable macro mid-conversation with:\n  plugin define {\"name\":\"<name>\",\"description\":\"...\",\"params\":[{\"name\":\"h\",\"default\":\"3\"}],\"body\":[\"rect 0,0 {0} {0}\",\"extrude last {h}\"]}\nBody lines are command templates; {0} {1} (or {param-name}) substitute positional args. Invoke it later as `<name> arg1 arg2`.\n",
    );

    format!(
        r#"You are the drafting companion inside ItsJustCAD, a CAD program for architects. You model by emitting commands — the same commands the human types. Coordinates are meters, Z is up, the ground plane is z=0.

## How to draw
Emit commands inside a ```draft fenced block, ONE command per line. Commands execute live as you stream them. Text outside the block is chat shown to the architect. Keep chat brief.

## Commands
{commands}
{selectors}
{plugin_block}

{view_verbs}
## Rules
- Points are x,y,z or x,y (z=0). No spaces inside a point. Units: bare numbers are meters; 250cm and 500mm also work.
- 'last' refers to the most recently created object; 'last N' to the N most recent. After a command that creates an object, that object is 'last'.
- To make a solid: draw a closed profile (rect/circle/polygon/closed polyline), then 'extrude last <height>'.
- Name important objects ('name last core') so you can refer to them later.
- If a command fails you will receive the error text; correct it and re-emit only the failed/remaining commands.

## Answering workflow questions
When the user asks HOW to do something (rather than asking you to draw), explain the workflow step by step, citing exact commands. Only execute (emit a ```draft block) if they explicitly ask you to draw or model it.
Example workflow — walls from a centerline:
  offset <centerline> 0.1   (outer face)
  offset <centerline> -0.1  (inner face)
  extrude last 2 3          (both offsets to wall height)
  difference <outer> <inner> (cut hollow wall)

## Example
User: make two 4x4x3 towers 10m apart
```draft
box 0,0,0 4,4,3
box 10,0,0 4,4,3
```

## Current scene
{scene}
"#,
        commands = commands,
        selectors = SELECTOR_HELP,
        plugin_block = plugin_block,
        view_verbs = VIEW_VERB_HELP,
        scene = if scene_digest.is_empty() {
            "(empty)"
        } else {
            scene_digest
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use itsjustcad_commands::{Plugin, PluginParam};

    #[test]
    fn system_prompt_lists_every_registry_command() {
        let prompt = system_prompt("", &PluginRegistry::new());
        for spec in registry() {
            assert!(
                prompt.contains(spec.usage),
                "prompt missing usage for '{}'",
                spec.name
            );
            assert!(
                prompt.contains(spec.summary),
                "prompt missing summary for '{}'",
                spec.name
            );
        }
        assert!(prompt.contains(SELECTOR_HELP));
    }

    #[test]
    fn empty_scene_digest_renders_placeholder() {
        assert!(system_prompt("", &PluginRegistry::new()).contains("(empty)"));
    }

    #[test]
    fn scene_digest_is_embedded_verbatim() {
        let prompt = system_prompt("abc1234 box 5x5x3 'core'", &PluginRegistry::new());
        assert!(prompt.contains("abc1234 box 5x5x3 'core'"));
        assert!(!prompt.contains("(empty)"));
    }

    #[test]
    fn plugin_verbs_appear_in_prompt() {
        let mut reg = PluginRegistry::new();
        reg.insert(Plugin {
            name: "column-grid".into(),
            description: "Grid of columns".into(),
            category: None,
            params: vec![PluginParam { name: "nx".into(), default: Some("5".into()) }],
            body: vec!["box 0,0,0 0.4,0.4,3".into()],
        });
        let prompt = system_prompt("", &reg);
        assert!(prompt.contains("column-grid <nx>"), "{prompt}");
        assert!(prompt.contains("Grid of columns"));
        // The authoring instruction is always present.
        assert!(prompt.contains("plugin define"));
    }

    #[test]
    fn authoring_instruction_present_without_plugins() {
        assert!(system_prompt("", &PluginRegistry::new()).contains("plugin define"));
    }

    #[test]
    fn prompt_advertises_view_and_camera_app_verbs() {
        let p = system_prompt("", &PluginRegistry::new());
        // The whole dedicated section is embedded verbatim.
        assert!(p.contains(VIEW_VERB_HELP));
        // Section header.
        assert!(p.contains("## View & camera commands"));
        // Framing / zoom-extents.
        assert!(p.contains("ze"));
        assert!(p.contains("zoom to fit all geometry"));
        // Standard views.
        assert!(p.contains("top|bottom|front|back|left|right|persp"));
        // Display modes (all five) + sketchup preset.
        assert!(p.contains("display shaded|wireframe|xray|ghosted|pencil"));
        assert!(p.contains("display pencil"));
        assert!(p.contains("sketchup"));
        // Lighting.
        assert!(p.contains("light working|sun|presentation"));
        // NPR.
        assert!(p.contains("sketchy on|off"));
        assert!(p.contains("edgefx jitter="));
        assert!(p.contains("profiles on|off"));
        // Camera projections + lenses + phone sims.
        assert!(p.contains("camera 2point"));
        assert!(p.contains("camera pano"));
        assert!(p.contains("camera fisheye [fov]"));
        assert!(p.contains("camera <n>mm"));
        assert!(p.contains("camera phone <lens>"));
        assert!(p.contains("iphone-ultrawide"));
        assert!(p.contains("pixel-main"));
        assert!(p.contains("galaxy-tele"));
        // Basemap.
        assert!(p.contains("basemap [osm|sat]"));
        // Gumball — must be advertised so the model stops saying it doesn't exist.
        assert!(p.contains("gumball on|off|toggle"), "gumball missing from VIEW_VERB_HELP");
        assert!(p.contains("G hotkey"), "gumball G-hotkey note missing");
    }

}
