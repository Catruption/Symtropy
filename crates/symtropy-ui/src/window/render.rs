use crate::theme::Theme;

/// Port of drawCorners. All placement is done in whole PHYSICAL pixels so that, at 1:1, every
/// texel lands exactly on a screen pixel (no bilinear blur) and the two halves meet without a seam.
pub fn draw_corners(painter: &egui::Painter, theme: &Theme, win: egui::Rect) {
    use egui::{pos2, vec2, Color32, Rect};
    let (Some(tex), Some(cs_pts)) = (&theme.squares, theme.corner_px()) else { return };
    let ppp = painter.ctx().pixels_per_point();
    let tp = theme.scale() * ppp; // physical pixels per texture pixel (1.0 when 1:1)
    let tex_px = tex.size_vec2();
    let cs = cs_pts * ppp; // corner block size in physical pixels
    let ctw = cs / tp; // corner block size in texture pixels

    let (x0, y0) = ((win.min.x * ppp).round(), (win.min.y * ppp).round());
    let (wp, hp) = ((win.width() * ppp).round(), (win.height() * ppp).round());

    // Visible size of each half. When the window is smaller than two corner blocks the halves
    // are cropped to meet in the middle (JS overlap); lw + rw == wp so there is no seam.
    let (lw, th) = ((wp / 2.0).floor().min(cs.x), (hp / 2.0).floor().min(cs.y));
    let (rw, bh) = ((wp - (wp / 2.0).floor()).min(cs.x), (hp - (hp / 2.0).floor()).min(cs.y));

    // (dst x, dst y, visible w, visible h, src x, src y) - dst in physical px, src in texture px
    let quads = [
        (x0, y0, lw, th, 0.0, 0.0),
        (x0 + wp - rw, y0, rw, th, ctw.x + (ctw.x - rw / tp), 0.0),
        (x0, y0 + hp - bh, lw, bh, 0.0, ctw.y + (ctw.y - bh / tp)),
        (x0 + wp - rw, y0 + hp - bh, rw, bh, ctw.x + (ctw.x - rw / tp), ctw.y + (ctw.y - bh / tp)),
    ];
    for (dx, dy, vw, vh, sx, sy) in quads {
        if vw <= 0.0 || vh <= 0.0 {
            continue;
        }
        let dst = Rect::from_min_size(pos2(dx / ppp, dy / ppp), vec2(vw / ppp, vh / ppp));
        let uv = Rect::from_min_max(
            pos2(sx / tex_px.x, sy / tex_px.y),
            pos2((sx + vw / tp) / tex_px.x, (sy + vh / tp) / tex_px.y),
        );
        painter.image(tex.id(), dst, uv, Color32::WHITE);
    }
}

/// Port of drawDiagonalEdges (triangles texture), in scaled units.
pub fn draw_edges(painter: &egui::Painter, theme: &Theme, win: egui::Rect) {
    let tex = match &theme.triangles {
        Some(t) => t,
        None => return,
    };
    let w = win.width();
    let h = win.height();
    let tex_size = tex.size_vec2() * theme.scale();
    if tex_size.x <= 0.0 || tex_size.y <= 0.0 {
        return;
    }

    let lhs = w * tex_size.y;
    let rhs = h * tex_size.x;

    let (top, right, bottom, left);
    if lhs <= rhs {
        let apex_y = (w * tex_size.y) / (2.0 * tex_size.x);
        let apex_x = w / 2.0;
        top = vec![(0.0, 0.0), (w, 0.0), (apex_x, apex_y)];
        bottom = vec![(w, h), (0.0, h), (apex_x, h - apex_y)];
        left = vec![(0.0, 0.0), (apex_x, apex_y), (apex_x, h - apex_y), (0.0, h)];
        right = vec![(w, 0.0), (apex_x, apex_y), (apex_x, h - apex_y), (w, h)];
    } else {
        let apex_x = (h * tex_size.x) / (2.0 * tex_size.y);
        let apex_y = h / 2.0;
        left = vec![(0.0, 0.0), (0.0, h), (apex_x, apex_y)];
        right = vec![(w, 0.0), (w, h), (w - apex_x, apex_y)];
        top = vec![(0.0, 0.0), (w, 0.0), (w - apex_x, apex_y), (apex_x, apex_y)];
        bottom = vec![(0.0, h), (w, h), (w - apex_x, apex_y), (apex_x, apex_y)];
    }

    let edges = [
        (top, (w - tex_size.x) / 2.0, 0.0),
        (right, w - tex_size.x, (h - tex_size.y) / 2.0),
        (bottom, (w - tex_size.x) / 2.0, h - tex_size.y),
        (left, 0.0, (h - tex_size.y) / 2.0),
    ];

    let mut mesh = egui::Mesh::with_texture(tex.id());

    for (poly, dx, dy) in edges.iter() {
        let expanded = expand_polygon(poly, 0.75);
        let base = mesh.vertices.len() as u32;

        for &(px, py) in &expanded {
            mesh.vertices.push(egui::epaint::Vertex {
                pos: egui::pos2(win.min.x + px, win.min.y + py),
                uv: egui::pos2((px - dx) / tex_size.x, (py - dy) / tex_size.y),
                color: egui::Color32::WHITE,
            });
        }

        for i in 1..(expanded.len() - 1) {
            mesh.indices.push(base);
            mesh.indices.push(base + i as u32);
            mesh.indices.push(base + i as u32 + 1);
        }
    }

    painter.add(egui::Shape::mesh(mesh));
}

fn expand_polygon(pts: &[(f32, f32)], amount: f32) -> Vec<(f32, f32)> {
    let cx = pts.iter().map(|p| p.0).sum::<f32>() / pts.len() as f32;
    let cy = pts.iter().map(|p| p.1).sum::<f32>() / pts.len() as f32;
    pts.iter()
        .map(|&(x, y)| {
            let dx = x - cx;
            let dy = y - cy;
            let len = (dx * dx + dy * dy).sqrt().max(1e-6);
            (x + dx / len * amount, y + dy / len * amount)
        })
        .collect()
}
