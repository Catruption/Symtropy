//! X11 (and XWayland) window-manager hints for the transparent, borderless steel window.
//!
//! * `_GTK_FRAME_EXTENTS`: tells the WM the transparent overhang band around the plate is invisible
//!   client-side shadow, so it tiles/snaps/maximizes the *plate* and grows the real window outward.
//! * XShape input region: only the plate (and the window buttons, which sit in the overhang) receive
//!   mouse input. Clicks anywhere else in the transparent band fall through to windows underneath.

use x11rb::connection::Connection;
use x11rb::protocol::shape::{ConnectionExt as _, SK, SO};
use x11rb::protocol::xproto::{AtomEnum, ClipOrdering, ConnectionExt as _, PropMode, Rectangle, Window};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

pub struct X11Window {
    conn: RustConnection,
    windows: Vec<Window>,
}

impl X11Window {
    /// Finds this process's window(s) by `_NET_WM_PID`. None until the window has been mapped.
    pub fn find() -> Option<Self> {
        let (conn, screen_num) = x11rb::connect(None).ok()?;
        let root = conn.setup().roots[screen_num].root;
        let pid_atom = conn.intern_atom(false, b"_NET_WM_PID").ok()?.reply().ok()?.atom;
        let pid = std::process::id();

        let mut windows = Vec::new();
        let mut stack: Vec<Window> = vec![root];
        while let Some(w) = stack.pop() {
            let ours = conn
                .get_property(false, w, pid_atom, AtomEnum::CARDINAL, 0, 1)
                .ok()
                .and_then(|c| c.reply().ok())
                .and_then(|r| r.value32().and_then(|mut v| v.next()))
                == Some(pid);
            if ours {
                windows.push(w);
            }
            if let Some(tree) = conn.query_tree(w).ok().and_then(|c| c.reply().ok()) {
                stack.extend(tree.children);
            }
        }
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

    /// Restricts mouse input to the union of `rects` (window-relative physical pixels: x, y, w, h).
    pub fn set_input_region(&self, rects: &[[i32; 4]]) {
        let rects: Vec<Rectangle> = rects
            .iter()
            .filter(|r| r[2] > 0 && r[3] > 0)
            .map(|r| Rectangle {
                x: r[0].clamp(i16::MIN as i32, i16::MAX as i32) as i16,
                y: r[1].clamp(i16::MIN as i32, i16::MAX as i32) as i16,
                width: r[2].clamp(0, u16::MAX as i32) as u16,
                height: r[3].clamp(0, u16::MAX as i32) as u16,
            })
            .collect();
        if rects.is_empty() {
            return;
        }
        for &w in &self.windows {
            let _ = self.conn.shape_rectangles(SO::SET, SK::INPUT, ClipOrdering::UNSORTED, w, 0, 0, &rects);
        }
        let _ = self.conn.flush();
    }
}