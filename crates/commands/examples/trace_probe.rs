// OBJECTIVE trace evaluator. Treats the image's colored ink (a fixed hue
// reference detector) as ground truth, rasterizes the traced polylines back to
// pixels, and scores precision / recall / F1 at a small pixel tolerance, plus
// fragmentation. Sweeps the pipeline parameters and prints a table sorted by F1,
// then dumps the best overlay. Pick parameters by the numbers, not by eye.
//
// Usage: cargo run -p itsjustcad-commands --example trace_probe -- <img> <outdir> [color]
//   color = red | blue | black | r,g,b   (default: red)

use image::{Rgba, RgbaImage};
use itsjustcad_commands::trace::{TraceParams, trace_with};

const TOL_PX: i32 = 2; // match tolerance: a trace within this many px of ink counts

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("image path");
    let outdir = args.next().unwrap_or_else(|| "/tmp/trace_eval".into());
    let color = args.next().unwrap_or_else(|| "red".into());
    std::fs::create_dir_all(&outdir).unwrap();

    let img = image::open(&path).expect("open image").to_rgba8();
    let (w, h) = img.dimensions();
    let (wu, hu) = (w as usize, h as usize);
    let target = parse_color(&color);
    println!("image {w}x{h}  color={color} target={target:?}  tol={TOL_PX}px");

    // Ground truth: reference ink mask (fixed, generous hue detector) + its
    // tolerance dilation, computed once.
    let gt = reference_ink(&img, target);
    let gt_count = gt.iter().filter(|&&b| b).count();
    let gt_d = dilate(&gt, wu, hu, TOL_PX);
    println!("ground-truth ink: {gt_count}px");

    // Parameter sweep. First sweep found dil=0 always best; now push the
    // continuity levers (bridge gap + stitch angle) around that winner.
    let tols = [80u8];
    let dilations = [0i32];
    let gaps = [16.0f64, 24.0, 32.0, 48.0, 64.0];
    let angles = [30.0f64, 45.0, 60.0];
    let min_lens = [12.0f64, 15.0, 20.0];

    struct Row {
        p: TraceParams,
        prec: f64,
        rec: f64,
        f1: f64,
        npoly: usize,
        tlen: f64,
    }
    let mut rows: Vec<Row> = Vec::new();
    for &tol in &tols {
        for &dr in &dilations {
            for &gap in &gaps {
              for &ang in &angles {
                for &ml in &min_lens {
                    let p = TraceParams {
                        tol,
                        dilate_radius: dr,
                        bridge_gap: gap,
                        bridge_angle_deg: ang,
                        min_len: ml,
                        epsilon: 1.5,
                    };
                    let polys = trace_with(img.as_raw(), w, h, target, &p);
                    let t = rasterize(&polys, wu, hu);
                    let t_count = t.iter().filter(|&&b| b).count();
                    if t_count == 0 {
                        continue;
                    }
                    let t_d = dilate(&t, wu, hu, TOL_PX);
                    // precision: traced pixels that land within tol of ink.
                    let tp_p = (0..wu * hu).filter(|&i| t[i] && gt_d[i]).count();
                    let prec = tp_p as f64 / t_count as f64;
                    // recall: ink pixels covered by a trace within tol.
                    let tp_r = (0..wu * hu).filter(|&i| gt[i] && t_d[i]).count();
                    let rec = tp_r as f64 / gt_count.max(1) as f64;
                    let f1 = if prec + rec > 0.0 { 2.0 * prec * rec / (prec + rec) } else { 0.0 };
                    let tlen: f64 = polys.iter().map(|pl| plen(pl)).sum();
                    rows.push(Row { p, prec, rec, f1, npoly: polys.len(), tlen });
                }
              }
            }
        }
    }

    rows.sort_by(|a, b| b.f1.partial_cmp(&a.f1).unwrap());
    println!("\n  F1    prec  recall | gap ang mlen | npoly  tlen");
    println!("  ----  ----  ------ | --- --- ---- | -----  ------");
    for r in rows.iter().take(14) {
        println!(
            "  {:.3} {:.3} {:.3}  | {:3.0} {:3.0} {:4.0} | {:5}  {:6.0}",
            r.f1, r.prec, r.rec, r.p.bridge_gap, r.p.bridge_angle_deg, r.p.min_len, r.npoly, r.tlen
        );
    }

    // Dump the best config's overlay on the faded original.
    if let Some(best) = rows.first() {
        let polys = trace_with(img.as_raw(), w, h, target, &best.p);
        save_overlay_on(&img, &polys, &format!("{outdir}/best_{color}.png"));
        println!(
            "\nBEST: F1={:.3} prec={:.3} rec={:.3}  tol={} dil={} gap={} mlen={}  → {outdir}/best_{color}.png",
            best.f1, best.prec, best.rec, best.p.tol, best.p.dilate_radius, best.p.bridge_gap, best.p.min_len
        );
    }
}

fn parse_color(s: &str) -> [u8; 3] {
    if let Some((a, rest)) = s.split_once(',')
        && let Some((b, c)) = rest.split_once(',')
        && let (Ok(r), Ok(g), Ok(b)) = (a.trim().parse(), b.trim().parse(), c.trim().parse::<u8>())
    {
        return [r, g, b];
    }
    match s {
        "blue" => [40, 55, 200],
        "black" => [25, 25, 25],
        "green" => [0, 150, 70],
        _ => [230, 30, 38],
    }
}

/// HSV-ish hue/chroma/value.
fn hcv(r: u8, g: u8, b: u8) -> (f64, f64, f64) {
    let (r, g, b) = (r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0);
    let mx = r.max(g).max(b);
    let mn = r.min(g).min(b);
    let c = mx - mn;
    let hgt = if c < 1e-6 {
        -1.0
    } else if mx == r {
        60.0 * (((g - b) / c).rem_euclid(6.0))
    } else if mx == g {
        60.0 * ((b - r) / c + 2.0)
    } else {
        60.0 * ((r - g) / c + 4.0)
    };
    (hgt, c, mx)
}

fn hue_dist(a: f64, b: f64) -> f64 {
    let d = (a - b).abs().rem_euclid(360.0);
    d.min(360.0 - d)
}

/// Fixed, generous reference detector for "ink of the target color" — the
/// ground truth. Independent of the trace's own (swept) tolerance.
fn reference_ink(img: &RgbaImage, target: [u8; 3]) -> Vec<bool> {
    let (th, tc, tv) = hcv(target[0], target[1], target[2]);
    let chromatic = tc >= 0.10;
    img.pixels()
        .map(|px| {
            let [r, g, b, a] = px.0;
            if a < 8 {
                return false;
            }
            let (hh, c, v) = hcv(r, g, b);
            if chromatic {
                c >= 0.08 && hh >= 0.0 && hue_dist(hh, th) <= 45.0
            } else {
                c < 0.10 && (v - tv).abs() <= 0.35
            }
        })
        .collect()
}

/// Chebyshev dilation by `radius`.
fn dilate(mask: &[bool], w: usize, h: usize, radius: i32) -> Vec<bool> {
    let mut out = vec![false; w * h];
    for y in 0..h as i32 {
        for x in 0..w as i32 {
            if !mask[y as usize * w + x as usize] {
                continue;
            }
            for dy in -radius..=radius {
                for dx in -radius..=radius {
                    let (nx, ny) = (x + dx, y + dy);
                    if nx >= 0 && ny >= 0 && (nx as usize) < w && (ny as usize) < h {
                        out[ny as usize * w + nx as usize] = true;
                    }
                }
            }
        }
    }
    out
}

/// Rasterize polylines into a 1px boolean mask (Bresenham per segment).
fn rasterize(polys: &[Vec<(f64, f64)>], w: usize, h: usize) -> Vec<bool> {
    let mut m = vec![false; w * h];
    let mut put = |x: i32, y: i32| {
        if x >= 0 && y >= 0 && (x as usize) < w && (y as usize) < h {
            m[y as usize * w + x as usize] = true;
        }
    };
    for poly in polys {
        for s in poly.windows(2) {
            let (mut x0, mut y0) = (s[0].0.round() as i32, s[0].1.round() as i32);
            let (x1, y1) = (s[1].0.round() as i32, s[1].1.round() as i32);
            let dx = (x1 - x0).abs();
            let dy = -(y1 - y0).abs();
            let sx = if x0 < x1 { 1 } else { -1 };
            let sy = if y0 < y1 { 1 } else { -1 };
            let mut err = dx + dy;
            loop {
                put(x0, y0);
                if x0 == x1 && y0 == y1 {
                    break;
                }
                let e2 = 2 * err;
                if e2 >= dy {
                    err += dy;
                    x0 += sx;
                }
                if e2 <= dx {
                    err += dx;
                    y0 += sy;
                }
            }
        }
    }
    m
}

fn plen(p: &[(f64, f64)]) -> f64 {
    p.windows(2).map(|s| ((s[1].0 - s[0].0).powi(2) + (s[1].1 - s[0].1).powi(2)).sqrt()).sum()
}

fn save_overlay_on(img: &RgbaImage, polys: &[Vec<(f64, f64)>], out: &str) {
    let (w, h) = img.dimensions();
    let mut canvas = RgbaImage::new(w, h);
    for (x, y, p) in img.enumerate_pixels() {
        let [r, g, b, _] = p.0;
        let f = |c: u8| (c as u16 + (255 - c as u16) * 3 / 4) as u8;
        canvas.put_pixel(x, y, Rgba([f(r), f(g), f(b), 255]));
    }
    for (i, poly) in polys.iter().enumerate() {
        let c = hsv((i as f64 * 0.61803398875).fract() * 360.0, 0.95, 0.75);
        for seg in poly.windows(2) {
            draw_line(&mut canvas, seg[0], seg[1], c);
        }
    }
    canvas.save(out).unwrap();
}

fn hsv(h: f64, s: f64, v: f64) -> [u8; 3] {
    let c = v * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let m = v - c;
    let (r, g, b) = match (h / 60.0) as i32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    [((r + m) * 255.0) as u8, ((g + m) * 255.0) as u8, ((b + m) * 255.0) as u8]
}

fn draw_line(img: &mut RgbaImage, a: (f64, f64), b: (f64, f64), c: [u8; 3]) {
    let (mut x0, mut y0) = (a.0.round() as i32, a.1.round() as i32);
    let (x1, y1) = (b.0.round() as i32, b.1.round() as i32);
    let dx = (x1 - x0).abs();
    let dy = -(y1 - y0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    let (w, h) = img.dimensions();
    loop {
        if x0 >= 0 && y0 >= 0 && (x0 as u32) < w && (y0 as u32) < h {
            img.put_pixel(x0 as u32, y0 as u32, Rgba([c[0], c[1], c[2], 255]));
        }
        if x0 == x1 && y0 == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x0 += sx;
        }
        if e2 <= dx {
            err += dx;
            y0 += sy;
        }
    }
}
