/// One entry per command verb. Single source of truth for command-line help
/// AND the LLM system prompt (`deck` renders this table into the prompt).
pub struct CommandSpec {
    pub name: &'static str,
    pub usage: &'static str,
    pub summary: &'static str,
}

pub fn registry() -> &'static [CommandSpec] {
    &[
        CommandSpec {
            name: "box",
            usage: "box <corner x,y,z> <size x,y,z>",
            summary: "Axis-aligned box. Example: box 0,0,0 5,5,3",
        },
        CommandSpec {
            name: "extrude",
            usage: "extrude <selector> <height>",
            summary: "Extrude a closed profile curve upward. Example: extrude last 3",
        },
        CommandSpec {
            name: "line",
            usage: "line <a x,y,z> <b x,y,z>",
            summary: "Line segment. Example: line 0,0,0 10,0,0",
        },
        CommandSpec {
            name: "polyline",
            usage: "polyline <p1> <p2> ... [closed]",
            summary: "Polyline through points; append 'closed' to close it. Example: polyline 0,0 5,0 5,5 closed",
        },
        CommandSpec {
            name: "rect",
            usage: "rect <corner x,y,z> <width> <height>",
            summary: "Rectangle in the XY plane. Example: rect 0,0,0 4 6",
        },
        CommandSpec {
            name: "circle",
            usage: "circle <center x,y,z> <radius>",
            summary: "Circle in the XY plane. Example: circle 0,0,0 2.5",
        },
        CommandSpec {
            name: "arc",
            usage: "arc <center x,y,z> <radius> <start deg> <end deg>",
            summary: "CCW arc in the XY plane. Example: arc 0,0,0 5 0 90",
        },
        CommandSpec {
            name: "ellipse",
            usage: "ellipse <center x,y,z> <rx> <ry>",
            summary: "Ellipse in the XY plane. Example: ellipse 0,0,0 4 2",
        },
        CommandSpec {
            name: "polygon",
            usage: "polygon <center x,y,z> <radius> <sides>",
            summary: "Regular polygon in the XY plane. Example: polygon 0,0,0 3 6",
        },
        CommandSpec {
            name: "curve",
            usage: "curve <p1> <p2> <p3> ... [degree N]",
            summary: "NURBS curve by control points (default degree 3). Example: curve 0,0 2,4 6,4 8,0",
        },
        CommandSpec {
            name: "union",
            usage: "union <selector>",
            summary: "Merge 2+ meshes into one solid; inputs are consumed. Example: union last 2",
        },
        CommandSpec {
            name: "difference",
            usage: "difference <target selector> <tool selector>",
            summary: "Subtract tool meshes from target meshes (cut holes, courtyards); inputs are consumed and tools win overlapping selectors. Make the tool slightly taller/deeper than the target for clean through-cuts. Examples: difference tower core · difference last 2 last",
        },
        CommandSpec {
            name: "intersect",
            usage: "intersect <selector>",
            summary: "Keep only the shared volume of 2+ meshes; inputs are consumed. Example: intersect last 2",
        },
        CommandSpec {
            name: "move",
            usage: "move <selector> <delta x,y,z>",
            summary: "Translate objects. Example: move last 5,0,0",
        },
        CommandSpec {
            name: "rotate",
            usage: "rotate <selector> <angle deg> [x|y|z] [about <x,y,z>]",
            summary: "Rotate objects (default: about z through their bounding-box center). Example: rotate last 45 · rotate all 90 x about 0,0,0",
        },
        CommandSpec {
            name: "scale",
            usage: "scale <selector> <factor | fx,fy,fz> [about <x,y,z>]",
            summary: "Scale objects uniformly or per-axis (default: about their bounding-box center). Example: scale last 2 · scale last 1,1,2",
        },
        CommandSpec {
            name: "offset",
            usage: "offset <selector> <distance>",
            summary: "Offset a curve in the XY plane; original kept. Closed curves: positive = outward, negative = inward (walls from centerlines: offset both ways, extrude). Example: offset last 0.2",
        },
        CommandSpec {
            name: "mirror",
            usage: "mirror <selector> <xy|yz|xz | point normal>",
            summary: "Mirror objects across a plane. Example: mirror last yz · mirror last 0,5,0 0,1,0",
        },
        CommandSpec {
            name: "copy",
            usage: "copy <selector> <delta x,y,z>",
            summary: "Duplicate objects with an offset. Example: copy last 6,0,0",
        },
        CommandSpec {
            name: "delete",
            usage: "delete <selector>",
            summary: "Delete objects. Example: delete last",
        },
        CommandSpec {
            name: "name",
            usage: "name <selector> <name>",
            summary: "Name objects for later reference. Example: name last tower-a",
        },
        CommandSpec {
            name: "select",
            usage: "select <selector>",
            summary: "Set the selection. Example: select all",
        },
        CommandSpec {
            name: "selectnone",
            usage: "selectnone",
            summary: "Clear the selection",
        },
        CommandSpec {
            name: "undo",
            usage: "undo",
            summary: "Undo the last geometry command",
        },
        CommandSpec {
            name: "redo",
            usage: "redo",
            summary: "Redo",
        },
    ]
}

/// Selector grammar shared by help and the LLM prompt.
pub const SELECTOR_HELP: &str =
    "Selectors: 'last' (most recent object), 'last N' (N most recent), 'all', 'sel' (current selection), or an object name.";
