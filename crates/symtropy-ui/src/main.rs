mod app;
mod filter;
mod gallery;
mod file_manager;
mod sidebar;
mod theme;
mod window;
mod x11;

use app::SymtropyApp;

fn main() -> eframe::Result<()> {
    // Force X11/XWayland so our _GTK_FRAME_EXTENTS tiling hack works. winit 0.29 picks native
    // Wayland whenever WAYLAND_DISPLAY is set (it no longer reads WINIT_UNIX_BACKEND), so hide it.
    // This must run before anything else touches the environment or spawns a thread.
    if std::env::var_os("DISPLAY").is_some() {
        std::env::remove_var("WAYLAND_DISPLAY");
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
        .with_title("Symtropy")
        .with_inner_size([1400.0, 900.0])
        .with_min_inner_size([800.0, 400.0])
        .with_decorations(false)
        .with_transparent(true),
        ..Default::default()
    };
    eframe::run_native(
        "Symtropy",
        options,
        Box::new(|cc| Box::new(SymtropyApp::new(cc))),
    )
}
