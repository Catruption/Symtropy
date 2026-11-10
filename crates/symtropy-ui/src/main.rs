mod app;
mod filter;
mod gallery;
mod theme;
mod window;
mod x11;

fn main() -> eframe::Result<()> {
    // Native Wayland never tells a client where its window is, which the tiling detection in
    // window/mod.rs needs. Running through XWayland/X11 does, so hide the Wayland display (this
    // runs before any other thread exists, so mutating the environment is fine).
    if std::env::var_os("DISPLAY").is_some() {
        std::env::remove_var("WAYLAND_DISPLAY");
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Symtropy")
            .with_inner_size([1440.0, 960.0])
            .with_min_inner_size([660.0, 500.0])
            // The steel frame IS the window chrome, and its texture margin needs
            // a transparent OS window so the decorations can overhang the plate.
            .with_decorations(false)
            .with_transparent(true),
        ..Default::default()
    };
    eframe::run_native("Symtropy", options, Box::new(|cc| Box::new(app::SymtropyApp::new(cc))))
}
