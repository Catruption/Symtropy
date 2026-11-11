use egui::{Color32, Sense, Stroke, Ui, Vec2};

pub fn draw_gauge(ui: &mut Ui, scroll_ratio: f32, files: &[(String, u64)]) -> Option<f32> {
    let size = 150.0_f32;
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(size), Sense::click_and_drag());
    let painter = ui.painter();
    let center = rect.center();
    let radius = size / 2.0;

    painter.circle_filled(center, radius, Color32::from_rgb(30, 30, 35));
    painter.circle_stroke(center, radius, Stroke::new(2.0_f32, Color32::from_rgb(80, 80, 90)));

    let total_size: u64 = files.iter().map(|f| f.1).sum();
    if total_size > 0 {
        let mut current_angle = -std::f32::consts::FRAC_PI_2;
        for (_, file_size) in files.iter().take(50) {
            let ratio = *file_size as f32 / total_size as f32;
            let sweep = ratio * std::f32::consts::TAU;
            let p1 = center + Vec2::angled(current_angle) * radius;
            let _p2 = center + Vec2::angled(current_angle + sweep) * radius;
            painter.line_segment([center, p1], Stroke::new(2.0_f32, Color32::from_rgb(100, 150, 255)));
            current_angle += sweep;
        }
    }

    let start_angle = -std::f32::consts::FRAC_PI_2 - 2.35;
    let end_angle = -std::f32::consts::FRAC_PI_2 + 2.35;
    let needle_angle = start_angle + (scroll_ratio * (end_angle - start_angle));

    let needle_tip = center + Vec2::angled(needle_angle) * (radius - 10.0);
    painter.line_segment([center, needle_tip], Stroke::new(3.0_f32, Color32::RED));
    painter.circle_filled(center, 8.0, Color32::from_rgb(200, 200, 200));

    if response.dragged() {
        if let Some(mouse_pos) = response.interact_pointer_pos() {
            let mut angle = (mouse_pos - center).y.atan2((mouse_pos - center).x);
            if angle < start_angle { angle = start_angle; }
            if angle > end_angle { angle = end_angle; }
            return Some((angle - start_angle) / (end_angle - start_angle));
        }
    }
    None
}
