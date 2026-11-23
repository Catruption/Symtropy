mod actions;
mod app;
mod drag_source;
mod filter;
mod gallery;
mod file_manager;
mod paths;
mod pathbar;
mod sidebar;
mod theme;
mod window;
mod x11;

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
    // Force X11/XWayland so our _GTK_FRAME_EXTENTS tiling hack works. winit 0.29 picks native
    // Wayland whenever WAYLAND_DISPLAY is set (it no longer reads WINIT_UNIX_BACKEND), so hide it.
    // This must run before anything else touches the environment or spawns a thread.
    if std::env::var_os("DISPLAY").is_some() {
        std::env::remove_var("WAYLAND_DISPLAY");
    }

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
            let app = SymtropyApp::new(cc, library, explicit_library);
            eprintln!("startup: app created @ {}ms", startup_ms());
            Box::new(app)
        }),
    )
}
