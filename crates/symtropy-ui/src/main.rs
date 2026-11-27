mod actions;
mod app;
mod wayland_drag;
mod filter;
mod gallery;
mod file_manager;
mod local_vision;
mod metadata_export;
mod paths;
mod pathbar;
mod query_agent;
mod query_composer;
mod sidebar;
mod source_search;
mod tag_review;
mod tag_manager;
mod theme;
mod window;

use app::SymtropyApp;
use std::sync::OnceLock;
use std::time::Instant;

static PROCESS_START: OnceLock<Instant> = OnceLock::new();

pub(crate) fn startup_ms() -> u128 {
    PROCESS_START.get_or_init(Instant::now).elapsed().as_millis()
}

fn main() -> eframe::Result<()> {
    PROCESS_START.get_or_init(Instant::now);
    eprintln!("startup: process start @ {}ms", startup_ms());
    // Optional positional argument: the image library to open. Falls back to
    // SYMTROPY_LIBRARY, then an executable/cwd-relative lookup. An explicit
    // choice overrides the directory remembered in the saved UI state.
    let explicit_library = std::env::args_os()
        .nth(1)
        .map(std::path::PathBuf::from)
        .or_else(paths::env_library_dir);
    let library = explicit_library
        .clone()
        .unwrap_or_else(paths::default_library_dir);

    // winit 0.29 has no public Wayland window-geometry API. The patched local
    // winit reads this before creating each surface and reports the visible
    // plate through xdg_surface.set_window_geometry, leaving the transparent
    // overhang outside snapping/tiling calculations. It is cleared in the app
    // creator below so only the main window gets that margin.
    std::env::set_var(
        "SYMTROPY_WAYLAND_SHADOW_MARGIN_PX",
        crate::window::OVERHANG.to_string(),
    );

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
        .with_title("Symtropy")
        .with_inner_size([1400.0, 900.0])
        .with_decorations(false)
        .with_transparent(true)
        .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native(
        "Symtropy",
        options,
        Box::new(move |cc| {
            // eframe has already created the root window, so the chrome margin
            // is in that window's state. Later viewports (the media viewer)
            // must not inherit it: they have no steel overhang.
            std::env::remove_var("SYMTROPY_WAYLAND_SHADOW_MARGIN_PX");
            let app = SymtropyApp::new(cc, library, explicit_library);
            eprintln!("startup: app created @ {}ms", startup_ms());
            Box::new(app)
        }),
    )
}
