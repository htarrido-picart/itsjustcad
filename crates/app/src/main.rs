mod app;
mod boxsel;
mod command_line;
mod deck_pane;
mod draw_tool;
mod gumball;
mod journal;
mod keymap;
mod osnap;
mod precise;
mod scene;
mod statusbar;

fn cli_help_text() -> String {
    let mut out = String::new();
    out.push_str("Usage: mydrafter [OPTIONS]\n\nOptions:\n  --help, -h    Show this help\n\nEnv vars:\n  MYDRAFTER_RUN=<cmd;cmd>    Execute commands on startup\n  MYDRAFTER_SHOT=<path.png>  Screenshot and exit\n  MYDRAFTER_DECK_RUN=<text>  Send deck message on startup\n\nCommands:\n");
    for spec in mydrafter_commands::registry() {
        let first_sentence = spec.summary.split('.').next().unwrap_or(spec.summary).trim();
        out.push_str(&format!("  {:<20} {:<50} {}\n", spec.name, spec.usage, first_sentence));
    }
    out.push('\n');
    out.push_str(mydrafter_commands::SELECTOR_HELP);
    out
}

fn main() -> eframe::Result<()> {
    if std::env::args().any(|a| a == "--help" || a == "-h") {
        print!("{}", cli_help_text());
        std::process::exit(0);
    }

    tracing_subscriber::fmt::init();

    // Tokio runtime on a background thread; the UI keeps a Handle for
    // spawning deck streams.
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let handle = runtime.handle().clone();
    std::thread::spawn(move || {
        runtime.block_on(std::future::pending::<()>());
    });

    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Wgpu,
        depth_buffer: 24,
        viewport: egui::ViewportBuilder::default()
            .with_title("mydrafter")
            .with_inner_size([1440.0, 900.0]),
        ..Default::default()
    };

    eframe::run_native(
        "mydrafter",
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
        for spec in mydrafter_commands::registry() {
            assert!(text.contains(spec.name), "CLI help missing '{}'", spec.name);
        }
    }
}
