//! Native Wayland file drag-and-drop.
//!
//! winit 0.29 only implements incoming DnD on X11, and eframe does not expose
//! pointer serials. This module wraps the *same* `wl_display` winit uses, opens
//! a second event queue on it, and creates its own `wl_pointer` so it can see
//! button serials. The compositor owns hit testing once `start_drag` is called.
//!
//! Internal drags stay in egui. A native drag is only started after the pointer
//! leaves our viewport, so right-drag move/copy inside Symtropy is unchanged.
//! External drags are Copy-only: the source file is never moved or trashed,
//! regardless of which action a target selects.

use crate::actions::DragPayload;
use egui::Context;
use raw_window_handle::{HasDisplayHandle as _, HasWindowHandle as _, RawDisplayHandle, RawWindowHandle};
use std::collections::HashSet;
use std::fs::File;
use std::io::{Read as _, Write as _};
use std::os::fd::AsFd as _;
use std::os::unix::net::UnixStream;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use wayland_client::backend::{Backend, ObjectId};
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::wl_data_device::{self, WlDataDevice};
use wayland_client::protocol::wl_data_device_manager::{self, DndAction, WlDataDeviceManager};
use wayland_client::protocol::wl_data_offer::{self, WlDataOffer};
use wayland_client::protocol::wl_data_source::{self, WlDataSource};
use wayland_client::protocol::wl_pointer::{self, ButtonState, WlPointer};
use wayland_client::protocol::wl_registry;
use wayland_client::protocol::wl_seat::{self, Capability, WlSeat};
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{event_created_child, Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};

const URI_LIST: &str = "text/uri-list";
const BUTTON_LEFT: u32 = 0x110;
const BUTTON_RIGHT: u32 = 0x111;

#[derive(Clone, Debug)]
struct DragData {
    uris: Vec<u8>,
}

#[derive(Clone, Debug)]
struct PendingDrag {
    paths: Vec<PathBuf>,
    secondary: bool,
    serial: Option<u32>,
}

struct ActiveSource {
    _source: WlDataSource,
    data: DragData,
}

#[derive(Clone, Default)]
struct OfferData(Arc<Mutex<Vec<String>>>);

struct WaylandDrag {
    conn: Connection,
    qh: QueueHandle<WaylandDrag>,
    surface: WlSurface,
    _manager: WlDataDeviceManager,
    _seat: WlSeat,
    pointer: Option<WlPointer>,
    data_device: Option<WlDataDevice>,
    pointer_pos: Option<(f64, f64)>,
    press_serial: [Option<u32>; 2],
    pending: Option<PendingDrag>,
    active: Option<ActiveSource>,
    incoming_offer: Option<(WlDataOffer, u32, OfferData)>,
    drop_tx: Sender<Vec<PathBuf>>,
}

struct DragRuntime {
    queue: EventQueue<WaylandDrag>,
    state: WaylandDrag,
}

static DRAG: OnceLock<Mutex<Option<DragRuntime>>> = OnceLock::new();
static ATTEMPTED: AtomicBool = AtomicBool::new(false);

struct Channels {
    drop_tx: Sender<Vec<PathBuf>>,
    drop_rx: Mutex<Receiver<Vec<PathBuf>>>,
}

fn channels() -> &'static Channels {
    static CHANNELS: OnceLock<Channels> = OnceLock::new();
    CHANNELS.get_or_init(|| {
        let (drop_tx, drop_rx) = mpsc::channel();
        Channels { drop_tx, drop_rx: Mutex::new(drop_rx) }
    })
}

/// Called once per frame. Cheap after the first successful init; returns
/// immediately on X11 or if the compositor lacks a data device manager.
pub fn init_from_frame(frame: &mut eframe::Frame) {
    let lock = DRAG.get_or_init(|| Mutex::new(None));
    if ATTEMPTED.swap(true, Ordering::Relaxed) {
        return;
    }
    let mut guard = lock.lock().expect("wayland drag lock poisoned");
    if guard.is_some() {
        return;
    }
    let result = (|| -> Result<DragRuntime, String> {
        let RawDisplayHandle::Wayland(display) = frame
            .display_handle()
            .map_err(|e| e.to_string())?
            .as_raw()
        else {
            return Err("not a Wayland display".into());
        };
        let RawWindowHandle::Wayland(window) = frame
            .window_handle()
            .map_err(|e| e.to_string())?
            .as_raw()
        else {
            return Err("not a Wayland window".into());
        };

        let backend = unsafe { Backend::from_foreign_display(display.display.as_ptr() as *mut _) };
        let conn = Connection::from_backend(backend);
        let (globals, queue) = registry_queue_init::<WaylandDrag>(&conn)
            .map_err(|e| e.to_string())?;
        let qh = queue.handle();

        let manager: WlDataDeviceManager = globals
            .bind(&qh, 1..=3, ())
            .map_err(|e| e.to_string())?;
        // wl_seat is multi-instance, but a desktop session has one pointer
        // seat in practice. Dynamic seats are intentionally not hot-plugged.
        let seat: WlSeat = globals
            .bind(&qh, 1..=7, ())
            .map_err(|e| e.to_string())?;
        let data_device = Some(manager.get_data_device(&seat, &qh, ()));

        let surface_id = unsafe {
            ObjectId::from_ptr(WlSurface::interface(), window.surface.as_ptr() as *mut _)
        }
        .map_err(|e| e.to_string())?;
        let surface = WlSurface::from_id(&conn, surface_id).map_err(|e| e.to_string())?;

        let ch = channels();
        let state = WaylandDrag {
            conn: conn.clone(),
            qh,
            surface,
            _manager: manager,
            _seat: seat,
            pointer: None,
            data_device,
            pointer_pos: None,
            press_serial: [None, None],
            pending: None,
            active: None,
            incoming_offer: None,
            drop_tx: ch.drop_tx.clone(),
        };
        let mut runtime = DragRuntime { queue, state };
        // Process the initial seat-capability events so our own wl_pointer
        // starts receiving button serials.
        let _ = conn.flush();
        let _ = runtime.queue.dispatch_pending(&mut runtime.state);
        let _ = conn.flush();
        Ok(runtime)
    })();

    match result {
        Ok(runtime) => *guard = Some(runtime),
        Err(error) => eprintln!("wayland-drag: unavailable: {error}"),
    }
}

/// Pump our event queue and hand a deferred drag to the compositor once the
/// pointer has left our viewport.
pub fn tick(ctx: &Context) {
    let Some(lock) = DRAG.get() else { return };
    let Ok(mut guard) = lock.lock() else { return };
    let Some(runtime) = guard.as_mut() else { return };

    let _ = runtime.queue.dispatch_pending(&mut runtime.state);
    let _ = runtime.state.conn.flush();

    let screen = ctx.screen_rect();
    let pointer_outside = ctx.input(|i| i.pointer.latest_pos().is_none())
        || runtime.state.pointer_pos.map_or(false, |(x, y)| {
            x < 0.0 || y < 0.0 || x > screen.width() as f64 || y > screen.height() as f64
        });
    let can_start = runtime.state.pending.is_some()
        && runtime.state.active.is_none()
        && runtime.state.data_device.is_some()
        && runtime
            .state
            .pending
            .as_ref()
            .and_then(|p| p.serial)
            .is_some()
        && pointer_outside;

    if can_start {
        let pending = runtime.state.pending.take().expect("checked above");
        runtime.state.start_drag(pending);
        // The compositor now owns the button. Do not leave egui holding a
        // payload or a pressed button that will never receive a release.
        let _: Option<DragPayload> = DragAndDropTake::take(ctx);
        ctx.request_repaint();
    }
    if runtime.state.pending.is_some() {
        ctx.request_repaint();
    }
}

/// Small helper so the generic egui call stays readable.
struct DragAndDropTake;
impl DragAndDropTake {
    fn take(ctx: &Context) -> Option<DragPayload> {
        egui::DragAndDrop::take_payload::<DragPayload>(ctx).map(|p| (*p).clone())
    }
}

/// True only while the compositor owns the drag. `actions::classify()` uses
/// this to ignore the stale button state egui may keep after the pointer is
/// grabbed. Pending drags must stay visible to `classify()` so repeated drag
/// frames can fill in a serial that arrived after the first call.
pub fn is_active() -> bool {
    let Some(lock) = DRAG.get() else { return false };
    let Ok(guard) = lock.lock() else { return false };
    guard.as_ref().map_or(false, |r| r.state.active.is_some())
}

/// Arm a native drag. `secondary` selects the right-button serial; left-drag
/// keeps egui's normal selection/scroll behavior until the pointer leaves our
/// viewport. Native drags always copy; Ctrl only changes internal move/copy.
pub fn begin_drag(paths: &[PathBuf], secondary: bool) -> bool {
    let paths: Vec<PathBuf> = paths
        .iter()
        .filter(|p| p.exists())
        .map(|p| if p.is_absolute() { p.clone() } else { current_dir().join(p) })
        .collect();
    if paths.is_empty() {
        return false;
    }
    let Some(lock) = DRAG.get() else { return false };
    let Ok(mut guard) = lock.lock() else { return false };
    let Some(runtime) = guard.as_mut() else { return false };
    let idx = if secondary { 1 } else { 0 };
    let serial = runtime.state.press_serial[idx];
    let same = runtime
        .state
        .pending
        .as_ref()
        .map_or(false, |p| p.paths == paths && p.secondary == secondary);
    if !same {
        runtime.state.pending = Some(PendingDrag { paths, secondary, serial });
    } else if let Some(pending) = &mut runtime.state.pending {
        pending.serial = pending.serial.or(serial);
    }
    true
}

fn current_dir() -> PathBuf {
    std::env::current_dir().unwrap_or_default()
}

/// Files dropped into our window from another Wayland client.
pub fn take_incoming_drops() -> Vec<PathBuf> {
    let guard = channels().drop_rx.lock().expect("drop channel poisoned");
    dedup(guard.try_iter().flatten().collect())
}

fn dedup(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    paths.into_iter().filter(|p| seen.insert(p.clone())).collect()
}

impl WaylandDrag {
    fn start_drag(&mut self, pending: PendingDrag) {
        let Some(serial) = pending.serial else { return };
        let Some(device) = self.data_device.as_ref() else { return };
        let idx = if pending.secondary { 1 } else { 0 };
        self.press_serial[idx] = None;

        let source = self._manager.create_data_source(&self.qh, ());
        // text/uri-list is the correct MIME type for file drags. Chromium-based
        // targets that also accept text/plain can interpret the payload as a
        // filename instead of a URI and create an empty file, so do not offer
        // it at all.
        source.offer(URI_LIST.into());
        // A target must never be able to make us delete the source. If it asks
        // for Move anyway, there is no source-side cleanup to perform.
        if source.version() >= 3 {
            source.set_actions(DndAction::Copy);
        }

        let data = build_drag_data(&pending.paths);
        self.active = Some(ActiveSource { _source: source.clone(), data });
        device.start_drag(Some(&source), &self.surface, None, serial);
        let _ = self.conn.flush();
    }

    fn request_incoming(&mut self, offer: WlDataOffer, serial: u32, data: OfferData) {
        let has_uris = data
            .0
            .lock()
            .map_or(false, |types| types.iter().any(|m| m == URI_LIST));
        if !has_uris {
            return;
        }
        offer.accept(serial, Some(URI_LIST.into()));
        let Ok((mut read, write)) = UnixStream::pair() else { return };
        offer.receive(URI_LIST.into(), write.as_fd());
        drop(write);
        let _ = self.conn.flush();
        let tx = self.drop_tx.clone();
        std::thread::Builder::new()
            .name("symtropy-wayland-drop".into())
            .spawn(move || {
                let mut bytes = Vec::new();
                if read.read_to_end(&mut bytes).is_ok() {
                    let paths = paths_from_uri_list(&bytes);
                    if !paths.is_empty() {
                        let _ = tx.send(paths);
                    }
                }
            })
            .ok();
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for WaylandDrag {
    fn event(
        _state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlDataDeviceManager, ()> for WaylandDrag {
    fn event(
        _state: &mut Self,
        _manager: &WlDataDeviceManager,
        _event: wl_data_device_manager::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSeat, ()> for WaylandDrag {
    fn event(
        state: &mut Self,
        seat: &WlSeat,
        event: wl_seat::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities { capabilities: WEnum::Value(capabilities) } = event {
            if capabilities.contains(Capability::Pointer) && state.pointer.is_none() {
                state.pointer = Some(seat.get_pointer(qh, ()));
            }
        }
    }
}

impl Dispatch<WlPointer, ()> for WaylandDrag {
    fn event(
        state: &mut Self,
        _pointer: &WlPointer,
        event: wl_pointer::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Motion { surface_x, surface_y, .. } => {
                state.pointer_pos = Some((surface_x, surface_y));
            }
            wl_pointer::Event::Leave { .. } => state.pointer_pos = None,
            wl_pointer::Event::Button {
                serial,
                button,
                state: WEnum::Value(button_state),
                ..
            } => {
                let idx = match button {
                    BUTTON_LEFT => 0,
                    BUTTON_RIGHT => 1,
                    _ => return,
                };
                match button_state {
                    ButtonState::Pressed => state.press_serial[idx] = Some(serial),
                    ButtonState::Released => {
                        state.press_serial[idx] = None;
                        let secondary = idx == 1;
                        if state.active.is_none()
                            && state
                                .pending
                                .as_ref()
                                .map_or(false, |p| p.secondary == secondary)
                        {
                            // A normal click/drag ended inside the app; never
                            // leave an armed native drag behind to swallow
                            // future input.
                            state.pending = None;
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<WlDataDevice, ()> for WaylandDrag {
    event_created_child!(WaylandDrag, WlDataDevice, [
        0 => (WlDataOffer, OfferData::default())
    ]);

    fn event(
        state: &mut Self,
        _device: &WlDataDevice,
        event: wl_data_device::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_device::Event::Enter { serial, id, .. } => {
                // Ignore our own drag. Any Enter here is for one of this
                // client's surfaces, including the viewer, so no surface
                // comparison is needed.
                if state.active.is_some() {
                    if let Some(offer) = id {
                        offer.destroy();
                    }
                    return;
                }
                let Some(offer) = id else { return };
                let offer_data = offer.data::<OfferData>().cloned().unwrap_or_default();
                if offer_data
                    .0
                    .lock()
                    .map_or(false, |types| types.iter().any(|m| m == URI_LIST))
                {
                    offer.accept(serial, Some(URI_LIST.into()));
                    state.incoming_offer = Some((offer, serial, offer_data));
                } else {
                    offer.destroy();
                }
            }
            wl_data_device::Event::Leave { .. } => state.incoming_offer = None,
            wl_data_device::Event::Drop { .. } => {
                if let Some((offer, serial, data)) = state.incoming_offer.take() {
                    state.request_incoming(offer, serial, data);
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<WlDataOffer, OfferData> for WaylandDrag {
    fn event(
        _state: &mut Self,
        _offer: &WlDataOffer,
        event: wl_data_offer::Event,
        data: &OfferData,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wl_data_offer::Event::Offer { mime_type } = event {
            if let Ok(mut types) = data.0.lock() {
                types.push(mime_type);
            }
        }
    }
}

impl Dispatch<WlDataSource, ()> for WaylandDrag {
    fn event(
        state: &mut Self,
        _source: &WlDataSource,
        event: wl_data_source::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_source::Event::Send { mime_type, fd } => {
                let bytes = state
                    .active
                    .as_ref()
                    .and_then(|a| drag_bytes(&a.data, &mime_type));
                let mut file = File::from(fd);
                if let Some(bytes) = bytes {
                    let _ = file.write_all(bytes);
                }
            }
            wl_data_source::Event::DndFinished => {
                state.active = None;
                state.incoming_offer = None;
            }
            wl_data_source::Event::Cancelled => {
                state.active = None;
                state.incoming_offer = None;
            }
            _ => {}
        }
    }
}

fn file_uri(path: &Path) -> String {
    let absolute = std::fs::canonicalize(path).unwrap_or_else(|_| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            current_dir().join(path)
        }
    });
    let mut uri = String::from("file:///");
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

fn drag_bytes<'a>(data: &'a DragData, mime_type: &str) -> Option<&'a [u8]> {
    if mime_type == URI_LIST { Some(&data.uris) } else { None }
}

fn build_drag_data(paths: &[PathBuf]) -> DragData {
    let uris: Vec<String> = paths.iter().map(|p| file_uri(p)).collect();
    DragData { uris: format!("{}\r\n", uris.join("\r\n")).into_bytes() }
}

fn paths_from_uri_list(bytes: &[u8]) -> Vec<PathBuf> {
    String::from_utf8_lossy(bytes)
        .lines()
        .filter_map(|line| {
            let line = line.trim_end_matches('\r').trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let rest = line.strip_prefix("file://")?;
            let rest = rest.strip_prefix("localhost").unwrap_or(rest);
            let decoded = urlencoding::decode(rest).ok()?;
            Some(PathBuf::from(decoded.into_owned()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_uri_uses_local_syntax_and_percent_encoding() {
        assert_eq!(file_uri(Path::new("/tmp/a b#c.txt")), "file:///tmp/a%20b%23c.txt");
    }

    #[test]
    fn uri_list_uses_crlf_terminators() {
        let data = build_drag_data(&[PathBuf::from("/tmp/symtropy-drag-test")]);
        assert_eq!(data.uris, b"file:///tmp/symtropy-drag-test\r\n".to_vec());
    }

    #[test]
    fn only_uri_list_carries_drag_payload() {
        let data = build_drag_data(&[PathBuf::from("/tmp/symtropy-drag-test")]);
        assert_eq!(drag_bytes(&data, URI_LIST), Some(data.uris.as_slice()));
        assert_eq!(drag_bytes(&data, "text/plain;charset=utf-8"), None);
        assert_eq!(drag_bytes(&data, "application/octet-stream"), None);
    }

    #[test]
    fn incoming_uri_list_decodes_local_paths() {
        let paths = paths_from_uri_list(b"file:///tmp/a%20b.txt\r\nfile:///tmp/c.txt\r\n");
        assert_eq!(paths, vec![PathBuf::from("/tmp/a b.txt"), PathBuf::from("/tmp/c.txt")]);
    }
}
