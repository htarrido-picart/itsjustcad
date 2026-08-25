// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

use itsjustcad_commands::{registry, PluginRegistry, SELECTOR_HELP};

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
}
