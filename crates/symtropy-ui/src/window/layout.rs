//! Pure, stable geometry for the interactive window chrome.
//!
//! Nothing in here consults the decorative texture scale or the theme's control offsets. The
//! `plate` (and the fixed overhang around it) is the coordinate system for controls; the theme's
//! textures may scale or fail to load without moving a button, slider, or drag band.

use crate::theme::Theme;
use egui::{pos2, vec2, Align2, Pos2, Rect, Vec2};

/// Window controls are pinned to the plate's top-right corner and lifted into the overhang band.
/// These are layout constants, not theme coordinates: JSON `position`/`offsetX`/`offsetY` are
/// decoration metadata only and never define an interactive hit target.
const BUTTON_SIDE_GAP: f32 = 0.0;
const BUTTON_TOP_LIFT: f32 = 30.0;

pub struct Layout {
    pub content: Rect,
    pub title_anchor: Pos2,
    pub title_align: Align2,
    /// close, minimize, maximize (same order as the JS)
    pub buttons: [Rect; 3],
}

/// `viewport` is the whole OS window; `plate` is the stable interactive window inside its
/// fixed transparent band. Both are in egui points.
pub fn compute(viewport: Rect, plate: Rect, theme: &Theme) -> Layout {
    let c = &theme.config;
    let s = theme.scale();
    let inset = c.inset * s;

    // The texture margin used to be added and immediately subtracted here. Moving it out makes
    // content geometry independent of corner scale and of whether the textures loaded at all.
    let mut content = Rect::from_min_max(
        plate.min + vec2(inset, inset * c.top_inset_multiplier),
        plate.max - vec2(inset, inset),
    );
    if content.width() < 0.0 || content.height() < 0.0 {
        content = Rect::from_min_size(content.min, vec2(content.width().max(0.0), content.height().max(0.0)));
    }

    // Title: anchored 8px above / below the CONTENT rect, in layout points.
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

    // Controls are pinned to a fixed plate anchor. Only their size follows the button slider;
    // theme offsets and texture scale intentionally play no part in where they are placed.
    let k = &c.controls;
    let bs = theme.button_scale();
    let (size, overlap) = (k.size * bs, k.overlap * bs);
    let total = size * 3.0 - overlap * 2.0;
    let offset = vec2(BUTTON_SIDE_GAP, -BUTTON_TOP_LIFT);
    let group = clamp_rect(align_rect(plate, vec2(total, size), "top-right", offset), viewport);
    let step = size - overlap;
    let buttons = [0, 1, 2].map(|i| clamp_rect(
        Rect::from_min_size(group.min + vec2(step * i as f32, 0.0), Vec2::splat(size)),
        viewport,
    ));

    Layout { content, title_anchor, title_align, buttons }
}

/// Keep a control fully reachable inside the OS window, even for unusual theme offsets or a
/// very small window. This is a fixed-geometry clamp; it never depends on decoration scale.
fn clamp_rect(r: Rect, bounds: Rect) -> Rect {
    let w = r.width().min(bounds.width()).max(0.0);
    let h = r.height().min(bounds.height()).max(0.0);
    let x = r.min.x.clamp(bounds.left(), bounds.right() - w);
    let y = r.min.y.clamp(bounds.top(), bounds.bottom() - h);
    Rect::from_min_size(pos2(x, y), vec2(w, h))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn viewport() -> Rect {
        Rect::from_min_max(pos2(0.0, 0.0), pos2(1280.0, 800.0))
    }

    fn plate() -> Rect {
        Rect::from_min_max(pos2(90.0, 90.0), pos2(1190.0, 710.0))
    }

    fn theme() -> Theme {
        let theme = Theme::load(&egui::Context::default());
        theme.set_button_scale(1.0);
        theme
    }

    #[test]
    fn controls_and_content_ignore_frame_scale() {
        let theme = theme();
        theme.set_frame_scale(0.5);
        theme.sync_scale(1.0, viewport().size());
        let small = compute(viewport(), plate(), &theme);

        theme.set_frame_scale(3.0);
        theme.sync_scale(1.0, viewport().size());
        let large = compute(viewport(), plate(), &theme);

        assert_eq!(small.buttons, large.buttons);
        assert_eq!(small.content, large.content);
        assert_eq!(small.title_anchor, large.title_anchor);
    }

    #[test]
    fn buttons_stay_inside_viewport_at_all_scales() {
        let theme = theme();
        for scale in [0.5, 1.0, 3.0] {
            theme.set_button_scale(scale);
            theme.sync_scale(1.0, viewport().size());
            let layout = compute(viewport(), plate(), &theme);
            for button in layout.buttons {
                assert!(
                    viewport().contains_rect(button),
                    "{scale}x produced {button:?}"
                );
            }
        }
    }

    #[test]
    fn theme_control_offsets_cannot_move_the_buttons() {
        let mut theme = theme();
        theme.set_button_scale(1.0);
        theme.sync_scale(1.0, viewport().size());
        let expected = compute(viewport(), plate(), &theme).buttons;

        theme.config.controls.position = "bottom-center".into();
        theme.config.controls.offset_x = 500.0;
        theme.config.controls.offset_y = 470.0;

        assert_eq!(expected, compute(viewport(), plate(), &theme).buttons);
    }
}
