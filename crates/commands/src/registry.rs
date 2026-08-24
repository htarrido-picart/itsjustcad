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
            name: "revolve",
            usage: "revolve <selector> [axis point x,y,z] [axis dir x,y,z] [angle deg]",
            summary: "Revolve one closed profile curve into a solid (default: full circle about the z axis through the origin; partial angles are capped). Draw the profile beside the axis, e.g. in the xz plane with x,0,z points. Example: revolve last · revolve vase 0,0,0 0,0,1 270",
        },
        CommandSpec {
            name: "loft",
            usage: "loft <selector>",
            summary: "Skin 2+ closed curves (stacked in creation order) into one capped solid; profiles are resampled to matching point counts. Example: loft last 3",
        },
        CommandSpec {
            name: "sweep",
            usage: "sweep <profile selector> <rail selector>",
            summary: "Sweep one closed profile curve along an open rail curve (profile centered on the rail, no-twist frames, capped ends). Example: sweep prof rail",
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
            name: "dim",
            usage: "dim <a x,y,z> <b x,y,z> [offset]",
            summary: "Linear dimension between two points; the measured distance is displayed automatically. Offset (default 0.5) places the dimension line left of a->b. Example: dim 0,0 10,0 0.8",
        },
        CommandSpec {
            name: "text",
            usage: "text <pos x,y,z> <words...> [height]",
            summary: "Text annotation at a point; trailing number = text height in meters (default 0.2). Example: text 5,3 living room 0.3",
        },
        CommandSpec {
            name: "hatch",
            usage: "hatch <selector> [solid | lines [angle spacing] | crosshatch [angle spacing] | brick [spacing] | concrete [spacing] | insulation [spacing] | earth [spacing]]",
            summary: "Hatch the region of a closed curve. Patterns: solid (fill), lines (parallel, default 45° 0.25m), crosshatch (two perpendicular sets), brick (running bond, horizontal courses), concrete (dash-dot scatter), insulation (batt zigzag), earth (45° short dashes). Example: hatch last · hatch last brick 0.2 · hatch last insulation 0.3",
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
            name: "section",
            usage: "section <selector> <plane point x,y,z> <normal x,y,z>",
            summary: "Cut meshes with a plane; each closed intersection loop becomes a heavy closed polyline on layer 'sections', and feature edges beyond the plane are projected onto it as thin polylines on 'sections-proj' (originals kept). Example: section all 0,0,1.2 0,0,1 · section tower 5,0,0 1,0,0 (vertical cross-section)",
        },
        CommandSpec {
            name: "plan",
            usage: "plan <height>",
            summary: "Architect's plan cut: horizontal section of every mesh at the given height; wall outlines and courtyard holes land as heavy closed polylines on layer 'sections', and edges of geometry below the cut (furniture, floor) are projected down as thin polylines on 'sections-proj'. Cut ~1.2m above the floor of interest. Example: plan 1.2 · plan 4.7 (second floor at 3.5m + 1.2m)",
        },
        CommandSpec {
            name: "elevation",
            usage: "elevation <north|south|east|west> [depth]",
            summary: "Orthographic side-view outline: feature edges of all geometry projected onto the vertical plane for the compass direction (no cutting), as thin polylines on layer 'elevations'. 'north' is the face seen looking south. Example: elevation south · elevation east 2",
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
            name: "split",
            usage: "split <selector> <point x,y>",
            summary: "Split a curve in two at the nearest point on it to the given point; the original is replaced by the pieces. Example: split last 5,0",
        },
        CommandSpec {
            name: "trim",
            usage: "trim <target selector> <cutter selector> <keep point x,y>",
            summary: "Cut a curve where it crosses the cutter curve(s) and keep only the piece nearest the keep point; the rest is removed. Example: trim wall slab 1,1",
        },
        CommandSpec {
            name: "extend",
            usage: "extend <selector> <distance>",
            summary: "Extend both open ends of curves by a distance (lines/polylines tangentially, arcs along their circle). Example: extend last 0.5",
        },
        CommandSpec {
            name: "join",
            usage: "join <selector>",
            summary: "Join end-touching curves (1e-6 gap tolerance) into one polyline; inputs are consumed, arcs are tessellated. Example: join last 3",
        },
        CommandSpec {
            name: "fillet",
            usage: "fillet <a selector> <b selector> <radius> | fillet <selector matching 2> <radius>",
            summary: "Round the corner between two lines with a tangent arc, trimming both lines to the tangency points. Example: fillet last 2 0.5",
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
            name: "array",
            usage: "array <selector> <nx,ny,nz> <dx,dy,dz>",
            summary: "Grid of copies at the given spacings; the originals fill cell 0,0,0 (nx,ny works too, nz=1). Example: box 0,0,0 0.4,0.4,3 then array last 5,3,1 3,4,0 makes a 5x3 column grid at 3m x 4m bays",
        },
        CommandSpec {
            name: "polararray",
            usage: "polararray <selector> <count> [center x,y,z] [total angle deg]",
            summary: "Circular array of copies about the z axis (default: full circle about the targets' bounding-box center; count includes the original). Example: polararray last 8 · polararray col 6 0,0,0 180",
        },
        CommandSpec {
            name: "delete",
            usage: "delete <selector>",
            summary: "Delete objects. Example: delete last",
        },
        CommandSpec {
            name: "group",
            usage: "group <selector> [name]",
            summary: "Group objects under a name (auto-named group1, group2... when omitted). Clicking any member selects the whole group, and the group name works as a selector. Example: group last 2 boxes",
        },
        CommandSpec {
            name: "ungroup",
            usage: "ungroup <selector>",
            summary: "Dissolve every group containing the selected objects; the objects themselves stay. Example: ungroup boxes",
        },
        CommandSpec {
            name: "name",
            usage: "name <selector> <name>",
            summary: "Name objects for later reference. Example: name last tower-a",
        },
        CommandSpec {
            name: "layer",
            usage: "layer <name>",
            summary: "Create (if needed) and switch the current layer; new objects land on it. Example: layer walls",
        },
        CommandSpec {
            name: "tolayer",
            usage: "tolayer <selector> <name>",
            summary: "Move objects onto a layer (created if needed). Example: tolayer last 2 structure",
        },
        CommandSpec {
            name: "layercolor",
            usage: "layercolor <layer> <r,g,b>",
            summary: "Set a layer's display color, 0-1 or 0-255 values. Example: layercolor walls 0.8,0.2,0.1",
        },
        CommandSpec {
            name: "layerweight",
            usage: "layerweight <layer> <mm>",
            summary: "Set a layer's print lineweight in mm (controls PDF stroke width and DXF code-370). Default 0.18 mm; sections layer convention is 0.35 mm. Example: layerweight sections 0.35",
        },
        CommandSpec {
            name: "hide",
            usage: "hide <layer>",
            summary: "Hide a layer (objects stay in the model, invisible). Example: hide walls",
        },
        CommandSpec {
            name: "show",
            usage: "show <layer>",
            summary: "Show a hidden layer. Example: show walls",
        },
        CommandSpec {
            name: "hideobj",
            usage: "hideobj <selector>",
            summary: "Hide individual objects (they stay in the model, invisible and unpickable). Example: hideobj last 2",
        },
        CommandSpec {
            name: "showobj",
            usage: "showobj <selector>",
            summary: "Show hidden objects ('showobj all' reveals everything). Example: showobj all",
        },
        CommandSpec {
            name: "color",
            usage: "color <selector> <r,g,b | off>",
            summary: "Set or clear a per-object color override (0-1 or 0-255 values). Beats the layer color. 'color last off' clears it. Example: color last 1,0.3,0 · color walls off",
        },
        CommandSpec {
            name: "units",
            usage: "units <m|cm|mm|ft|in|ftin>",
            summary: "Set the display unit for dimensions and readouts (geometry stays meters internally). Number suffixes work everywhere regardless: 500mm, 2.5m, 12ft, 6in, and feet-inches typed as 12ft6in (shown as 12'-6\"). Example: units ftin",
        },
        CommandSpec {
            name: "underlay",
            usage: "underlay <path.png> [corner x,y] [width]",
            summary: "Place a raster image (PNG) flat on the ground plane to trace over; corner is the lower-left in meters (default 0,0), width in meters (default 10, height follows the image aspect). Example: underlay /tmp/site.png 0,0 20",
        },
        CommandSpec {
            name: "underlayopacity",
            usage: "underlayopacity <0..1>",
            summary: "Set the underlay's blend opacity. Example: underlayopacity 0.4",
        },
        CommandSpec {
            name: "underlayoff",
            usage: "underlayoff",
            summary: "Remove the underlay image",
        },
        CommandSpec {
            name: "sun",
            usage: "sun <lat> <lon> <YYYY-MM-DD> <HH:MM>",
            summary: "Set solar lighting by computing azimuth + altitude (NOAA simplified SPA) for the given observer location and UTC time. The renderer shades meshes from that direction. Example: sun 40.71 -74.01 2024-06-21 14:00",
        },
        CommandSpec {
            name: "sunoff",
            usage: "sunoff",
            summary: "Remove solar lighting; revert to headlight shading",
        },
        CommandSpec {
            name: "sheet",
            usage: "sheet <name> [a4|a3|a2|a1|a0]",
            summary: "Create a named paper sheet, landscape (default a3). Example: sheet plan a1",
        },
        CommandSpec {
            name: "sheetview",
            usage: "sheetview <sheet> <top|front|right|persp> <scale>",
            summary: "Add a scaled ortho view to a sheet; scale is 1:100 or 100 (1m = 10mm at 1:100). Example: sheetview plan top 1:100",
        },
        CommandSpec {
            name: "print",
            usage: "print <sheet> <path.pdf>",
            summary: "Export a sheet as a vector PDF at its views' scales. Example: print plan /tmp/plan.pdf",
        },
        CommandSpec {
            name: "export",
            usage: "export <path.{dxf|stl|obj|gltf|glb|svg|csv|ifc}>",
            summary: "Export the whole document, format chosen by extension: DXF R12 (2D entities, meshes as feature edges), binary STL and glTF/GLB (triangle meshes only), OBJ (meshes plus curves as polylines), SVG, CSV, or IFC4 openBIM (mesh objects as IfcBuildingElementProxy for Revit/BlenderBIM). Example: export /tmp/model.ifc",
        },
        CommandSpec {
            name: "import",
            usage: "import <path.{dxf|obj|stl|gltf|glb|ifc}>",
            summary: "Import a file by extension: DXF (LINE, LWPOLYLINE, POLYLINE, CIRCLE, ARC, TEXT → logged ops), OBJ/STL/glTF/GLB meshes, or IFC4/IFC2x3 (IfcTriangulatedFaceSet + IfcFacetedBrep meshes → the 'ifc' layer); unknown entities are skipped. Example: import /tmp/model.ifc",
        },
        CommandSpec {
            name: "distance",
            usage: "distance <a x,y,z> <b x,y,z>",
            summary: "Measure the distance between two points (query only, nothing is created). Example: distance 0,0,0 3,4,0",
        },
        CommandSpec {
            name: "area",
            usage: "area <selector>",
            summary: "Measure area: closed curves report the enclosed XY area, meshes their total surface area (query only). Example: area last",
        },
        CommandSpec {
            name: "volume",
            usage: "volume <selector>",
            summary: "Measure the volume of solid meshes (query only). Example: volume last",
        },
        CommandSpec {
            name: "bbox",
            usage: "bbox <selector>",
            summary: "Report the combined bounding box (min, max, size) of objects (query only). Example: bbox all",
        },
        CommandSpec {
            name: "schedule",
            usage: "schedule [layer]",
            summary: "Print a schedule table (name/id/layer/type/area/volume) for all objects or a layer's objects, grouped by name (query only). Example: schedule · schedule walls",
        },
        CommandSpec {
            name: "sheettable",
            usage: "sheettable <sheet> [layer]",
            summary: "Place a schedule table on a sheet; the table is rendered as a text grid in the PDF at print time. Example: sheettable plan · sheettable plan walls",
        },
        CommandSpec {
            name: "sheetdim",
            usage: "sheetdim <sheet> <x1,y1> <x2,y2> [offset_mm]",
            summary: "Add a paper-space linear dimension to a sheet; a and b are paper coordinates in mm, offset is the perpendicular dim-line offset in mm (default 8). The numeric label is derived from the model distance via the view scale. Example: sheetdim plan 20,30 120,30 10",
        },
        CommandSpec {
            name: "view",
            usage: "view save <name> | view <name> | view list",
            summary: "Named views: save the active viewport camera, restore it later, or list saved views. Example: view save entry then view entry",
        },
        CommandSpec {
            name: "block",
            usage: "block <selector> <name>",
            summary: "Capture the geometry of selected objects as a named reusable block definition (like a symbol/component). Inputs stay in the scene; the definition is a geometry snapshot. Example: block last door · block last 3 tree (architect: door, window, tree, column, stair)",
        },
        CommandSpec {
            name: "insert",
            usage: "insert <name> <position x,y,z> [rotation_deg] [scale]",
            summary: "Place an instance of a named block at a point with optional rotation (degrees CCW about Z) and uniform scale. Instances move/scale/rotate as a single object. Example: insert door 3,0,0 · insert tree 10,5,0 0 0.8 · insert column 0,0,0 90 1",
        },
        CommandSpec {
            name: "blocks",
            usage: "blocks",
            summary: "List all block definitions with their geometry counts (query only). Example: blocks",
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
        CommandSpec {
            name: "amend",
            usage: "amend <step> <command...>",
            summary: "Rewrite history: replace the op at <step> (0 = first op) with a new command and replay the whole log; later ops rebuild against the change. On failure nothing changes. Example: amend 0 box 0,0,0 8,8,3",
        },
    ]
}

/// Selector grammar shared by help and the LLM prompt.
pub const SELECTOR_HELP: &str =
    "Selectors: 'last' (most recent object), 'last N' (N most recent), 'all', 'sel' (current selection), or an object name.";
