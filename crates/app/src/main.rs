// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

mod app;
mod app_verbs;
mod basemap;
mod boxsel;
mod chat_store;
mod command_line;
mod deck_pane;
mod download;
mod draw_tool;
mod gumball;
mod hardware;
mod headless;
mod icons;
mod journal;
mod keymap;
mod local_runtime;
mod menu;
mod model_catalog;
#[cfg(not(target_os = "linux"))]
mod native_menu;
mod osnap;
mod point_edit;
mod precise;
mod preset;
mod scene;
mod statusbar;
mod suggest;
mod tabstrip;
mod theme;
mod ui_plane;

fn cli_help_text() -> String {
    let mut out = String::new();
    out.push_str(
        "ItsJustCAD — It's just CAD\n\
         \n\
         Usage: itsjustcad [OPTIONS]\n\
         \n\
         Options:\n\
         \x20 --help, -h                  Show this help\n\
         \x20 --run <script.txt | ->      Execute commands from file or stdin (one per line, # comments)\n\
         \x20 --out <file>                Save document after running script\n\
         \x20 --shot <path.png>           Render offscreen screenshot after running script\n\
         \x20 --headless                  No window (required for --shot without a display)\n\
         \n\
         Scripts may also use app-level verbs: ze/zoomextents, the standard views\n\
         (top/front/persp/…), camera <lens>, display <mode>, lightmode <mode>,\n\
         profileedges [on|off], sketchup, save [path], help [verb].\n\
         These frame/style the --shot render. GUI-only verbs (template, critique) are\n\
         ignored with a warning.\n\
         \n\
         Exit codes:\n\
         \x20 0  success\n\
         \x20 1  command error (failing line + error printed to stderr)\n\
         \x20 2  file / IO error\n\
         \n\
         Env vars:\n\
         \x20 ITSJUSTCAD_RUN=<cmd;cmd>    Execute commands on startup (GUI mode)\n\
         \x20 ITSJUSTCAD_SHOT=<path.png>  Screenshot and exit (GUI mode)\n\
         \x20 ITSJUSTCAD_DECK_RUN=<text>  Send deck message on startup\n\
         \n\
         Commands:\n",
    );
    for spec in itsjustcad_commands::registry() {
        let first_sentence = spec.summary.split('.').next().unwrap_or(spec.summary).trim();
        out.push_str(&format!("  {:<20} {:<50} {}\n", spec.name, spec.usage, first_sentence));
    }
    out.push('\n');
    out.push_str(itsjustcad_commands::SELECTOR_HELP);
    out
}

/// Parsed CLI arguments for the headless / script path.
struct CliArgs {
    run_path: Option<String>, // "--run <path | ->"
    out_path: Option<String>, // "--out <file>"
    shot_path: Option<String>, // "--shot <path.png>"
    headless: bool,
}

fn parse_cli_args() -> CliArgs {
    let args: Vec<String> = std::env::args().collect();
    let mut run_path = None;
    let mut out_path = None;
    let mut shot_path = None;
    let mut headless = false;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--run" => {
                i += 1;
                run_path = args.get(i).cloned();
            }
            "--out" => {
                i += 1;
                out_path = args.get(i).cloned();
            }
            "--shot" => {
                i += 1;
                shot_path = args.get(i).cloned();
            }
            "--headless" => headless = true,
            // --help / -h handled before parse_cli_args is called
            _ => {}
        }
        i += 1;
    }
    CliArgs { run_path, out_path, shot_path, headless }
}

/// Entry point for `--run` (headless or not). Returns an OS exit code.
fn run_headless_mode(args: &CliArgs) -> i32 {
    // Read the script source.
    let src = match args.run_path.as_deref() {
        Some("-") => {
            use std::io::Read as _;
            let mut s = String::new();
            if std::io::stdin().read_to_string(&mut s).is_err() {
                eprintln!("error: could not read stdin");
                return 2;
            }
            s
        }
        Some(path) => match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("error: could not read '{}': {e}", path);
                return 2;
            }
        },
        None => String::new(),
    };

    let lines = headless::parse_script(&src);
    let session = itsjustcad_commands::Session::default();
    let (session, view) = match headless::run_script_lines(session, &lines) {
        Ok(s) => s,
        Err((line, msg)) => {
            eprintln!("command error: {line}\n  {msg}");
            return 1;
        }
    };

    // Optional: save document.
    if let Some(out) = &args.out_path
        && let Err(e) = itsjustcad_commands::io::save_file(&session, std::path::Path::new(out))
    {
        eprintln!("error: could not save '{}': {e}", out);
        return 2;
    }

    // Optional: render headless screenshot.
    if let Some(shot) = &args.shot_path {
        if let Err(e) = headless::render_headless(&session, std::path::Path::new(shot), &view) {
            eprintln!("error: render failed: {e}");
            return 2;
        }
        println!("wrote {shot}");
    }

    0
}

fn main() -> eframe::Result<()> {
    if std::env::args().any(|a| a == "--help" || a == "-h") {
        print!("{}", cli_help_text());
        std::process::exit(0);
    }

    let cli = parse_cli_args();

    // Headless path: --headless flag, or --run/--shot with no GUI needed.
    if cli.headless || (cli.run_path.is_some() && cli.shot_path.is_some() && !cli.headless) {
        // Only go headless when --headless is explicitly set, or when both
        // --run and --shot are given together (the canonical "batch render" use).
        // GUI path can also honour --run after startup through env vars.
        if cli.headless {
            tracing_subscriber::fmt::init();
            itsjustcad_commands::blocklib::seed_if_empty();
            let code = run_headless_mode(&cli);
            std::process::exit(code);
        }
    }

    // Explicit --run without --headless: run with a window but execute script
    // first (honours --out / --shot via headless path if --headless is set).
    if cli.run_path.is_some() && !cli.headless {
        tracing_subscriber::fmt::init();
        let code = run_headless_mode(&cli);
        std::process::exit(code);
    }

    tracing_subscriber::fmt::init();

    // Tokio runtime on a background thread; the UI keeps a Handle for
    // spawning deck streams.
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let handle = runtime.handle().clone();
    std::thread::spawn(move || {
        runtime.block_on(std::future::pending::<()>());
    });

    // ITSJUSTCAD_WINDOW_SIZE=WxH lets screenshots test at different resolutions.
    let window_size: [f32; 2] = std::env::var("ITSJUSTCAD_WINDOW_SIZE")
        .ok()
        .and_then(|s| {
            let mut parts = s.split('x');
            let w = parts.next()?.parse::<f32>().ok()?;
            let h = parts.next()?.parse::<f32>().ok()?;
            Some([w, h])
        })
        .unwrap_or([1440.0, 900.0]);

    // Embed the 256×256 icon PNG at compile time so the window icon is always
    // available without any runtime file-system access.
    let icon: Option<std::sync::Arc<egui::IconData>> = {
        const ICON_PNG: &[u8] = include_bytes!("../../../assets/icon/256.png");
        eframe::icon_data::from_png_bytes(ICON_PNG).ok().map(std::sync::Arc::new)
    };

    let mut viewport = egui::ViewportBuilder::default()
        .with_title("ItsJustCAD")
        .with_inner_size(window_size);
    if let Some(icon) = icon {
        viewport = viewport.with_icon(icon);
    }

    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Wgpu,
        depth_buffer: 24,
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "ItsJustCAD",
        options,
        Box::new(move |cc| Ok(Box::new(app::App::new(cc, handle)))),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_help_contains_every_verb() {
        let text = cli_help_text();
        for spec in itsjustcad_commands::registry() {
            assert!(text.contains(spec.name), "CLI help missing '{}'", spec.name);
        }
    }

    #[test]
    fn cli_help_contains_run_flag() {
        let text = cli_help_text();
        assert!(text.contains("--run"));
        assert!(text.contains("--headless"));
        assert!(text.contains("--out"));
        assert!(text.contains("--shot"));
    }
}
