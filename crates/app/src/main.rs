// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

mod api_keys;
mod app;
mod app_verbs;
mod basemap;
mod boxsel;
mod chat_crypto;
mod chat_db;
mod chat_store;
mod command_line;
mod deck_pane;
mod diffusion;
mod doc_paths;
mod download;
mod feedback_sheet;
mod draw_tool;
mod dyntabs;
mod filewatch;
mod gumball;
mod hardware;
mod headless;
mod history_rebuild;
mod i18n;
mod icons;
mod journal;
mod keep_awake;
mod keybindings;
mod keybindings_editor;
mod keymap;
mod local_runtime;
mod menu;
mod model_catalog;
#[cfg(not(target_os = "linux"))]
mod native_menu;
use itsjustcad_commands::osnap;
mod palette;
mod param_editor;
mod point_edit;
mod precise;
mod preset;
mod raytrace_ui;
mod scene;
mod sd_catalog;
mod smarttrack;
mod statusbar;
mod suggest;
mod syntax_layout;
mod tabstrip;
mod text_context_menu;
mod theme;
mod ui_plane;
mod update;
mod usage_spec;
mod widgets;

/// The user's config dir: [`home_config_dir`] of the home directory.
#[cfg(not(test))]
pub(crate) fn config_dir() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| home_config_dir(&h))
}

/// Tests never read or write the developer's real config: they get a throwaway
/// dir under the system temp dir instead, one per test process.
#[cfg(test)]
pub(crate) fn config_dir() -> Option<std::path::PathBuf> {
    Some(std::env::temp_dir().join(format!("ijc_test_config_{}", std::process::id())))
}

/// `~/.config/itsjustcad` under `home`, on every platform (CLI-tool convention,
/// XDG default on Linux; never a macOS-only dir — GitHub issue #26).
pub(crate) fn home_config_dir(home: &std::path::Path) -> std::path::PathBuf {
    home.join(".config").join("itsjustcad")
}

fn cli_help_text() -> String {
    let mut out = String::new();
    out.push_str(
        "ItsJustCAD — It's just CAD\n\
         \n\
         Usage: itsjustcad [OPTIONS]\n\
         \n\
         Options:\n\
         \x20 --help, -h                  Show this help\n\
         \x20 --open <file.itsjustcad.json>  Load + replay a saved document op-log (combine with --shot to render it)\n\
         \x20 --run <script.txt | ->      Execute commands from file or stdin (one per line, # comments)\n\
         \x20 --out <file>                Save document after running script\n\
         \x20 --shot <path.png>           Render offscreen screenshot after running script\n\
         \x20 --headless                  No window (required for --shot without a display)\n\
         \n\
         Scripts may also use app-level verbs: ze/zoomextents, the standard views\n\
         (top/front/persp/…), camera <lens>, display <mode>, lightmode <mode>,\n\
         profileedges [on|off], massing, save [path], help [verb].\n\
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
    open_path: Option<String>, // "--open <file.itsjustcad.json>"
    run_path: Option<String>, // "--run <path | ->"
    out_path: Option<String>, // "--out <file>"
    shot_path: Option<String>, // "--shot <path.png>"
    headless: bool,
}

fn parse_cli_args() -> CliArgs {
    parse_cli_args_from(&std::env::args().collect::<Vec<_>>())
}

/// Pure parse of an argv-style slice (first element is the program name).
/// Factored out of [`parse_cli_args`] so the flag handling is unit-testable.
fn parse_cli_args_from(args: &[String]) -> CliArgs {
    let mut open_path = None;
    let mut run_path = None;
    let mut out_path = None;
    let mut shot_path = None;
    let mut headless = false;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--open" => {
                i += 1;
                open_path = args.get(i).cloned();
            }
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
    CliArgs { open_path, run_path, out_path, shot_path, headless }
}

/// Seed a headless session from an optional `--open <file.itsjustcad.json>`.
///
/// `None` yields an empty [`Session`]. `Some(path)` replays the saved op-log via
/// [`itsjustcad_commands::io::load_file`] (checkpoint fast-path or full replay).
/// On a missing / unreadable / malformed / wrong-version file it prints a clear
/// stderr message and returns `Err(2)` so the caller exits with the IO code.
fn load_open_session(open_path: Option<&str>) -> Result<itsjustcad_commands::Session, i32> {
    match open_path {
        None => Ok(itsjustcad_commands::Session::default()),
        Some(path) => itsjustcad_commands::io::load_file(std::path::Path::new(path))
            .map_err(|e| {
                eprintln!("error: could not open '{}': {e}", path);
                2
            }),
    }
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

    // Seed the session: either a saved op-log (`--open`, replayed via
    // `io::load_file`) or an empty document. Any `--run` script then executes on
    // top of it (e.g. framing / style app-verbs over a loaded massing).
    let session = match load_open_session(args.open_path.as_deref()) {
        Ok(s) => s,
        Err(code) => return code,
    };

    let lines = headless::parse_script(&src);
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

/// Format one crash record: ISO-ish timestamp, panic message, source location,
/// and a captured backtrace. The message and location are compiled in even in a
/// stripped release binary, so this pinpoints panics that Finder-launched apps
/// would otherwise swallow (no stderr, SIGABRT with no note).
fn format_crash_record(
    timestamp: &str,
    message: &str,
    location: Option<&std::panic::Location<'_>>,
    backtrace: &std::backtrace::Backtrace,
) -> String {
    let loc = location
        .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
        .unwrap_or_else(|| "<unknown location>".to_string());
    format!(
        "===== PANIC {timestamp} =====\n\
         message : {message}\n\
         location: {loc}\n\
         backtrace:\n{backtrace}\n\n"
    )
}

/// Path to the crash log: `~/.config/itsjustcad/crash.log` (matches the rest of
/// the app's config convention).
fn crash_log_path() -> Option<std::path::PathBuf> {
    Some(config_dir()?.join("crash.log"))
}

/// A coarse UTC timestamp (seconds since the Unix epoch) with no extra deps —
/// enough to order crashes in the log.
fn crash_timestamp() -> String {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => format!("t+{}s (unix)", d.as_secs()),
        Err(_) => "t=?".to_string(),
    }
}

/// Append one crash record to `path`, creating parent dirs as needed.
/// Best-effort: IO errors are ignored (the caller is already panicking).
fn append_crash_record(path: &std::path::Path, record: &str) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    use std::io::Write as _;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = f.write_all(record.as_bytes());
    }
    // Restrict permissions to owner-only (0600) so doc paths / hostile imported
    // strings in crash records are not world-readable on multi-user systems.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
}

/// Install a panic hook that appends a full crash record (message + location +
/// backtrace + timestamp) to `~/.config/itsjustcad/crash.log`, and still prints
/// to stderr for terminal launches. Appends so multiple crashes accumulate.
fn install_crash_hook() {
    std::panic::set_hook(Box::new(|info| {
        // Extract the panic payload message.
        let message = if let Some(s) = info.payload().downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "<non-string panic payload>".to_string()
        };
        let backtrace = std::backtrace::Backtrace::force_capture();
        let record =
            format_crash_record(&crash_timestamp(), &message, info.location(), &backtrace);

        // Always surface to stderr (terminal launches).
        eprint!("{record}");

        // Append to the crash log (best-effort; ignore IO errors — we're already
        // panicking).
        if let Some(path) = crash_log_path() {
            append_crash_record(&path, &record);
        }
    }));
}

fn main() -> eframe::Result<()> {
    install_crash_hook();

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

    // Explicit --run or --open without --headless: run the batch path (load the
    // doc / execute the script, then honour --out / --shot).
    if (cli.run_path.is_some() || cli.open_path.is_some()) && !cli.headless {
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
    fn crash_record_includes_message_and_location() {
        // A real Location can't be constructed by hand, so exercise the format
        // via #[track_caller]: capture the caller location here.
        #[track_caller]
        fn record() -> (String, &'static std::panic::Location<'static>) {
            let loc = std::panic::Location::caller();
            let bt = std::backtrace::Backtrace::disabled();
            (format_crash_record("t+42s (unix)", "boom: it broke", Some(loc), &bt), loc)
        }
        let (rec, loc) = record();
        assert!(rec.contains("PANIC t+42s (unix)"), "{rec}");
        assert!(rec.contains("message : boom: it broke"), "{rec}");
        assert!(rec.contains(&format!("location: {}:{}", loc.file(), loc.line())), "{rec}");
        assert!(rec.contains("backtrace:"), "{rec}");
    }

    #[test]
    fn crash_record_handles_unknown_location() {
        let bt = std::backtrace::Backtrace::disabled();
        let rec = format_crash_record("t=?", "msg", None, &bt);
        assert!(rec.contains("location: <unknown location>"), "{rec}");
        assert!(rec.contains("message : msg"), "{rec}");
    }

    #[test]
    fn append_crash_record_accumulates() {
        let dir = std::env::temp_dir().join(format!("ijc_crash_test_{}", std::process::id()));
        let path = dir.join("crash.log");
        let _ = std::fs::remove_file(&path);
        append_crash_record(&path, "first\n");
        append_crash_record(&path, "second\n");
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("first"));
        assert!(contents.contains("second"));
        // Append, not overwrite.
        assert!(contents.find("first").unwrap() < contents.find("second").unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cli_help_contains_run_flag() {
        let text = cli_help_text();
        assert!(text.contains("--open"));
        assert!(text.contains("--run"));
        assert!(text.contains("--headless"));
        assert!(text.contains("--out"));
        assert!(text.contains("--shot"));
    }

    fn argv(parts: &[&str]) -> Vec<String> {
        std::iter::once("itsjustcad")
            .chain(parts.iter().copied())
            .map(String::from)
            .collect()
    }

    #[test]
    fn parse_open_flag_populates_path() {
        let cli = parse_cli_args_from(&argv(&["--open", "plan.itsjustcad.json"]));
        assert_eq!(cli.open_path.as_deref(), Some("plan.itsjustcad.json"));
        assert!(cli.run_path.is_none());
    }

    #[test]
    fn parse_open_with_headless_and_shot() {
        let cli = parse_cli_args_from(&argv(&[
            "--headless",
            "--open",
            "doc.itsjustcad.json",
            "--shot",
            "doc.png",
        ]));
        assert_eq!(cli.open_path.as_deref(), Some("doc.itsjustcad.json"));
        assert_eq!(cli.shot_path.as_deref(), Some("doc.png"));
        assert!(cli.headless);
    }

    #[test]
    fn load_open_none_is_empty_session() {
        let s = load_open_session(None).expect("empty session");
        assert_eq!(s.doc.objects().count(), 0);
    }

    #[test]
    fn load_open_missing_file_maps_to_exit_2() {
        let missing = std::env::temp_dir().join("ijc_no_such_doc.itsjustcad.json");
        let _ = std::fs::remove_file(&missing);
        match load_open_session(Some(missing.to_str().unwrap())) {
            Ok(_) => panic!("missing file should not load"),
            Err(code) => assert_eq!(code, 2, "missing --open file must exit 2"),
        }
    }

    #[test]
    fn load_open_invalid_file_maps_to_exit_2() {
        let dir = std::env::temp_dir().join(format!("ijc_open_bad_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bad = dir.join("garbage.itsjustcad.json");
        std::fs::write(&bad, b"not valid json {{{").unwrap();
        match load_open_session(Some(bad.to_str().unwrap())) {
            Ok(_) => panic!("malformed file should not load"),
            Err(code) => assert_eq!(code, 2),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_round_trip_restores_object_count() {
        // Save a tiny doc (one box) via the same io path the GUI uses, then
        // reload it through `load_open_session` and assert the geometry replayed.
        let dir = std::env::temp_dir().join(format!("ijc_open_rt_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("one_box.itsjustcad.json");

        let lines = vec!["box 0,0,0 1,1,1".to_owned()];
        let (session, _view) =
            headless::run_script_lines(itsjustcad_commands::Session::default(), &lines)
                .expect("build doc");
        let before = session.doc.objects().count();
        assert_eq!(before, 1, "one box");
        itsjustcad_commands::io::save_file(&session, &path).expect("save");

        let reloaded = load_open_session(Some(path.to_str().unwrap())).expect("reload");
        assert_eq!(reloaded.doc.objects().count(), before, "op-log replayed on open");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// On Unix, crash.log must be created with 0600 permissions.
    #[test]
    #[cfg(unix)]
    fn crash_log_is_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = std::env::temp_dir()
            .join(format!("ijc_crash_perm_test_{}", std::process::id()));
        let path = dir.join("crash.log");
        let _ = std::fs::remove_file(&path);
        append_crash_record(&path, "test\n");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        // mode & 0o777 gives the rwx bits; must be exactly 0600 (owner rw, no others).
        assert_eq!(mode & 0o777, 0o600, "crash.log should be 0600, got {mode:#o}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
