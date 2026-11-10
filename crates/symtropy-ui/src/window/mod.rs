//! The steel window chrome. The OS window is transparent and undecorated, and it is bigger than
//! the visible plate by `overhang()` on every side. That band is where the decorations stick out.
//! The compositor is told (via _GTK_FRAME_EXTENTS, see x11.rs) that the band is invisible shadow,
//! so it tiles/maximizes/snaps the *plate*, not the whole OS window: tiled plates sit flush
//! against screen edges and against other windows, while the overhang spills past them.

mod layout;
mod render;

use crate::theme::Theme;
use egui::{pos2, vec2, Color32, Id, Rect, Sense, Stroke, ViewportCommand};
use render::{draw_corners, draw_edges};

const BORDER: f32 = 6.0; // resize grab band along the viewport edge
/// How far (in texture pixels) the decorations may overhang the plate while windowed. The texture
/// margin itself is ~500px per side, far more than the lobes/bolts actually stick out, so only this
/// much transparent OS-window margin is reserved; the rest of the margin is simply clipped.
pub const OVERHANG: f32 = 90.0;
/// Transparent band (in points) between the OS window edge and the plate. Always a whole number of
/// physical pixels at the default scale (it is `OVERHANG` texture pixels), which is exactly what
/// x11.rs reports to the window manager.
pub fn overhang(theme: &Theme) -> f32 {
    (OVERHANG * theme.scale()).min(theme.side(vec2(0.0, 0.0)))
}

/// Draws the chrome around the whole viewport and runs `add_content` inside the content area.
/// Returns the rects (in points) that should receive mouse input: the plate plus the window buttons.
pub fn show(ctx: &egui::Context, theme: &Theme, title: &str, add_content: impl FnOnce(&mut egui::Ui)) -> Vec<Rect> {
    theme.sync_scale(ctx.pixels_per_point());
    egui::CentralPanel::default().frame(egui::Frame::none()).show(ctx, |ui| {
        let screen = ctx.screen_rect();
        let (maximized, fullscreen) =
        ctx.input(|i| (i.viewport().maximized.unwrap_or(false), i.viewport().fullscreen.unwrap_or(false)));
        let side = theme.side(screen.size());
        // The plate is the OS window minus the overhang band. Only true fullscreen (where the WM
        // ignores frame extents) fills the whole viewport.
        let plate = if fullscreen { screen } else { screen.shrink(overhang(theme)) };
        let frame = plate.expand(side); // plate + full texture margin (clipped by the viewport)
    let layout = layout::compute(plate, theme);
    let mut regions = vec![plate];
    if theme.config.controls.enabled {
        regions.extend(layout.buttons.iter().copied());
    }
    let s = theme.scale();
    let painter = ui.painter().clone();

    // ---- decoration + title ----
    draw_edges(&painter, theme, frame);
    draw_corners(&painter, theme, frame);

    painter.text(
        layout.title_anchor,
        layout.title_align,
        title,
        theme.title_font(),
                 Theme::parse_color(&theme.config.title_bar.color),
    );

    let toggle_max = || ctx.send_viewport_cmd(ViewportCommand::Maximized(!maximized));

    // ---- dragging: the whole band above the content is the title bar ----
    let bar = Rect::from_min_max(plate.min, pos2(plate.max.x, layout.content.top()));
    let bar_resp = ui.interact(bar, Id::new("title-bar"), Sense::click_and_drag());
    if bar_resp.drag_started() {
        ctx.send_viewport_cmd(ViewportCommand::StartDrag);
    }
    if bar_resp.double_clicked() {
        toggle_max();
    }

    // ---- window controls: close, minimize, maximize ----
    if theme.config.controls.enabled {
        let full = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
        let texs = [
            (&theme.btn_close, &theme.btn_close_hl),
                                                                  (&theme.btn_min, &theme.btn_min_hl),
                                                                  (&theme.btn_max, &theme.btn_max_hl),
        ];
        for (i, (normal, hl)) in texs.iter().enumerate() {
            let r = snap(layout.buttons[i], ctx.pixels_per_point());
            let resp = ui.interact(r, Id::new(("btn", i)), Sense::click());
            let tex = if resp.hovered() { hl.as_ref().or(normal.as_ref()) } else { normal.as_ref() };
            if let Some(t) = tex {
                painter.image(t.id(), r, full, Color32::WHITE);
            }
            if resp.clicked() {
                match i {
                    0 => ctx.send_viewport_cmd(ViewportCommand::Close),
                                                                  1 => ctx.send_viewport_cmd(ViewportCommand::Minimized(true)),
                                                                  _ => toggle_max(),
                }
            }
        }
    }

    // ---- resize along the viewport edge, and shift+drag anywhere to move ----
    let (pressed, pos, shift) =
    ctx.input(|i| (i.pointer.primary_pressed(), i.pointer.latest_pos(), i.modifiers.shift));
    if let Some(p) = pos {
        if pressed && shift && plate.contains(p) {
            ctx.send_viewport_cmd(ViewportCommand::StartDrag);
        } else if !(maximized || fullscreen) {
            if let Some(dir) = layout::border_dir(p, plate, BORDER) {
                ctx.set_cursor_icon(dir.cursor());
                if pressed {
                    ctx.send_viewport_cmd(ViewportCommand::BeginResize(dir.to_viewport()));
                }
            }
        }
    }

    // ---- content area (theme "content": translucent black, 2px light border) ----
    let c = layout.content;
    // CHANGE 0.0 to 8.0 * s for rounded corners
    painter.rect_filled(c, 0.0, Color32::from_black_alpha(77));
    painter.rect_stroke(c, 0.0, Stroke::new(2.0 * s, Color32::from_white_alpha(77)));

    // The window adds NO padding of its own: content fills the frame right up to the inside
    // edge of the 2px stroke (the stroke is centered on the edge, so 1px in). Whatever is
    // placed here (the gallery, ...) decides its own margins.
    let inner = c.shrink(1.0 * s);
    let mut cui = ui.child_ui(inner, egui::Layout::top_down(egui::Align::Min));
    cui.set_clip_rect(inner.intersect(plate));
    add_content(&mut cui);
    regions
    })
    .inner
}

/// Snap a rect's origin to whole physical pixels so 1:1 textures stay crisp.
fn snap(r: Rect, ppp: f32) -> Rect {
    Rect::from_min_size(pos2((r.min.x * ppp).round() / ppp, (r.min.y * ppp).round() / ppp), r.size())
}
