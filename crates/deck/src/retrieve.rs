// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Per-request retrieval over the command registry (#105). Ported from the iOS
//! on-device deck's `CommandRetriever`: a deterministic weighted token scorer
//! (IDF over each verb's name + usage + summary, boosted by name, prefix,
//! category and alias hits). No embeddings, no I/O; cheap enough to run on
//! every turn. The deck puts the top matches' exact usage + summary into that
//! turn's system prompt so the model writes catalog verbs with the right syntax.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use itsjustcad_commands::{Category, CommandSpec, registry};

/// Side-effecting verbs (filesystem / network) the FFI side-effect gate
/// rejects for deck lines. Never retrieved, never advertised to the FFI deck.
pub const GATED_VERBS: &[&str] = &[
    "export",
    "controlimages",
    "print",
    "blocksave",
    "import",
    "xref",
    "terrain",
    "osmfile",
    "plantschedule",
    "underlay",
    "trace",
    "hatchpat",
    "blockload",
    "workdir",
];

/// The everyday verbs whose usage the FFI prompt always carries (the "core
/// set", plus `geodesic` for spheres). They are never retrieved, and their
/// names never count as evidence for another verb ("copy the box" must not
/// pull every verb that mentions a box), mirroring the iOS typed-core
/// exclusion. `geodesic` stays retrievable: its own summary is worth showing
/// for "a Fuller dome".
pub const CORE_VERBS: &[&str] = &[
    "box", "circle", "rect", "polygon", "line", "polyline", "arc", "extrude", "move", "copy",
    "array", "rotate", "scale", "mirror", "delete", "name", "select",
];

const STOPWORDS: &[&str] = &[
    "a", "an", "the", "of", "with", "and", "to", "at", "in", "on", "by", "for", "from", "make",
    "draw", "create", "add", "me", "please", "it", "its", "style", "put", "build", "some", "one",
    "this", "that", "into", "over", "is", "be", "can", "you", "i", "want", "x", "y", "z", "m",
    "unit", "new", "my", "them", "all",
];

/// Phrases people use → the verbs they mean (architect names, everyday words).
/// Matched as contiguous stemmed token runs; each hit adds [`ALIAS_WEIGHT`].
const ALIASES: &[(&str, &[&str])] = &[
    // Expressive structures / form-finding
    ("dieste", &["gaussvault"]),
    ("eladio", &["gaussvault"]),
    ("brick vault", &["gaussvault"]),
    ("gaussian vault", &["gaussvault"]),
    ("undulating", &["gaussvault"]),
    ("wavy vault", &["gaussvault"]),
    ("candela", &["hypar"]),
    ("saddle", &["hypar"]),
    ("hyperbolic paraboloid", &["hypar"]),
    ("paraboloid", &["hypar"]),
    ("pringle", &["hypar"]),
    ("gaudi", &["funicular"]),
    ("hanging chain", &["funicular"]),
    ("hang a chain", &["funicular"]),
    ("chain", &["funicular"]),
    ("inverted arch", &["funicular"]),
    ("invert", &["funicular"]),
    (
        "form finding",
        &["funicular", "cablenet", "minsurf", "tensegrity"],
    ),
    ("catenary", &["catenaryvault", "funicular"]),
    ("barrel vault", &["catenaryvault"]),
    ("vault", &["catenaryvault", "gaussvault"]),
    ("schwedler", &["schwedler"]),
    ("ribbed dome", &["schwedler"]),
    ("fuller", &["geodesic"]),
    ("buckminster", &["geodesic"]),
    ("geodesic", &["geodesic"]),
    ("icosahedron", &["geodesic"]),
    ("dome", &["geodesic", "schwedler"]),
    ("frei otto", &["cablenet", "minsurf"]),
    ("otto", &["cablenet", "minsurf"]),
    ("cable net", &["cablenet"]),
    ("tensile", &["cablenet"]),
    ("membrane", &["cablenet", "minsurf"]),
    ("tent", &["cablenet"]),
    ("soap film", &["minsurf"]),
    ("minimal surface", &["minsurf"]),
    ("tensegrity", &["tensegrity"]),
    ("snelson", &["tensegrity"]),
    ("gridshell", &["gridshell"]),
    ("grid shell", &["gridshell"]),
    ("lattice", &["gridshell", "diagrid"]),
    ("lath", &["gridshell"]),
    ("timber shell", &["gridshell"]),
    ("diagrid", &["diagrid"]),
    ("diagonal grid", &["diagrid"]),
    ("space frame", &["spaceframe"]),
    ("truss", &["spaceframe"]),
    ("reciprocal", &["reciprocal"]),
    ("nexorade", &["reciprocal"]),
    ("waffle", &["waffle"]),
    ("egg crate", &["waffle"]),
    ("voronoi", &["voronoishell"]),
    ("cell pattern", &["voronoishell"]),
    // Building elements
    ("stair", &["stair", "spiral"]),
    ("staircase", &["stair", "spiral"]),
    ("steps", &["stair"]),
    ("spiral stair", &["spiral"]),
    ("spiral staircase", &["spiral"]),
    ("helical stair", &["spiral"]),
    ("ramp", &["ramp"]),
    ("accessible", &["ramp"]),
    ("beam", &["beam"]),
    ("slab", &["slab"]),
    ("floor plate", &["slab"]),
    ("wall", &["wall"]),
    // Solids from curves
    ("lathe", &["revolve"]),
    ("revolve", &["revolve"]),
    ("vase", &["revolve"]),
    ("solid of revolution", &["revolve"]),
    ("spin", &["revolve"]),
    ("loft", &["loft"]),
    ("skin", &["loft"]),
    ("sweep", &["sweep"]),
    ("pipe", &["pipe"]),
    // Curves / 2D
    ("ellipse", &["ellipse"]),
    ("oval", &["ellipse"]),
    ("helix", &["helix"]),
    ("spring", &["helix"]),
    ("coil", &["helix"]),
    ("offset", &["offset"]),
    ("parallel", &["offset"]),
    ("fillet", &["fillet"]),
    ("round the corner", &["fillet"]),
    ("chamfer", &["chamfer"]),
    ("bevel", &["chamfer"]),
    ("resample", &["rebuild"]),
    ("merge the outline", &["curvebool"]),
    ("outline", &["curvebool"]),
    // Transform / annotate / dimension / analyze / tools / view
    ("polar array", &["polararray"]),
    ("around the", &["polararray"]),
    ("circular array", &["polararray"]),
    ("to the origin", &["tozero"]),
    ("sits at the origin", &["tozero"]),
    ("color", &["color"]),
    ("colour", &["color"]),
    ("paint", &["color"]),
    ("layer", &["layer"]),
    ("hatch", &["hatch"]),
    ("label", &["text"]),
    ("dimension", &["dim"]),
    ("sheet", &["sheet"]),
    ("four view", &["fourviews"]),
    ("how far", &["distance"]),
    ("distance", &["distance"]),
    ("volume", &["volume"]),
    ("area", &["area"]),
    ("bounding box", &["bbox"]),
    ("sun", &["sun"]),
    ("shadow", &["shadowstudy"]),
    ("solar", &["sun", "radiation"]),
    ("tree", &["plant", "plantrow"]),
    ("row of tree", &["plantrow"]),
    ("plant", &["plant"]),
    ("forest", &["miyawaki"]),
    ("path", &["sitepath"]),
    ("perspective", &["camera"]),
    ("camera", &["camera"]),
    ("save the view", &["view"]),
    ("lineweight", &["showweights", "lineweight"]),
    ("line weight", &["showweights", "lineweight"]),
    ("name it", &["name"]),
    ("call it", &["name"]),
];

const ALIAS_WEIGHT: f64 = 12.0;

/// Phrases that mean a core verb (array/copy) despite catalog words: with no
/// alias hit, they retrieve nothing.
const SUPPRESSORS: &[&str] = &["copies of", "copy of", "grid of copies", "grid of"];

/// One retrieved verb and its score.
#[derive(Clone)]
pub struct Match {
    pub spec: &'static CommandSpec,
    pub score: f64,
}

struct Doc {
    spec: &'static CommandSpec,
    name_tokens: HashSet<String>,
    text_tokens: HashSet<String>,
    category_tokens: HashSet<String>,
}

struct Index {
    docs: Vec<Doc>,
    idf: HashMap<String, f64>,
    aliases: Vec<(Vec<String>, &'static [&'static str])>,
    suppressors: Vec<Vec<String>>,
    excluded: HashSet<String>,
}

fn category_word(c: Category) -> &'static str {
    match c {
        Category::File => "file",
        Category::Edit => "edit",
        Category::View => "view",
        Category::Draw2d => "draw 2d",
        Category::Curve => "curve",
        Category::Solid => "solid",
        Category::Boolean => "boolean",
        Category::Transform => "transform",
        Category::Annotate => "annotate",
        Category::Dimension => "dimension",
        Category::Analyze => "analyze",
        Category::Structure => "structure",
        Category::Tools => "tools",
    }
}

fn index() -> &'static Index {
    static INDEX: OnceLock<Index> = OnceLock::new();
    INDEX.get_or_init(|| {
        let excluded: HashSet<String> = CORE_VERBS.iter().map(|s| s.to_string()).collect();
        let docs: Vec<Doc> = registry()
            .iter()
            .filter(|s| !CORE_VERBS.contains(&s.name) && !GATED_VERBS.contains(&s.name))
            .map(|spec| Doc {
                spec,
                name_tokens: tokens(spec.name).into_iter().collect(),
                text_tokens: tokens(&format!("{} {}", spec.usage, spec.summary))
                    .into_iter()
                    .collect(),
                category_tokens: tokens(category_word(spec.category)).into_iter().collect(),
            })
            .collect();
        let mut df: HashMap<String, usize> = HashMap::new();
        for d in &docs {
            for t in d.text_tokens.union(&d.name_tokens) {
                *df.entry(t.clone()).or_default() += 1;
            }
        }
        let n = docs.len().max(1) as f64;
        let idf = df
            .into_iter()
            .map(|(t, c)| (t, ((n + 1.0) / (c as f64 + 0.5)).ln()))
            .collect();
        Index {
            docs,
            idf,
            aliases: ALIASES.iter().map(|(p, v)| (tokens(p), *v)).collect(),
            suppressors: SUPPRESSORS.iter().map(|p| tokens(p)).collect(),
            excluded,
        }
    })
}

/// Fold the accented Latin letters people type (Gaudí → gaudi).
fn fold(c: char) -> char {
    match c {
        'á' | 'à' | 'â' | 'ä' | 'ã' | 'å' => 'a',
        'é' | 'è' | 'ê' | 'ë' => 'e',
        'í' | 'ì' | 'î' | 'ï' => 'i',
        'ó' | 'ò' | 'ô' | 'ö' | 'õ' | 'ø' => 'o',
        'ú' | 'ù' | 'û' | 'ü' => 'u',
        'ñ' => 'n',
        'ç' => 'c',
        'ý' | 'ÿ' => 'y',
        c => c,
    }
}

/// Plural/gerund stripping only — enough for "vaults"/"stairs"/"hanging".
pub fn stem(w: &str) -> String {
    let n = w.chars().count();
    if n > 4 && w.ends_with("ies") {
        return format!("{}y", &w[..w.len() - 3]);
    }
    if n > 5 && w.ends_with("ing") {
        return w[..w.len() - 3].to_string();
    }
    if n > 4
        && ["xes", "ches", "shes", "zes"]
            .iter()
            .any(|s| w.ends_with(s))
    {
        return w[..w.len() - 2].to_string();
    }
    if n > 3 && w.ends_with('s') && !w.ends_with("ss") {
        return w[..w.len() - 1].to_string();
    }
    w.to_string()
}

/// Lowercased, accent-folded, light-stemmed word tokens.
pub fn tokens(text: &str) -> Vec<String> {
    let folded: String = text.to_lowercase().chars().map(fold).collect();
    folded
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(stem)
        .filter(|w| !w.is_empty())
        .collect()
}

fn contains_run(phrase: &[String], query: &[String]) -> bool {
    !phrase.is_empty()
        && phrase.len() <= query.len()
        && query.windows(phrase.len()).any(|w| w == phrase)
}

fn wants_gridshell(request: &str) -> bool {
    let lower = request.to_lowercase();
    lower.contains("gridshell")
        || lower
            .split(|c: char| !c.is_alphanumeric())
            .any(|w| ["shell", "shells", "lattice", "lath", "laths"].contains(&w))
}

/// The best `limit` registry verbs for a request, strongest first. Weak
/// matches are dropped so "draw a box" retrieves nothing and the prompt stays
/// small. Core and gated verbs are never returned.
pub fn retrieve(request: &str, limit: usize) -> Vec<Match> {
    const MIN_SCORE: f64 = 7.0;
    const RELATIVE_CUTOFF: f64 = 0.3;
    let ix = index();
    let query = tokens(request);
    if query.is_empty() {
        return Vec::new();
    }
    let unique: HashSet<&String> = query
        .iter()
        .filter(|t| !STOPWORDS.contains(&t.as_str()))
        .collect();

    let mut alias_boost: HashMap<&str, f64> = HashMap::new();
    for (phrase, verbs) in &ix.aliases {
        if contains_run(phrase, &query) {
            for v in *verbs {
                *alias_boost.entry(v).or_default() += ALIAS_WEIGHT;
            }
        }
    }
    if alias_boost.is_empty() && ix.suppressors.iter().any(|s| contains_run(s, &query)) {
        return Vec::new();
    }
    let gridshell = wants_gridshell(request);

    let mut scored: Vec<Match> = Vec::new();
    for d in &ix.docs {
        let name = d.spec.name;
        if name == "gridshell" && !gridshell {
            continue;
        }
        let mut s = alias_boost.get(name).copied().unwrap_or(0.0);
        let mut anchored = s > 0.0;
        let mut text = 0.0;
        for &t in &unique {
            let core_word = ix.excluded.contains(t);
            if d.name_tokens.contains(t) {
                s += 10.0;
                anchored = true;
            } else if t.chars().count() >= 4
                && !core_word
                && (name.starts_with(t.as_str()) || t.starts_with(name))
            {
                s += 4.0;
                anchored = true;
            }
            if d.text_tokens.contains(t) && !core_word {
                text += ix.idf.get(t).copied().unwrap_or(0.0).min(4.0);
            }
            if d.category_tokens.contains(t) {
                s += 1.0;
            }
        }
        s += text;
        if s >= MIN_SCORE && (anchored || text >= 12.0) {
            scored.push(Match {
                spec: d.spec,
                score: s,
            });
        }
    }
    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.spec.name.cmp(b.spec.name))
    });
    let floor = scored.first().map_or(0.0, |m| m.score) * RELATIVE_CUTOFF;
    scored
        .into_iter()
        .take_while(|m| m.score >= floor)
        .take(limit)
        .collect()
}

/// Rough token estimate (chars × 2 / 5, as the iOS prompt fitter uses).
pub fn estimate_tokens(text: &str) -> usize {
    (text.chars().count() * 2).div_ceil(5)
}

/// The per-request "verbs for this request" block: each retrieved verb's exact
/// usage + summary, strongest first, kept within `token_budget` (a verb whose
/// full entry doesn't fit falls back to its usage line, then is dropped).
/// Empty when nothing relevant is retrieved.
pub fn retrieved_block(request: &str, limit: usize, token_budget: usize) -> String {
    let matches = retrieve(request, limit);
    if matches.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "## Verbs for THIS request (exact syntax — copy the argument order exactly)\n",
    );
    let mut used = estimate_tokens(&out);
    for m in matches {
        let full = format!("  {}\n      {}\n", m.spec.usage, m.spec.summary);
        let short = format!("  {}\n", m.spec.usage);
        for entry in [full, short] {
            let cost = estimate_tokens(&entry);
            if used + cost <= token_budget {
                out.push_str(&entry);
                used += cost;
                break;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn top(request: &str) -> Vec<&'static str> {
        retrieve(request, 6).iter().map(|m| m.spec.name).collect()
    }

    #[test]
    fn architect_names_retrieve_their_structures() {
        assert_eq!(top("Dieste vault").first(), Some(&"gaussvault"));
        assert_eq!(
            top("a Dieste-style undulating brick vault 6 wide 12 long").first(),
            Some(&"gaussvault")
        );
        assert_eq!(top("Candela saddle").first(), Some(&"hypar"));
        assert_eq!(top("Candela saddle shell").first(), Some(&"hypar"));
        assert_eq!(top("Schwedler dome radius 8").first(), Some(&"schwedler"));
        assert_eq!(
            top("a geodesic dome by Buckminster Fuller, radius 6").first(),
            Some(&"geodesic")
        );
        assert_eq!(
            top("draw a geodesic dome with radius 8").first(),
            Some(&"geodesic")
        );
        assert!(top("Frei Otto cable net over corners").contains(&"cablenet"));
        assert_eq!(top("Gaudí hanging chain").first(), Some(&"funicular"));
    }

    #[test]
    fn everyday_words_retrieve_catalog_verbs() {
        assert_eq!(
            top("revolve the profile into a vase").first(),
            Some(&"revolve")
        );
        assert_eq!(
            top("make a polar array of 6 copies of it around its center").first(),
            Some(&"polararray")
        );
        assert_eq!(
            top("rebuild the curve with 20 points").first(),
            Some(&"rebuild")
        );
        assert_eq!(
            top("resample the polyline to 12 points").first(),
            Some(&"rebuild")
        );
        assert_eq!(
            top("hatch the circle with concrete").first(),
            Some(&"hatch")
        );
        assert_eq!(
            top("a staircase rising 3 m, 1.2 m wide").first(),
            Some(&"stair")
        );
        assert_eq!(
            top("a catenary vault 8 span 12 long 4 rise").first(),
            Some(&"catenaryvault")
        );
        assert_eq!(
            top("voronoi pattern of 24 cells over 20 by 20").first(),
            Some(&"voronoishell")
        );
        assert_eq!(top("what's the volume of the box").first(), Some(&"volume"));
        assert!(top("a timber gridshell on a hypar 5 by 5").contains(&"gridshell"));
    }

    #[test]
    fn core_requests_retrieve_little_or_nothing() {
        assert!(
            top("draw a box 4 by 4 by 3").is_empty(),
            "{:?}",
            top("draw a box 4 by 4 by 3")
        );
        assert!(top("make a 3 by 3 grid of copies of the box spaced 15 apart").is_empty());
        assert!(top("move it up 5").is_empty(), "{:?}", top("move it up 5"));
    }

    #[test]
    fn gridshell_needs_an_explicit_shell_word() {
        assert!(!top("a hypar 5 by 5").contains(&"gridshell"));
        assert!(top("a lattice shell").contains(&"gridshell"));
    }

    #[test]
    fn gated_and_core_verbs_are_never_retrieved() {
        for req in [
            "export the drawing to a file",
            "import a dxf",
            "underlay the plan image",
            "box",
            "copy it",
        ] {
            for name in top(req) {
                assert!(!GATED_VERBS.contains(&name), "{req}: gated {name}");
                assert!(!CORE_VERBS.contains(&name), "{req}: core {name}");
            }
        }
    }

    #[test]
    fn retrieval_is_deterministic() {
        let a = top("Candela saddle shell over a square");
        for _ in 0..5 {
            assert_eq!(a, top("Candela saddle shell over a square"));
        }
    }

    #[test]
    fn tokens_fold_accents_and_stem() {
        assert_eq!(
            tokens("Gaudí's Vaults, hanging stairs"),
            vec!["gaudi", "s", "vault", "hang", "stair"]
        );
    }

    #[test]
    fn retrieved_block_carries_usage_within_budget() {
        let b = retrieved_block("Dieste vault", 6, 600);
        assert!(
            b.contains("gaussvault <span> <length> <rise> [undulate]"),
            "{b}"
        );
        assert!(estimate_tokens(&b) <= 600);
        let tiny = retrieved_block("Dieste vault", 6, 40);
        assert!(estimate_tokens(&tiny) <= 40);
        assert!(retrieved_block("draw a box", 6, 600).is_empty());
    }

    #[test]
    fn core_and_gated_verbs_exist_in_registry() {
        for v in CORE_VERBS.iter().chain(GATED_VERBS) {
            assert!(
                registry().iter().any(|s| s.name == *v),
                "{v} not in registry"
            );
        }
    }
}
