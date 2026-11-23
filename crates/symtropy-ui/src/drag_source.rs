//! Outgoing file drag-and-drop for X11/XWayland.
//!
//! egui's `DragAndDrop` is purely in-process, and eframe/winit only implement
//! the *receiving* side of XDND. This module is the missing source side: it
//! owns `XdndSelection`, serves `text/uri-list`, and walks the XDND handshake
//! with whatever window is under the pointer.
//!
//! Internal drops stay in egui. The XDND handoff only starts once the pointer
//! leaves Symtropy's own window, so right-dragging inside the app still moves
//! or copies files exactly as before.

use crate::x11;
use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ClientMessageData, ClientMessageEvent, ConnectionExt as _, CreateWindowAux,
    EventMask, KeyButMask, PropMode, SelectionNotifyEvent, Timestamp, Window, WindowClass,
    SELECTION_NOTIFY_EVENT,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

const XDND_VERSION: u32 = 5;
const POLL_INTERVAL: Duration = Duration::from_millis(16);
const FINISH_TIMEOUT: Duration = Duration::from_secs(3);
const DELETE_GRACE: Duration = Duration::from_secs(1);
const CURRENT_TIME: Timestamp = 0;

#[derive(Clone, Debug)]
struct DragRequest {
    paths: Vec<PathBuf>,
    copy: bool,
    /// The button actually held down by the user. XDND itself is
    /// button-agnostic, so a left-drag can feed the exact same protocol as a
    /// right-drag; we only need to know which one to watch for release.
    button: KeyButMask,
}

struct DragBridge {
    request_tx: Sender<DragRequest>,
    move_rx: Mutex<Receiver<Vec<PathBuf>>>,
}

static BRIDGE: OnceLock<DragBridge> = OnceLock::new();

/// Called by the shared drag initiator when a right-drag is recognised.
pub fn begin_drag(paths: &[PathBuf], copy: bool, button: KeyButMask) {
    let paths: Vec<PathBuf> = paths
        .iter()
        .filter(|p| p.exists())
        .map(|p| absolute_path(p))
        .collect();
    if paths.is_empty() {
        return;
    }
    let bridge = BRIDGE.get_or_init(start_bridge);
    let _ = bridge.request_tx.send(DragRequest { paths, copy, button });
}

/// Opt-in verbose tracing for diagnosing XDND handoff problems.
fn trace(message: impl std::fmt::Display) {
    if std::env::var_os("SYMTROPY_DRAG_TRACE").is_some() {
        eprintln!("drag-source: {message}");
    }
}

/// Paths whose originals should be removed after an external target accepted
/// `XdndActionMove`. The caller feeds them through the normal Trash action so
/// cleanup and status use the same path as every other delete in the app.
pub fn take_external_moves() -> Vec<PathBuf> {
    let Some(bridge) = BRIDGE.get() else {
        return Vec::new();
    };
    let Ok(rx) = bridge.move_rx.lock() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    while let Ok(paths) = rx.try_recv() {
        out.extend(paths);
    }
    dedup(out)
}

fn start_bridge() -> DragBridge {
    let (request_tx, request_rx) = mpsc::channel();
    let (move_tx, move_rx) = mpsc::channel();
    thread::Builder::new()
        .name("symtropy-xdnd".into())
        .spawn(move || {
            match Manager::new(request_rx, move_tx) {
                Ok(mut manager) => manager.run(),
                Err(error) => eprintln!("drag-source: XDND unavailable: {error}"),
            }
        })
        .expect("spawn XDND thread");
    DragBridge { request_tx, move_rx: Mutex::new(move_rx) }
}

fn dedup(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    paths.into_iter().filter(|p| seen.insert(p.clone())).collect()
}

fn absolute_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    }
}

fn local_host() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|h| !h.trim().is_empty())
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "localhost".into())
}

#[derive(Clone, Copy)]
struct Atoms {
    aware: Atom,
    selection: Atom,
    enter: Atom,
    type_list: Atom,
    position: Atom,
    status: Atom,
    leave: Atom,
    drop: Atom,
    finished: Atom,
    proxy: Atom,
    action_copy: Atom,
    action_move: Atom,
    uri_list: Atom,
    utf8_string: Atom,
    text_plain: Atom,
    username: Atom,
    targets: Atom,
    delete: Atom,
}

impl Atoms {
    fn intern(conn: &RustConnection) -> Result<Self, String> {
        fn atom(conn: &RustConnection, name: &[u8]) -> Result<Atom, String> {
            Ok(conn.intern_atom(false, name).map_err(|e| e.to_string())?.reply().map_err(|e| e.to_string())?.atom)
        }
        Ok(Self {
            aware: atom(conn, b"XdndAware")?,
            selection: atom(conn, b"XdndSelection")?,
            enter: atom(conn, b"XdndEnter")?,
            type_list: atom(conn, b"XdndTypeList")?,
            position: atom(conn, b"XdndPosition")?,
            status: atom(conn, b"XdndStatus")?,
            leave: atom(conn, b"XdndLeave")?,
            drop: atom(conn, b"XdndDrop")?,
            finished: atom(conn, b"XdndFinished")?,
            proxy: atom(conn, b"XdndProxy")?,
            action_copy: atom(conn, b"XdndActionCopy")?,
            action_move: atom(conn, b"XdndActionMove")?,
            uri_list: atom(conn, b"text/uri-list")?,
            utf8_string: atom(conn, b"UTF8_STRING")?,
            text_plain: atom(conn, b"text/plain;charset=utf-8")?,
            username: atom(conn, b"text/x-xdnd-username")?,
            targets: atom(conn, b"TARGETS")?,
            delete: atom(conn, b"DELETE")?,
        })
    }

    fn data_types(&self) -> [Atom; 5] {
        [self.targets, self.uri_list, self.utf8_string, self.text_plain, self.username]
    }
}

#[derive(Clone, Copy)]
struct Target {
    /// Window the pointer is actually over. XDND client messages must carry
    /// this id even when the event is delivered to a proxy.
    window: Window,
    /// Window that receives the XDND client messages (XdndProxy-aware).
    message_window: Window,
    version: u8,
}

#[derive(Clone)]
struct DragData {
    uris: Vec<u8>,
    text: Vec<u8>,
    username: Vec<u8>,
}

struct ActiveDrag {
    paths: Vec<PathBuf>,
    data: DragData,
    copy: bool,
    button: KeyButMask,
    target: Option<Target>,
    last_pos: Option<(i16, i16)>,
    awaiting_status: bool,
    pending_pos: Option<(i16, i16)>,
    accepted: bool,
    move_accepted: bool,
    selection_owned: bool,
    /// XdndSelection is claimed once, the first time the pointer leaves our windows. Claiming it
    /// when the drag is merely armed makes KWin start a compositor-level drag while the pointer
    /// is still over our own window, which takes the pointer away from egui (no motion, no
    /// release), so buttons and internal drops stop working until the next press.
    selection_claimed: bool,
    drop_sent_at: Option<Instant>,
    /// Button release seen before the target's XdndStatus arrived. Wait briefly
    /// so a fast drag onto a slow target still completes the drop.
    release_pending_at: Option<Instant>,
    delete_sent: bool,
    /// Some targets send XdndFinished before their delayed DELETE selection
    /// request. Keep the drag data alive briefly so that request still works.
    finished_at: Option<Instant>,
}

struct Manager {
    conn: RustConnection,
    root: Window,
    source: Window,
    atoms: Atoms,
    our_windows: HashSet<Window>,
    request_rx: Receiver<DragRequest>,
    move_tx: Sender<Vec<PathBuf>>,
    active: Option<ActiveDrag>,
}

impl Manager {
    fn new(request_rx: Receiver<DragRequest>, move_tx: Sender<Vec<PathBuf>>) -> Result<Self, String> {
        let (conn, screen_num) = x11rb::connect(None).map_err(|e| e.to_string())?;
        let screen = &conn.setup().roots[screen_num];
        let root = screen.root;

        let source = conn
            .generate_id()
            .map_err(|e| e.to_string())?;
        // InputOnly windows must use CopyFromParent (0) as their visual.
        conn.create_window(
            0,
            source,
            root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            0,
            &CreateWindowAux::new().event_mask(EventMask::NO_EVENT),
        )
        .map_err(|e| e.to_string())?;
        conn.flush().map_err(|e| e.to_string())?;

        let atoms = Atoms::intern(&conn)?;
        let types = atoms.data_types();
        conn.change_property32(PropMode::REPLACE, source, atoms.type_list, AtomEnum::ATOM, &types)
            .map_err(|e| e.to_string())?;
        conn.flush().map_err(|e| e.to_string())?;

        Ok(Self {
            conn,
            root,
            source,
            atoms,
            our_windows: HashSet::new(),
            request_rx,
            move_tx,
            active: None,
        })
    }

    fn run(&mut self) {
        loop {
            while let Ok(request) = self.request_rx.try_recv() {
                self.start(request);
            }
            loop {
                match self.conn.poll_for_event() {
                    Ok(Some(event)) => self.handle_event(event),
                    Ok(None) => break,
                    Err(error) => {
                        eprintln!("drag-source: X11 error: {error}");
                        return;
                    }
                }
            }
            if self.active.is_some() {
                self.poll_pointer();
            }
            thread::sleep(POLL_INTERVAL);
        }
    }

    fn start(&mut self, request: DragRequest) {
        if let Some(active) = &self.active {
            if active.paths == request.paths
                && active.copy == request.copy
                && active.button == request.button
            {
                begin_drag_called_again();
                return;
            }
            self.cleanup();
        }
        self.our_windows = x11::pid_windows(&self.conn, self.root, std::process::id());
        let data = build_drag_data(&request.paths);
        eprintln!(
            "drag-source: armed {} file(s) ({}, {} button)",
            request.paths.len(),
            if request.copy { "copy" } else { "move" },
            if request.button == KeyButMask::BUTTON1 { "left" } else { "right" }
        );
        self.active = Some(ActiveDrag {
            paths: request.paths,
            data,
            copy: request.copy,
            button: request.button,
            target: None,
            last_pos: None,
            awaiting_status: false,
            pending_pos: None,
            accepted: false,
            move_accepted: false,
            selection_owned: false,
            selection_claimed: false,
            drop_sent_at: None,
            release_pending_at: None,
            delete_sent: false,
            finished_at: None,
        });
        // Selection ownership is deferred to poll_pointer: see `selection_claimed`.
    }

    fn handle_event(&mut self, event: Event) {
        match event {
            Event::SelectionRequest(e) => self.serve_selection(&e),
            Event::SelectionClear(e) => {
                if let Some(active) = &mut self.active {
                    if e.selection == self.atoms.selection {
                        active.selection_owned = false;
                    }
                }
            }
            Event::ClientMessage(e) => {
                let data = e.data.as_data32();
                if e.type_ == self.atoms.status {
                    self.handle_status(data);
                } else if e.type_ == self.atoms.finished {
                    eprintln!("drag-source: target finished");
                    let wait_for_delete = match &mut self.active {
                        Some(active)
                            if active.move_accepted
                                && !active.delete_sent
                                && active.drop_sent_at.is_some() =>
                        {
                            active.finished_at = Some(Instant::now());
                            true
                        }
                        _ => false,
                    };
                    if !wait_for_delete {
                        self.cleanup();
                    }
                }
            }
            _ => {}
        }
    }

    fn handle_status(&mut self, data: [u32; 5]) {
        let Some(active) = &mut self.active else { return };
        let Some(target) = active.target else { return };
        // data[0] is the target window id; stale statuses from a previous target
        // must not change this drag's state.
        if data[0] != target.window {
            return;
        }
        let accepted = data[1] & 1 != 0;
        let action = data[4];
        let copy = active.copy;
        active.awaiting_status = false;
        active.accepted = accepted;
        active.move_accepted = accepted
            && !copy
            && (action == self.atoms.action_move || action == 0);
        if active.move_accepted {
            eprintln!("drag-source: target accepted Move");
        }
        let pending = active.pending_pos.take();
        if let Some((x, y)) = pending {
            self.send_position(x, y);
        }
    }

    fn serve_selection(&mut self, e: &x11rb::protocol::xproto::SelectionRequestEvent) {
        let atoms = self.atoms;
        if e.selection != atoms.selection {
            self.notify(e, 0);
            return;
        }
        let property = if e.property == 0 { e.target } else { e.property };
        let data = self.active.as_ref().map(|a| a.data.clone());
        let Some(data) = data else {
            self.notify(e, 0);
            return;
        };

        if e.target == atoms.delete {
            let should_delete = self
                .active
                .as_ref()
                .map_or(false, |a| a.move_accepted && !a.delete_sent);
            if should_delete {
                let paths = self.active.as_ref().unwrap().paths.clone();
                if let Some(active) = &mut self.active {
                    active.delete_sent = true;
                }
                eprintln!("drag-source: target requested DELETE for {} file(s)", paths.len());
                let _ = self.move_tx.send(paths);
            }
            self.notify(e, e.property);
            return;
        }

        if e.target == atoms.targets {
            let types = atoms.data_types();
            let _ = self.conn.change_property32(
                PropMode::REPLACE,
                e.requestor,
                property,
                AtomEnum::ATOM,
                &types,
            );
            self.notify(e, property);
            return;
        }

        let (bytes, type_atom) = if e.target == atoms.uri_list {
            (&data.uris, atoms.uri_list)
        } else if e.target == atoms.utf8_string || e.target == atoms.text_plain {
            (&data.text, e.target)
        } else if e.target == atoms.username {
            (&data.username, u32::from(AtomEnum::STRING))
        } else {
            self.notify(e, 0);
            return;
        };
        let _ = self.conn.change_property8(
            PropMode::REPLACE,
            e.requestor,
            property,
            type_atom,
            bytes,
        );
        self.notify(e, property);
    }

    fn notify(&self, e: &x11rb::protocol::xproto::SelectionRequestEvent, property: Atom) {
        let event = SelectionNotifyEvent {
            response_type: SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: e.time,
            requestor: e.requestor,
            selection: e.selection,
            target: e.target,
            property,
        };
        let _ = self.conn.send_event(false, e.requestor, EventMask::NO_EVENT, event);
        let _ = self.conn.flush();
    }

    fn poll_pointer(&mut self) {
        let Some(active) = &self.active else { return };
        if active.drop_sent_at.map_or(false, |at| at.elapsed() > FINISH_TIMEOUT) {
            eprintln!("drag-source: target did not send XdndFinished; cleaning up");
            self.cleanup();
            return;
        }
        // A target can finish before issuing its delayed DELETE request. Give
        // that request a short window before releasing XdndSelection.
        if active.finished_at.map_or(false, |at| at.elapsed() > DELETE_GRACE) {
            self.cleanup();
            return;
        }
        // Once XdndDrop has been sent the pointer is already released. Never
        // call release_button() again or we would send XdndLeave and tear down
        // the selection the target still needs.
        if active.drop_sent_at.is_some() {
            return;
        }

        let Some(pointer) = self.conn.query_pointer(self.root).ok().and_then(|c| c.reply().ok()) else {
            return;
        };
        let held = active.button;
        trace(format!(
            "pointer ({}, {}) child=0x{:x} mask=0x{:x} held=0x{:x} in_app={} target={:?}",
            pointer.root_x,
            pointer.root_y,
            pointer.child,
            pointer.mask.bits(),
            held.bits(),
            self.pointer_in_app(pointer.child),
            active.target.map(|t| t.message_window)
        ));
        if !pointer.mask.contains(held) {
            let (awaiting, pending_since) = {
                let active = self.active.as_ref().unwrap();
                (active.awaiting_status, active.release_pending_at)
            };
            if pending_since.is_none() && awaiting {
                if let Some(active) = &mut self.active {
                    active.release_pending_at = Some(Instant::now());
                }
                return;
            }
            if let Some(since) = pending_since {
                if awaiting {
                    if since.elapsed() > Duration::from_millis(250) {
                        let target = self.active.as_ref().and_then(|a| a.target);
                        if let Some(target) = target {
                            self.send_leave(target);
                        }
                        self.cleanup();
                    }
                    return;
                }
                if let Some(active) = &mut self.active {
                    active.release_pending_at = None;
                }
            }
            self.release_button();
            return;
        }
        if self.pointer_in_app(pointer.child) {
            self.leave_target();
            return;
        }

        // First time outside our windows: claim XdndSelection now and give the compositor a
        // poll or two to notice before probing for a target (KWin maps its Wayland drop proxy
        // only once it knows about the drag).
        let claimed = self.active.as_ref().map_or(true, |a| a.selection_claimed);
        if !claimed {
            self.own_selection();
            if let Some(active) = &mut self.active {
                active.selection_claimed = true;
            }
            return;
        }

        let x = pointer.root_x;
        let y = pointer.root_y;
        if self.active.as_ref().and_then(|a| a.last_pos) == Some((x, y)) {
            return;
        }

        let target = self.find_target(x, y);
        if target.is_none() {
            trace(format!("no XDND target under pointer at ({x}, {y})"));
        }
        let changed = {
            let active = self.active.as_ref().unwrap();
            target.map(|t| t.message_window) != active.target.map(|t| t.message_window)
        };
        if changed {
            self.leave_target();
            if let Some(target) = target {
                if let Some(active) = &mut self.active {
                    active.target = Some(target);
                    active.last_pos = None;
                    active.awaiting_status = false;
                    active.pending_pos = None;
                    active.accepted = false;
                    active.move_accepted = false;
                    active.drop_sent_at = None;
                    active.release_pending_at = None;
                    active.delete_sent = false;
                    active.finished_at = None;
                }
                self.own_selection();
                self.send_enter(target);
                eprintln!("drag-source: entered XDND target 0x{:x}", target.message_window);
            }
        }
        self.send_position(x, y);
    }

    fn release_button(&mut self) {
        let Some(active) = &self.active else { return };
        let Some(target) = active.target else {
            self.cleanup();
            return;
        };
        if active.accepted && active.drop_sent_at.is_none() {
            eprintln!("drag-source: dropping on 0x{:x}", target.message_window);
            self.send_drop(target, CURRENT_TIME);
            if let Some(active) = &mut self.active {
                active.drop_sent_at = Some(Instant::now());
            }
        } else {
            self.send_leave(target);
            self.cleanup();
        }
    }

    fn leave_target(&mut self) {
        let Some(active) = &mut self.active else { return };
        let target = active.target.take();
        if let Some(target) = target {
            self.send_leave(target);
        }
        if let Some(active) = &mut self.active {
            active.last_pos = None;
            active.awaiting_status = false;
            active.pending_pos = None;
            active.accepted = false;
            active.move_accepted = false;
            active.drop_sent_at = None;
            active.release_pending_at = None;
            active.delete_sent = false;
            active.finished_at = None;
        }
    }

    fn cleanup(&mut self) {
        self.disown_selection();
        self.active = None;
    }

    fn own_selection(&mut self) {
        let owned = self.active.as_ref().map_or(false, |a| a.selection_owned);
        if owned {
            return;
        }
        let _ = self.conn.set_selection_owner(
            self.source,
            self.atoms.selection,
            CURRENT_TIME,
        );
        let _ = self.conn.flush();
        if let Some(active) = &mut self.active {
            active.selection_owned = true;
        }
    }

    fn disown_selection(&mut self) {
        let owned = self.active.as_ref().map_or(false, |a| a.selection_owned);
        if !owned {
            return;
        }
        let _ = self.conn.set_selection_owner(0u32, self.atoms.selection, CURRENT_TIME);
        let _ = self.conn.flush();
        if let Some(active) = &mut self.active {
            active.selection_owned = false;
        }
    }

    /// `event_window` is the window under the pointer; `dest` is where the
    /// event is sent. They differ only with `XdndProxy`, but XDND requires the
    /// event itself to keep naming the real target window.
    fn send_client_message(&self, event_window: Window, dest: Window, type_: Atom, data: [u32; 5]) {
        let event = ClientMessageEvent::new(32, event_window, type_, ClientMessageData::from(data));
        let _ = self.conn.send_event(false, dest, EventMask::NO_EVENT, event);
        let _ = self.conn.flush();
    }

    fn send_enter(&self, target: Target) {
        // Bit 0 set: the full type list is on XdndTypeList. High byte: version.
        let flags = 1 | (target.version as u32) << 24;
        self.send_client_message(
            target.window,
            target.message_window,
            self.atoms.enter,
            [self.source, flags, self.atoms.uri_list, self.atoms.utf8_string, self.atoms.text_plain],
        );
    }

    fn send_position(&mut self, x: i16, y: i16) {
        let Some(active) = &mut self.active else { return };
        active.last_pos = Some((x, y));
        if active.awaiting_status {
            active.pending_pos = Some((x, y));
            return;
        }
        let Some(target) = active.target else { return };
        let copy = active.copy;
        let action = if copy { self.atoms.action_copy } else { self.atoms.action_move };
        self.send_client_message(
            target.window,
            target.message_window,
            self.atoms.position,
            [self.source, 0, pack_coords(x, y), CURRENT_TIME, action],
        );
        if let Some(active) = &mut self.active {
            active.awaiting_status = true;
        }
    }

    fn send_leave(&self, target: Target) {
        self.send_client_message(
            target.window,
            target.message_window,
            self.atoms.leave,
            [self.source, 0, 0, 0, 0],
        );
    }

    fn send_drop(&self, target: Target, time: Timestamp) {
        self.send_client_message(
            target.window,
            target.message_window,
            self.atoms.drop,
            [self.source, 0, time.into(), 0, 0],
        );
    }

    /// True when the top-level window under the pointer is, or contains, one of ours. Under a
    /// reparenting window manager that top-level is the WM frame, and our client window is a
    /// descendant of it, so the search has to go down, not up.
    fn pointer_in_app(&self, child: Window) -> bool {
        self.subtree_has_ours(child, 0)
    }

    fn subtree_has_ours(&self, window: Window, depth: u8) -> bool {
        if window == 0 || window == self.root {
            return false;
        }
        if self.our_windows.contains(&window) {
            return true;
        }
        if depth >= 4 {
            return false;
        }
        let Some(tree) = self.conn.query_tree(window).ok().and_then(|c| c.reply().ok()) else {
            return false;
        };
        tree.children.iter().any(|c| self.subtree_has_ours(*c, depth + 1))
    }

    fn find_target(&self, x: i16, y: i16) -> Option<Target> {
        let pointer = self.conn.query_pointer(self.root).ok()?.reply().ok()?;
        self.descend(pointer.child, x, y)
    }

    fn descend(&self, window: Window, x: i16, y: i16) -> Option<Target> {
        if window == 0 || self.our_windows.contains(&window) {
            return None;
        }
        if let Some(tree) = self.conn.query_tree(window).ok().and_then(|c| c.reply().ok()) {
            for child in tree.children.iter().rev() {
                let Some(coords) = self.conn.translate_coordinates(self.root, *child, x, y).ok().and_then(|c| c.reply().ok()) else {
                    continue;
                };
                if !coords.same_screen || coords.dst_x < 0 || coords.dst_y < 0 {
                    continue;
                }
                let Some(geometry) = self.conn.get_geometry(*child).ok().and_then(|c| c.reply().ok()) else {
                    continue;
                };
                if (coords.dst_x as u16) < geometry.width && (coords.dst_y as u16) < geometry.height {
                    if let Some(target) = self.descend(*child, x, y) {
                        return Some(target);
                    }
                }
            }
        }
        self.check_aware(window)
    }

    fn check_aware(&self, window: Window) -> Option<Target> {
        let proxy = self
            .conn
            .get_property(false, window, self.atoms.proxy, AtomEnum::WINDOW, 0, 1)
            .ok()
            .and_then(|c| c.reply().ok())
            .and_then(|r| r.value32().and_then(|mut v| v.next()))
            .unwrap_or(0);
        let valid_proxy = proxy != 0
            && proxy != window
            && self
                .conn
                .get_property(false, proxy, self.atoms.proxy, AtomEnum::WINDOW, 0, 1)
                .ok()
                .and_then(|c| c.reply().ok())
                .and_then(|r| r.value32().and_then(|mut v| v.next()))
                == Some(proxy);
        let message_window = if valid_proxy { proxy } else { window };
        let version = self
            .conn
            .get_property(false, message_window, self.atoms.aware, AtomEnum::ATOM, 0, 1)
            .ok()
            .and_then(|c| c.reply().ok())
            .and_then(|r| r.value32().and_then(|mut v| v.next()))
            .filter(|v| *v > 0 && *v <= XDND_VERSION)? as u8;
        Some(Target { window, message_window, version })
    }
}

fn begin_drag_called_again() {
    // The initiator calls begin_drag on every frame of a right-drag; the
    // manager keeps the first request until the button is released.
}

fn pack_coords(x: i16, y: i16) -> u32 {
    ((x as u16 as u32) << 16) | (y as u16 as u32)
}

fn file_uri(path: &Path) -> String {
    let absolute = std::fs::canonicalize(path).unwrap_or_else(|_| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir().unwrap_or_default().join(path)
        }
    });
    // XDND's file-dragging protocol requires the host name in every URI.
    let mut uri = format!("file://{}", local_host());
    for component in absolute.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(part) => {
                if !uri.ends_with('/') {
                    uri.push('/');
                }
                uri.push_str(&urlencoding::encode(&part.to_string_lossy()));
            }
            Component::ParentDir => {
                if !uri.ends_with('/') {
                    uri.push('/');
                }
                uri.push_str("..");
            }
            Component::Prefix(prefix) => uri.push_str(&prefix.as_os_str().to_string_lossy()),
        }
    }
    uri
}

fn build_drag_data(paths: &[PathBuf]) -> DragData {
    let uris: Vec<String> = paths.iter().map(|p| file_uri(p)).collect();
    let text: Vec<String> = paths
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    let username = std::env::var("USER").unwrap_or_default();
    DragData {
        uris: format!("{}\r\n", uris.join("\r\n")).into_bytes(),
        text: format!("{}\n", text.join("\n")).into_bytes(),
        username: format!("{}\0", username).into_bytes(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coords_pack_high_word_first() {
        assert_eq!(pack_coords(0x1234, 0x5678), 0x12345678);
        assert_eq!(pack_coords(-1, -1), 0xffffffff);
    }

    #[test]
    fn file_uri_includes_host_and_percent_encoding() {
        let uri = file_uri(Path::new("/tmp/a b#c.txt"));
        assert_eq!(uri, format!("file://{}/tmp/a%20b%23c.txt", local_host()));
    }

    #[test]
    fn uri_list_uses_crlf_terminators() {
        let data = build_drag_data(&[PathBuf::from("/tmp/symtropy-drag-test")]);
        assert_eq!(
            data.uris,
            format!("file://{}/tmp/symtropy-drag-test\r\n", local_host()).into_bytes()
        );
    }
}
