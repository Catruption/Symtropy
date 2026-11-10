//! Pure geometry: port of window-draw.js (updateContentPosition).

use crate::theme::Theme;
use egui::{pos2, vec2, Align2, Pos2, Rect, Vec2};

pub struct Layout {
    pub content: Rect,
    pub title_anchor: Pos2,
    pub title_align: Align2,
    /// close, minimize, maximize (same order as the JS)
    pub buttons: [Rect; 3],
}

/// `plate` is the visible window (here: the whole viewport); the texture margin is added internally.
pub fn compute(plate: Rect, theme: &Theme) -> Layout {
    let c = &theme.config;
    let s = theme.scale();
    let side = theme.side(plate.size());
    let win = plate.expand(side); // frame incl. the transparent texture margin
    let inset = c.inset * s;

    let mut content = Rect::from_min_max(
        win.min + vec2(side + inset, side + inset * c.top_inset_multiplier),
        win.max - vec2(side + inset, side + inset),
    );
    if content.width() < 0.0 || content.height() < 0.0 {
        content = Rect::from_min_size(content.min, vec2(content.width().max(0.0), content.height().max(0.0)));
    }

    // Title: anchored 8px above / below the CONTENT rect.
    let t = &c.title_bar;
    let (ox, oy) = (t.offset_x * s, t.offset_y * s);
    let top_y = content.top() - 8.0 * s + oy;
    let bot_y = content.bottom() + 8.0 * s + oy;
    let (title_anchor, title_align) = match t.position.as_str() {
        "top-left" => (pos2(content.left() + ox, top_y), Align2::LEFT_BOTTOM),
        "top-right" => (pos2(content.right() + ox, top_y), Align2::RIGHT_BOTTOM),
        "bottom-left" => (pos2(content.left() + ox, bot_y), Align2::LEFT_TOP),
        "bottom-center" => (pos2(content.center().x + ox, bot_y), Align2::CENTER_TOP),
        "bottom-right" => (pos2(content.right() + ox, bot_y), Align2::RIGHT_TOP),
        _ => (pos2(content.center().x + ox, top_y), Align2::CENTER_BOTTOM),
    };

    // Controls: positioned relative to the FRAME; buttons overlap by `overlap`.
    let k = &c.controls;
    let (size, overlap) = (k.size * s, k.overlap * s);
    let total = size * 3.0 - overlap * 2.0;
    let origin = align_rect(win, vec2(total, size), &k.position, vec2(k.offset_x, k.offset_y) * s).min;
    let step = size - overlap;
    let buttons = [0, 1, 2].map(|i| Rect::from_min_size(origin + vec2(step * i as f32, 0.0), Vec2::splat(size)));

    Layout { content, title_anchor, title_align, buttons }
}

pub fn align_rect(win: Rect, size: Vec2, pos: &str, o: Vec2) -> Rect {
    let (w, h) = (win.width(), win.height());
    let (x, y) = match pos {
        "top-left" => (0.0, 0.0),
        "top-right" => (w - size.x, 0.0),
        "bottom-left" => (0.0, h - size.y),
        "bottom-center" => ((w - size.x) / 2.0, h - size.y),
        "bottom-right" => (w - size.x, h - size.y),
        _ => ((w - size.x) / 2.0, 0.0), // top-center
    };
    Rect::from_min_size(win.min + vec2(x, y) + o, size)
}

#[derive(Clone, Copy, PartialEq)]
pub struct ResizeDir {
    pub l: bool,
    pub r: bool,
    pub t: bool,
    pub b: bool,
}

impl ResizeDir {
    pub fn cursor(self) -> egui::CursorIcon {
        use egui::CursorIcon::*;
        if (self.l && self.t) || (self.r && self.b) {
            ResizeNwSe
        } else if (self.r && self.t) || (self.l && self.b) {
            ResizeNeSw
        } else if self.l || self.r {
            ResizeHorizontal
        } else {
            ResizeVertical
        }
    }

    pub fn to_viewport(self) -> egui::viewport::ResizeDirection {
        use egui::viewport::ResizeDirection as R;
        match (self.l, self.r, self.t, self.b) {
            (true, _, true, _) => R::NorthWest,
            (_, true, true, _) => R::NorthEast,
            (true, _, _, true) => R::SouthWest,
            (_, true, _, true) => R::SouthEast,
            (true, _, _, _) => R::West,
            (_, true, _, _) => R::East,
            (_, _, true, _) => R::North,
            _ => R::South,
        }
    }
}

/// Grab band along the inside of the viewport edge (the OS window has no border of its own).
pub fn border_dir(p: Pos2, plate: Rect, edge: f32) -> Option<ResizeDir> {
    if !plate.contains(p) {
        return None;
    }
    let d = ResizeDir {
        l: p.x < plate.left() + edge,
        r: p.x > plate.right() - edge,
        t: p.y < plate.top() + edge,
        b: p.y > plate.bottom() - edge,
    };
    (d.l || d.r || d.t || d.b).then_some(d)
}
