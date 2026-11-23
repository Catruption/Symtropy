//! X11 (and XWayland) window-manager hints for the transparent, borderless steel window.
//!
//! * `_GTK_FRAME_EXTENTS`: tells the WM the transparent overhang band around the plate is invisible
//!   client-side shadow, so it tiles/snaps/maximizes the *plate* and grows the real window outward.
//!
//! The window deliberately does not install an XShape input region. A mask that is stale or in the
//! wrong coordinate space makes every widget disappear from pointer input; accepting input over the
//! whole transparent client is safer than click-through decoration margins.

use std::collections::HashSet;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ConnectionExt as _, MapState, PropMode, Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

/// Every X window owned by `pid` (top-levels and their children) that carries
/// `_NET_WM_PID`. Used both for chrome hints and for keeping XDND inside our own
/// windows while a right-drag is still internal.
fn pid_atom(conn: &RustConnection) -> Option<Atom> {
    conn.intern_atom(false, b"_NET_WM_PID").ok()?.reply().ok().map(|r| r.atom)
}

fn window_has_pid(conn: &RustConnection, atom: Atom, window: Window, pid: u32) -> bool {
    conn.get_property(false, window, atom, AtomEnum::CARDINAL, 0, 1)
        .ok()
        .and_then(|c| c.reply().ok())
        .and_then(|r| r.value32().and_then(|mut v| v.next()))
        == Some(pid)
}

/// Every X window owned by `pid` (top-levels and their children) that carries
/// `_NET_WM_PID`. Used by XDND to keep a drag inside our own windows.
pub fn pid_windows(conn: &RustConnection, root: Window, pid: u32) -> HashSet<Window> {
    let Some(atom) = pid_atom(conn) else {
        return HashSet::new();
    };

    let mut windows = HashSet::new();
    let mut stack = vec![root];
    while let Some(w) = stack.pop() {
        if window_has_pid(conn, atom, w, pid) {
            windows.insert(w);
        }
        if let Some(tree) = conn.query_tree(w).ok().and_then(|c| c.reply().ok()) {
            stack.extend(tree.children);
        }
    }
    windows
}

/// Real application windows owned by `pid`.
///
/// `_NET_WM_PID` alone is not enough. A reparenting WM may copy it onto the
/// frame it creates, and toolkits create small helper/input-only windows with
/// the same PID. Applying frame extents to any of those makes window-manager
/// tiling disagree with the real client. A real client is a mapped,
/// non-override-redirect `InputOutput` window with a `WM_CLASS` and a usable size.
fn client_windows(conn: &RustConnection, root: Window, pid: u32) -> Vec<Window> {
    let Some(pid_property) = pid_atom(conn) else {
        return Vec::new();
    };
    let window_type = conn
        .intern_atom(false, b"_NET_WM_WINDOW_TYPE")
        .ok()
        .and_then(|c| c.reply().ok())
        .map(|r| r.atom);
    let normal_type = conn
        .intern_atom(false, b"_NET_WM_WINDOW_TYPE_NORMAL")
        .ok()
        .and_then(|c| c.reply().ok())
        .map(|r| r.atom);

    let mut found = Vec::new();
    let mut stack = vec![root];
    while let Some(w) = stack.pop() {
        if window_has_pid(conn, pid_property, w, pid) && is_client_window(conn, w) {
            if let (Some(type_atom), Some(normal)) = (window_type, normal_type) {
                let explicit_type = conn
                    .get_property(false, w, type_atom, AtomEnum::ATOM, 0, 1)
                    .ok()
                    .and_then(|c| c.reply().ok())
                    .and_then(|r| r.value32().and_then(|mut v| v.next()));
                if explicit_type.is_some() && explicit_type != Some(normal) {
                    continue;
                }
            }
            found.push(w);
        }
        if let Some(tree) = conn.query_tree(w).ok().and_then(|c| c.reply().ok()) {
            stack.extend(tree.children);
        }
    }
    found
}

fn is_client_window(conn: &RustConnection, window: Window) -> bool {
    let Some(attrs) = conn
        .get_window_attributes(window)
        .ok()
        .and_then(|c| c.reply().ok())
    else {
        return false;
    };
    if attrs.class != WindowClass::INPUT_OUTPUT
        || attrs.map_state == MapState::UNMAPPED
        || attrs.override_redirect
    {
        return false;
    }
    let Some(geometry) = conn.get_geometry(window).ok().and_then(|c| c.reply().ok()) else {
        return false;
    };
    if geometry.width < 200 || geometry.height < 200 {
        return false;
    }
    // Real winit/eframe windows have a class hint. WM frames do not.
    conn.get_property(false, window, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 256)
        .ok()
        .and_then(|c| c.reply().ok())
        .map_or(false, |r| r.value.iter().any(|b| *b != 0))
}

pub struct X11Window {
    conn: RustConnection,
    windows: Vec<Window>,
}

impl X11Window {
    /// Finds this process's window(s) by `_NET_WM_PID`. None until the window has been mapped.
    pub fn find() -> Option<Self> {
        let (conn, screen_num) = x11rb::connect(None).ok()?;
        let root = conn.setup().roots[screen_num].root;
        let windows = client_windows(&conn, root, std::process::id());
        (!windows.is_empty()).then_some(Self { conn, windows })
    }

    /// `px` (physical pixels) of invisible shadow on every side: left, right, top, bottom.
    pub fn set_frame_extents(&self, px: u32) {
        let Some(atom) = self.conn.intern_atom(false, b"_GTK_FRAME_EXTENTS").ok().and_then(|c| c.reply().ok()) else {
            return;
        };
        for &w in &self.windows {
            let _ = self.conn.change_property32(PropMode::REPLACE, w, atom.atom, AtomEnum::CARDINAL, &[px, px, px, px]);
        }
        let _ = self.conn.flush();
    }
}