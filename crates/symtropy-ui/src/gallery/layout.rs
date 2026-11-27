//! Aspect-preserving strip mosaic for the gallery.
//!
//! Each strip is filled left-to-right with adjacent items. Strip height is
//! derived from the items' aspect ratios, so a strip always fills the
//! available width without scaling any image away from its own aspect ratio.
//! Different strips get different heights, which keeps the masonry look while
//! making a resize O(n) instead of a beam search.

const MIN_ASPECT: f32 = 0.25;
const MAX_ASPECT: f32 = 4.5;
const SIZE_POWER: f32 = 0.12;
const MIN_SIZE_SCALE: f32 = 0.8;
const MAX_SIZE_SCALE: f32 = 1.25;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

#[derive(Clone, Copy, Debug)]
pub struct Item {
    pub aspect: f32,
    /// Original pixel area. Used only to give large originals slightly taller
    /// strips; never used to distort an image's aspect ratio.
    pub area: f32,
}

pub struct Params {
    pub width: i32,
    pub target_h: f32,
    pub margin: i32,
}

fn clamped_aspect(aspect: f32) -> f32 {
    if aspect.is_finite() && aspect > 0.0 {
        aspect.clamp(MIN_ASPECT, MAX_ASPECT)
    } else {
        1.0
    }
}

/// A mild per-image height preference. The exponent is deliberately small:
/// big originals get more room, but not enough to break the mosaic rhythm.
fn preferred_heights(items: &[Item], target_h: f32) -> Vec<f32> {
    let mut known: Vec<f32> = items
        .iter()
        .map(|i| i.area)
        .filter(|a| a.is_finite() && *a > 1.0)
        .collect();
    known.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median = known.get(known.len() / 2).copied().unwrap_or(1.0);

    items
        .iter()
        .map(|i| {
            if i.area.is_finite() && i.area > 1.0 {
                let scale = (i.area / median)
                    .powf(SIZE_POWER)
                    .clamp(MIN_SIZE_SCALE, MAX_SIZE_SCALE);
                target_h * scale
            } else {
                target_h
            }
        })
        .collect()
}

pub fn compute(items: &[Item], p: &Params) -> (Vec<Cell>, i32) {
    let margin = p.margin.max(0);
    let n = items.len();
    if n == 0 {
        return (Vec::new(), margin);
    }

    let inner_w = (p.width - 2 * margin).max(1);
    let aspects: Vec<f32> = items.iter().map(|i| clamped_aspect(i.aspect)).collect();
    let preferred = preferred_heights(items, p.target_h.max(1.0));

    let mut cells = vec![Cell { x: margin, y: margin, w: 1, h: 1 }; n];
    let mut top = margin;
    let mut lo = 0;

    while lo < n {
        // Start a strip with one item. Its ideal height is width / aspect.
        let mut sum_a = aspects[lo];
        let mut target_sum = sum_a * preferred[lo];
        let mut count = 1usize;
        let mut height = inner_w as f32 / sum_a;
        let mut target = target_sum / sum_a;

        // Add neighbors while the strip height moves closer to the average
        // preferred height. Widths stay proportional to each item's aspect,
        // so every leaf keeps its own shape.
        while lo + count < n {
            let next_a = aspects[lo + count];
            if count + 1 > inner_w as usize {
                break;
            }
            let avail = (inner_w - margin * count as i32).max(1) as f32;
            let next_sum = sum_a + next_a;
            let next_target = (target_sum + next_a * preferred[lo + count]) / next_sum;
            let next_height = avail / next_sum;

            if (next_height - next_target).abs() <= (height - target).abs() {
                sum_a = next_sum;
                target_sum += next_a * preferred[lo + count];
                count += 1;
                height = next_height;
                target = next_target;
            } else {
                break;
            }
        }

        let gap = margin * count.saturating_sub(1) as i32;
        let avail = (inner_w - gap).max(1);
        let h_px = height.round().max(1.0) as i32;

        let mut x = margin;
        let mut acc = 0.0f32;
        for j in 0..count {
            acc += aspects[lo + j];
            let next_x = margin + ((acc / sum_a) * avail as f32).round() as i32;
            let next_x = next_x.clamp(x + 1, margin + avail);
            cells[lo + j] = Cell {
                x,
                y: top,
                w: next_x - x,
                h: h_px,
            };
            x = next_x;
        }

        lo += count;
        top += h_px;
        if lo < n {
            top += margin;
        }
    }

    (cells, top + margin)
}

/// Uniform-height rows, wrapping when the next thumbnail would overflow.
///
/// This is the Gallery mode: predictable density and O(n) layout. Aspect
/// ratios stay visible through clipping in the painter, so no image is
/// stretched to fill a cell.
pub fn compute_rows(items: &[Item], p: &Params) -> (Vec<Cell>, i32) {
    let margin = p.margin.max(0);
    let inner_w = (p.width - 2 * margin).max(1);
    if items.is_empty() {
        return (Vec::new(), margin);
    }

    let row_h = p.target_h.max(1.0).round().max(1.0) as i32;
    let mut cells = vec![Cell { x: margin, y: margin, w: 1, h: row_h }; items.len()];
    let mut x = margin;
    let mut y = margin;

    for (index, item) in items.iter().enumerate() {
        let aspect = clamped_aspect(item.aspect);
        let wanted = (row_h as f32 * aspect).round().max(1.0) as i32;
        let w = wanted.min(inner_w);
        if x > margin && x + w > margin + inner_w {
            y += row_h + margin;
            x = margin;
        }
        cells[index] = Cell { x, y, w, h: row_h };
        x += w + margin;
    }

    (cells, y + row_h + margin)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(aspect: f32, area: f32) -> Item {
        Item { aspect, area }
    }

    #[test]
    fn empty_layout_has_no_cells() {
        let (cells, h) = compute(&[], &Params { width: 800, target_h: 200.0, margin: 0 });
        assert!(cells.is_empty());
        assert_eq!(h, 0);
    }

    #[test]
    fn cells_stay_inside_the_viewport() {
        let items: Vec<_> = (0..64)
            .map(|i| item(0.5 + (i % 7) as f32 * 0.3, 1000.0 * (i as f32 + 1.0)))
            .collect();
        let p = Params { width: 640, target_h: 120.0, margin: 4 };
        let (cells, h) = compute(&items, &p);
        assert_eq!(cells.len(), items.len());
        assert!(h > 0);
        for c in &cells {
            assert!(c.x >= p.margin);
            assert!(c.x + c.w <= p.width - p.margin);
            assert!(c.y >= p.margin);
            assert!(c.w >= 1 && c.h >= 1);
        }
    }

    #[test]
    fn cells_do_not_overlap() {
        let items = vec![
            item(1.0, 100.0), item(0.7, 400.0), item(1.8, 900.0),
            item(1.2, 1600.0), item(0.9, 2500.0), item(1.5, 3600.0),
        ];
        let (cells, _) = compute(&items, &Params { width: 600, target_h: 150.0, margin: 0 });
        for (i, a) in cells.iter().enumerate() {
            for b in cells.iter().skip(i + 1) {
                let overlap = a.x < b.x + b.w && b.x < a.x + a.w
                    && a.y < b.y + b.h && b.y < a.y + a.h;
                assert!(!overlap, "cells {} and {} overlap", i, i + 1 + cells.iter().position(|c| c == b).unwrap_or(0));
            }
        }
    }

    #[test]
    fn uniform_items_stay_near_the_target_height() {
        let items = vec![item(1.0, 100.0); 24];
        let (cells, _) = compute(&items, &Params { width: 800, target_h: 100.0, margin: 0 });
        for c in &cells {
            assert!((c.h as f32 - 100.0).abs() <= 25.0);
        }
    }

    #[test]
    fn strips_keep_each_images_aspect_ratio() {
        let aspects = [0.5f32, 1.0, 1.5, 2.0, 0.8, 1.25];
        let items: Vec<_> = (0..48)
            .map(|i| item(aspects[i % aspects.len()], 1000.0))
            .collect();
        let (cells, _) = compute(&items, &Params { width: 1200, target_h: 120.0, margin: 0 });
        for (cell, item) in cells.iter().zip(&items) {
            let actual = cell.w as f32 / cell.h as f32;
            let expected = clamped_aspect(item.aspect);
            assert!(
                (actual / expected - 1.0).abs() < 0.12,
                "cell aspect {:.3}, expected {:.3}",
                actual,
                expected
            );
        }
    }

    #[test]
    fn mixed_aspects_make_more_than_one_strip_height() {
        let items = vec![
            item(0.5, 100.0), item(0.5, 100.0), item(0.5, 100.0),
            item(3.0, 100.0), item(3.0, 100.0),
            item(1.0, 100.0), item(1.0, 100.0), item(1.0, 100.0),
        ];
        let (cells, _) = compute(&items, &Params { width: 900, target_h: 150.0, margin: 0 });
        let heights: std::collections::HashSet<i32> = cells.iter().map(|c| c.h).collect();
        assert!(heights.len() > 1, "all strips had the same height");
    }

    #[test]
    fn rows_wrap_and_keep_a_uniform_height() {
        let items = vec![item(1.0, 100.0); 10];
        let p = Params { width: 450, target_h: 100.0, margin: 0 };
        let (cells, total_h) = compute_rows(&items, &p);
        assert_eq!(cells.len(), items.len());
        assert!(cells.iter().all(|c| c.h == 100));
        assert!(cells.iter().any(|c| c.y > 0), "rows did not wrap");
        assert!(total_h >= 200);
        for c in &cells {
            assert!(c.x >= 0 && c.x + c.w <= p.width);
        }
    }

    #[test]
    fn rows_do_not_overlap() {
        let items: Vec<_> = (0..40)
            .map(|i| item(0.6 + (i % 5) as f32 * 0.4, 1000.0))
            .collect();
        let (cells, _) = compute_rows(&items, &Params { width: 640, target_h: 120.0, margin: 4 });
        for (i, a) in cells.iter().enumerate() {
            for b in cells.iter().skip(i + 1) {
                let overlap = a.x < b.x + b.w && b.x < a.x + a.w
                    && a.y < b.y + b.h && b.y < a.y + a.h;
                assert!(!overlap, "row cells {} and {} overlap", i, i + 1);
            }
        }
    }
}
