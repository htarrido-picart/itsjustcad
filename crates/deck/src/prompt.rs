use mydrafter_commands::{registry, SELECTOR_HELP};

/// Build the system prompt from the command registry (single source of truth)
/// plus a compact scene digest. Regenerated every turn so the model always
/// sees current geometry.
pub fn system_prompt(scene_digest: &str) -> String {
    let mut commands = String::new();
    for spec in registry() {
        commands.push_str(&format!("  {:<44} {}\n", spec.usage, spec.summary));
    }

    format!(
        r#"You are the drafting companion inside mydrafter, a CAD program for architects. You model by emitting commands — the same commands the human types. Coordinates are meters, Z is up, the ground plane is z=0.

## How to draw
Emit commands inside a ```draft fenced block, ONE command per line. Commands execute live as you stream them. Text outside the block is chat shown to the architect. Keep chat brief.

## Commands
{commands}
{selectors}

## Rules
- Points are x,y,z or x,y (z=0). No spaces inside a point. Units: bare numbers are meters; 250cm and 500mm also work.
- 'last' refers to the most recently created object; 'last N' to the N most recent. After a command that creates an object, that object is 'last'.
- To make a solid: draw a closed profile (rect/circle/polygon/closed polyline), then 'extrude last <height>'.
- Name important objects ('name last core') so you can refer to them later.
- If a command fails you will receive the error text; correct it and re-emit only the failed/remaining commands.

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

    #[test]
    fn system_prompt_lists_every_registry_command() {
        let prompt = system_prompt("");
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
        assert!(system_prompt("").contains("(empty)"));
    }

    #[test]
    fn scene_digest_is_embedded_verbatim() {
        let prompt = system_prompt("abc1234 box 5x5x3 'core'");
        assert!(prompt.contains("abc1234 box 5x5x3 'core'"));
        assert!(!prompt.contains("(empty)"));
    }
}
