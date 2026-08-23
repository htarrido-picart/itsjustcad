mod app;
mod command_line;
mod deck_pane;
mod draw_tool;
mod gumball;
mod journal;
mod keymap;
mod osnap;
mod scene;

fn main() -> eframe::Result<()> {
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
