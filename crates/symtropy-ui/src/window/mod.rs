//! The steel window chrome. The OS window is transparent and undecorated, and it is bigger than
//! the visible plate by `overhang()` on every side. That band is where the decorations stick out.
//! The compositor is told (via _GTK_FRAME_EXTENTS, see x11.rs) that the band is invisible shadow,
//! so it tiles/maximizes/snaps the *plate*, not the whole OS window: tiled plates sit flush
//! against screen edges and against other windows, while the overhang spills past them.
//! The chrome-scale sliders sit in the plate-to-content gutter, not in that overhang band.

mod layout;
mod render;

use crate::theme::{self, Theme};
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
    (OVERHANG * theme.decor_scale()).min(theme.side(vec2(0.0, 0.0)))
}

/// Draws the chrome around the whole viewport and runs `add_content` inside the content area.
/// Returns the rects (in points) that should receive mouse input: the plate plus the window buttons.
pub fn show(ctx: &egui::Context, theme: &Theme, title: &str, add_content: impl FnOnce(&mut egui::Ui)) -> Vec<Rect> {
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

    // ---- gutter sliders: chrome settings inside the theme's side buffer ----
    if fullscreen {
        ui.memory_mut(|m| m.data.remove_temp::<FrozenStrip>(Id::new(SCALE_DRAG_ID)));
    } else {
        // These hug the content edge inside the plate-to-content band. The frame slider shrinks
        // the plate and content together, so draw_scale_strip freezes the rail at drag start;
        // otherwise the shrinking content would make the track chase the pointer.
        let (left, right) = scale_strip_rects(plate, layout.content);
        let frozen = ui.memory(|m| m.data.get_temp::<FrozenStrip>(Id::new(SCALE_DRAG_ID)));
        draw_scale_strip(ui, theme, left, ScaleKind::Frame, screen.size(), frozen);
        draw_scale_strip(ui, theme, right, ScaleKind::Buttons, screen.size(), frozen);
    }

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

    // ---- resize along the viewport edge, and shift+drag the titlebar to move ----
    let (pressed, pos, shift) =
    ctx.input(|i| (i.pointer.primary_pressed(), i.pointer.latest_pos(), i.modifiers.shift));
    if let Some(p) = pos {
        // Only the titlebar consumes Shift. Content clicks need it for range
        // selection, so a Shift+click inside `content` must never start a drag.
        if pressed && shift && bar.contains(p) {
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

const SCALE_DRAG_ID: &str = "chrome-scale-drag";
const STRIP_MAX_WIDTH: f32 = 24.0;
const STRIP_MIN_WIDTH: f32 = 8.0;
const STRIP_GAP: f32 = 2.0;
const STRIP_END_PAD: f32 = 18.0;

/// Vertical strips in the plate-to-content gutters, tight against the content edge.
/// The theme's side buffer is only ~16–20 points wide, so the strip is sized from that gutter
/// rather than given a fixed width that would be clipped by the plate.
fn scale_strip_rects(plate: Rect, content: Rect) -> (Rect, Rect) {
    let top = content.top() + STRIP_END_PAD;
    let bottom = content.bottom() - STRIP_END_PAD;
    let left_gutter = content.left() - plate.left();
    let right_gutter = plate.right() - content.right();
    let left_w = strip_width(left_gutter);
    let right_w = strip_width(right_gutter);

    let left_max = content.left() - STRIP_GAP;
    let right_min = content.right() + STRIP_GAP;
    let left = Rect::from_min_max(pos2(left_max - left_w, top), pos2(left_max, bottom)).intersect(plate);
    let right = Rect::from_min_max(pos2(right_min, top), pos2(right_min + right_w, bottom)).intersect(plate);
    (left, right)
}

fn strip_width(gutter: f32) -> f32 {
    (gutter - 2.0 * STRIP_GAP)
        .clamp(STRIP_MIN_WIDTH, STRIP_MAX_WIDTH)
        .min(gutter.max(0.0))
}

/// While a scale is being dragged, keep its strip at the geometry the drag began with.
#[derive(Clone, Copy)]
struct FrozenStrip {
    kind: ScaleKind,
    rect: Rect,
}

impl Default for FrozenStrip {
    fn default() -> Self {
        Self {
            kind: ScaleKind::Frame,
            rect: Rect::from_min_size(pos2(0.0, 0.0), vec2(0.0, 0.0)),
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum ScaleKind {
    Frame,
    Buttons,
}

impl ScaleKind {
    fn label(self) -> &'static str {
        match self {
            Self::Frame => "F",
            Self::Buttons => "B",
        }
    }

    fn tooltip(self, value: f32) -> String {
        let what = match self {
            Self::Frame => "Frame / corner image scale",
            Self::Buttons => "Window button scale",
        };
        format!("{what}\n{value:.2}× — drag to adjust")
    }

    fn value(self, theme: &Theme, screen: egui::Vec2) -> f32 {
        match self {
            Self::Frame => theme.frame_scale_setting().max(theme.min_frame_scale(screen)),
            Self::Buttons => theme.button_scale_setting(),
        }
    }

    fn range(self, theme: &Theme, screen: egui::Vec2) -> std::ops::RangeInclusive<f32> {
        match self {
            Self::Frame => {
                let min = theme.min_frame_scale(screen);
                min..=theme.max_frame_scale(screen).max(min)
            }
            Self::Buttons => theme::MIN_BUTTON_SCALE..=theme::MAX_BUTTON_SCALE,
        }
    }

    fn set(self, theme: &Theme, value: f32, screen: egui::Vec2) {
        match self {
            Self::Frame => theme.set_frame_scale(
                value.clamp(theme.min_frame_scale(screen), theme.max_frame_scale(screen)),
            ),
            Self::Buttons => theme.set_button_scale(value),
        }
    }
}

fn draw_scale_strip(
    ui: &mut egui::Ui,
    theme: &Theme,
    rect: Rect,
    kind: ScaleKind,
    screen: egui::Vec2,
    frozen: Option<FrozenStrip>,
) {
    let rect = frozen.filter(|f| f.kind == kind).map(|f| f.rect).unwrap_or(rect);
    if rect.width() < STRIP_MIN_WIDTH || rect.height() < 70.0 {
        return;
    }

    let inner = Rect::from_min_max(rect.min + vec2(2.0, 12.0), rect.max - vec2(2.0, 10.0));
    if inner.height() < 30.0 {
        return;
    }

    let hit = Rect::from_center_size(
        pos2(rect.center().x, inner.center().y),
        vec2(rect.width(), inner.height()),
    );
    let range = kind.range(theme, screen);
    let (min, max) = (*range.start(), *range.end());
    let response = ui.interact(hit, Id::new(("chrome_scale", kind.label())), Sense::click_and_drag());

    let drag_id = Id::new(SCALE_DRAG_ID);
    if response.drag_started() {
        ui.memory_mut(|m| m.data.insert_temp(drag_id, FrozenStrip { kind, rect }));
    }
    let primary_down = ui.input(|i| i.pointer.primary_down());
    if response.drag_stopped() || (frozen.is_some() && !primary_down) {
        ui.memory_mut(|m| m.data.remove_temp::<FrozenStrip>(drag_id));
    }

    if response.dragged() || response.clicked() {
        if let Some(p) = ui.input(|i| i.pointer.latest_pos()) {
            let n = ((hit.bottom() - p.y) / hit.height()).clamp(0.0, 1.0);
            let raw = if max > min { min + n * (max - min) } else { min };
            let stepped = (raw / 0.05).round() * 0.05;
            kind.set(theme, stepped.clamp(min, max), screen);
        }
    }

    let value = kind.value(theme, screen).clamp(min, max);
    let n = if max > min { (value - min) / (max - min) } else { 0.0 };
    let handle_y = hit.bottom() - n * hit.height();

    let painter = ui.painter().clone();
    painter.rect_filled(rect, 4.0, Color32::from_black_alpha(190));
    painter.rect_stroke(rect, 4.0, Stroke::new(1.0_f32, Color32::from_white_alpha(40)));

    let rail = Rect::from_center_size(
        pos2(rect.center().x, hit.center().y),
        vec2(3.0, hit.height()),
    );
    painter.rect_filled(rail, 1.5, Color32::from_white_alpha(70));
    painter.circle_filled(pos2(rect.center().x, handle_y), 3.5, Color32::from_white_alpha(230));

    painter.text(
        pos2(rect.center().x, rect.top() + 6.0),
        egui::Align2::CENTER_CENTER,
        kind.label(),
        egui::FontId::proportional(8.0),
        Color32::from_white_alpha(220),
    );
    let _ = response.on_hover_text(kind.tooltip(value));
}

/// Snap a rect's origin to whole physical pixels so 1:1 textures stay crisp.
fn snap(r: Rect, ppp: f32) -> Rect {
    Rect::from_min_size(pos2((r.min.x * ppp).round() / ppp, (r.min.y * ppp).round() / ppp), r.size())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scale_strips_fit_the_real_theme_gutter() {
        // At 1.25 px/pt the theme's 20px inset is a 16pt gutter. The strips must still fit
        // without spilling into the content or being clipped by the plate.
        let plate = Rect::from_min_max(pos2(100.0, 50.0), pos2(1100.0, 850.0));
        let content = plate.shrink(16.0);
        let (left, right) = scale_strip_rects(plate, content);

        assert!(plate.contains_rect(left));
        assert!(plate.contains_rect(right));
        assert!(!left.intersects(content));
        assert!(!right.intersects(content));
        assert_eq!(left.right(), content.left() - STRIP_GAP);
        assert_eq!(right.left(), content.right() + STRIP_GAP);
        assert_eq!(left.width(), 12.0);
        assert_eq!(right.width(), 12.0);
        assert_eq!(left.height(), content.height() - 2.0 * STRIP_END_PAD);
        assert_eq!(right.height(), content.height() - 2.0 * STRIP_END_PAD);
    }

    #[test]
    fn scale_strips_cap_at_the_max_width_in_a_large_gutter() {
        let plate = Rect::from_min_max(pos2(100.0, 50.0), pos2(1100.0, 850.0));
        let content = Rect::from_min_max(pos2(400.0, 150.0), pos2(800.0, 750.0));
        let (left, right) = scale_strip_rects(plate, content);

        assert!(plate.contains_rect(left));
        assert!(plate.contains_rect(right));
        assert!(!left.intersects(content));
        assert!(!right.intersects(content));
        assert_eq!(left.width(), STRIP_MAX_WIDTH);
        assert_eq!(right.width(), STRIP_MAX_WIDTH);
    }
}
