//! Gap-free "mosaic" layout for the gallery.
//!
//! Images are packed into small slicing trees ("blocks" of up to MAX_BLOCK
//! images). Every leaf keeps its exact aspect ratio, so nothing is cropped or
//! padded. Instead of forcing cells, the search picks the tree whose leaf sizes
//! best follow each image's size weight, and blocks are stacked down the page.

use std::cmp::Ordering;

#[derive(Clone, Copy, Debug)]
pub struct Cell {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

#[derive(Clone, Copy, Debug)]
pub struct Item {
    pub aspect: f32,
    /// Original pixel area. <= 1.0 means "unknown" (folders etc.) and gets weight 1.
    pub area: f32,
    pub height: f32,
    pub seed: u64,
    pub flexible: bool,
}

pub struct Params {
    pub width: i32,
    pub target_h: f32,
    pub margin: i32,
}

// ---- tuning knobs -----------------------------------------------------------
/// Largest group of images solved together as one tree. Cost grows quickly.
const MAX_BLOCK: usize = 8;
/// Candidate trees kept per image range.
const BEAM: usize = 4;
/// 0.0 = every image drawn the same size, 1.0 = drawn area proportional to
/// original pixel area. In between keeps relative sizes without extremes.
const SIZE_BIAS: f32 = 0.5;
const W_MIN: f32 = 0.5;
const W_MAX: f32 = 2.5;
/// Penalty weight for leaves not matching their size weight inside a block.
const SHAPE: f32 = 1.0;
/// Penalty weight for a block's average size drifting from the target size.
const SCALE: f32 = 0.6;
/// Aspects beyond this are clamped for layout (the UI crops those few).
const MIN_ASPECT: f32 = 0.25;
const MAX_ASPECT: f32 = 4.5;
// -----------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Kind {
    Leaf(usize),
    V,
    H,
}

/// A slicing tree. Its width at height h is `a * h + b`.
///
/// `n`, `s1`, `s2` are statistics of ln(leaf_area / leaf_weight) when the block
/// is rendered at height 1. They let us score a tree in O(1) when joining two
/// subtrees, without walking it.
#[derive(Clone)]
struct Block {
    kind: Kind,
    kids: Vec<Block>,
    a: f32,
    b: f32,
    n: f32,
    s1: f32,
    s2: f32,
}

struct Stats {
    a: f32,
    b: f32,
    n: f32,
    s1: f32,
    s2: f32,
}

fn var_of(n: f32, s1: f32, s2: f32) -> f32 {
    let mu = s1 / n;
    (s2 / n - mu * mu).max(0.0)
}

/// Shift every ln(area) value by d: returns the new (s1, s2).
fn shift(n: f32, s1: f32, s2: f32, d: f32) -> (f32, f32) {
    (s1 + n * d, s2 + 2.0 * d * s1 + n * d * d)
}

fn leaf_block(i: usize, aspect: f32, weight: f32) -> Block {
    // At height 1 the leaf is `aspect` wide, so its area is `aspect`.
    let l = (aspect / weight).ln();
    Block { kind: Kind::Leaf(i), kids: Vec::new(), a: aspect, b: 0.0, n: 1.0, s1: l, s2: l * l }
}

/// Stats of joining two blocks, without building the tree. `w` is the page
/// width, used as the reference width when stacking vertically.
fn join_stats(l: &Block, r: &Block, horiz: bool, m: f32, w: f32) -> Stats {
    let n = l.n + r.n;
    if horiz {
        // Side by side: same height, so stats at height 1 just add.
        Stats { a: l.a + r.a, b: l.b + r.b + m, n, s1: l.s1 + r.s1, s2: l.s2 + r.s2 }
    } else {
        // Stacked: same width. Convert each child's stats from height 1 to the
        // height it has at width w, add, then convert the whole to height 1.
        let a = 1.0 / (1.0 / l.a + 1.0 / r.a);
        let b = a * (l.b / l.a + r.b / r.a - m);
        let hl = ((w - l.b) / l.a).max(1.0);
        let hr = ((w - r.b) / r.a).max(1.0);
        let hv = hl + hr + m;
        let (l1, l2) = shift(l.n, l.s1, l.s2, 2.0 * hl.ln());
        let (r1, r2) = shift(r.n, r.s1, r.s2, 2.0 * hr.ln());
        let (s1, s2) = shift(n, l1 + r1, l2 + r2, -2.0 * hv.ln());
        Stats { a, b, n, s1, s2 }
    }
}

fn join(l: &Block, r: &Block, horiz: bool, m: f32, w: f32) -> Block {
    let s = join_stats(l, r, horiz, m, w);
    Block {
        kind: if horiz { Kind::H } else { Kind::V },
        kids: vec![l.clone(), r.clone()],
        a: s.a,
        b: s.b,
        n: s.n,
        s1: s.s1,
        s2: s.s2,
    }
}

fn asp(items: &[Item], k: usize) -> f32 {
    let a = items[k].aspect;
    if a.is_finite() && a > 0.0 { a.clamp(MIN_ASPECT, MAX_ASPECT) } else { 1.0 }
}

fn weights(items: &[Item]) -> Vec<f32> {
    let known = |a: f32| a.is_finite() && a > 1.0;
    let mut areas: Vec<f32> = items.iter().map(|i| i.area).filter(|&a| known(a)).collect();
    areas.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));
    let med = areas.get(areas.len() / 2).copied().unwrap_or(1.0);
    items
        .iter()
        .map(|i| {
            if known(i.area) {
                (i.area / med).powf(SIZE_BIAS).clamp(W_MIN, W_MAX)
            } else {
                1.0
            }
        })
        .collect()
}

/// table[lo][len - 1] = best few trees for items lo..lo+len.
fn build_table(leaves: &[Block], m: f32, w: f32) -> Vec<Vec<Vec<Block>>> {
    let n = leaves.len();
    let mut t: Vec<Vec<Vec<Block>>> = leaves.iter().map(|l| vec![vec![l.clone()]]).collect();

    for len in 2..=MAX_BLOCK {
        for lo in 0..n {
            if lo + len > n {
                break;
            }
            // (score, split, horizontal, left idx, right idx)
            let mut cands: Vec<(f32, usize, bool, usize, usize)> = Vec::new();
            for k in 1..len {
                let left = &t[lo][k - 1];
                let right = &t[lo + k][len - k - 1];
                for (ia, l) in left.iter().enumerate() {
                    for (ib, r) in right.iter().enumerate() {
                        for horiz in [true, false] {
                            let s = join_stats(l, r, horiz, m, w);
                            cands.push((var_of(s.n, s.s1, s.s2), k, horiz, ia, ib));
                        }
                    }
                }
            }
            cands.sort_by(|x, y| x.0.partial_cmp(&y.0).unwrap_or(Ordering::Equal));
            let best: Vec<Block> = cands
                .iter()
                .take(BEAM)
                .map(|&(_, k, horiz, ia, ib)| {
                    join(&t[lo][k - 1][ia], &t[lo + k][len - k - 1][ib], horiz, m, w)
                })
                .collect();
            t[lo].push(best);
        }
    }
    t
}

/// Cost of using `b` as a full-width block on a page `w` wide.
fn block_cost(b: &Block, w: f32, ln_h0: f32, last: bool) -> f32 {
    let hb = ((w - b.b) / b.a).max(1.0);
    // mean of ln(area / weight) at the real size
    let mean = b.s1 / b.n + 2.0 * hb.ln();
    let mut off = mean - 2.0 * ln_h0;
    // The final block may be shrunk instead of stretched to full width.
    if last && off > 0.0 {
        off = 0.0;
    }
    SHAPE * var_of(b.n, b.s1, b.s2) + SCALE * off * off
}

fn place(b: &Block, x: i32, y: i32, w: i32, h: i32, m: i32, out: &mut [Cell]) {
    match b.kind {
        Kind::Leaf(i) => out[i] = Cell { x, y, w: w.max(1), h: h.max(1) },
        Kind::H => {
            let k = b.kids.len();
            let ws: Vec<f32> = b.kids.iter().map(|c| c.a * h as f32 + c.b).collect();
            let total = ws.iter().sum::<f32>().max(1e-3);
            let avail = (w - (k as i32 - 1) * m).max(k as i32);
            let f = avail as f32 / total;
            let mut cum = 0.0f32;
            for (idx, c) in b.kids.iter().enumerate() {
                let wf = ws[idx] * f;
                let left = cum.round() as i32;
                let right = if idx + 1 == k { avail } else { (cum + wf).round() as i32 };
                place(c, x + left + idx as i32 * m, y, (right - left).max(1), h, m, out);
                cum += wf;
            }
        }
        Kind::V => {
            let k = b.kids.len();
            let hs: Vec<f32> = b.kids.iter().map(|c| (w as f32 - c.b) / c.a).collect();
            let total = hs.iter().sum::<f32>().max(1e-3);
            let avail = (h - (k as i32 - 1) * m).max(k as i32);
            let f = avail as f32 / total;
            let mut cum = 0.0f32;
            for (idx, c) in b.kids.iter().enumerate() {
                let hf = hs[idx] * f;
                let top = cum.round() as i32;
                let bottom = if idx + 1 == k { avail } else { (cum + hf).round() as i32 };
                place(c, x, y + top + idx as i32 * m, w, (bottom - top).max(1), m, out);
                cum += hf;
            }
        }
    }
}

pub fn compute(items: &[Item], p: &Params) -> (Vec<Cell>, i32) {
    let m = p.margin.max(0);
    let n = items.len();
    if n == 0 {
        return (Vec::new(), m);
    }
    let mf = m as f32;
    let w_inner = (p.width - 2 * m).max(1) as f32;
    let ln_h0 = p.target_h.max(1.0).ln();

    let wt = weights(items);
    let leaves: Vec<Block> = (0..n).map(|i| leaf_block(i, asp(items, i), wt[i])).collect();
    let table = build_table(&leaves, mf, w_inner);

    // Cut the item list into blocks (DP over block boundaries).
    let mut best = vec![f32::INFINITY; n + 1];
    let mut prev = vec![(0usize, 0usize); n + 1]; // (block start, beam index)
    best[0] = 0.0;
    for q in 1..=n {
        for len in 1..=MAX_BLOCK.min(q) {
            let p0 = q - len;
            if !best[p0].is_finite() {
                continue;
            }
            for (bi, blk) in table[p0][len - 1].iter().enumerate() {
                // weight by item count so big blocks aren't favoured or punished
                let c = block_cost(blk, w_inner, ln_h0, q == n) * blk.n;
                if best[p0] + c < best[q] {
                    best[q] = best[p0] + c;
                    prev[q] = (p0, bi);
                }
            }
        }
        if !best[q].is_finite() {
            best[q] = best[q - 1] + 1.0e3;
            prev[q] = (q - 1, 0);
        }
    }

    let mut blocks: Vec<(usize, usize, usize)> = Vec::new();
    let mut q = n;
    while q > 0 {
        let (p0, bi) = prev[q];
        blocks.push((p0, q, bi));
        q = p0;
    }
    blocks.reverse();

    let mut out = vec![Cell { x: 0, y: 0, w: 1, h: 1 }; n];
    let mut y = m;
    for (p0, q, bi) in blocks {
        let blk = &table[p0][q - p0 - 1][bi];
        let mut wb = w_inner;
        let mut hb = ((wb - blk.b) / blk.a).max(1.0);

        if q == n {
            // Last block: if full width would blow it up past target size,
            // draw it at target size and leave the right side empty.
            let mean = blk.s1 / blk.n + 2.0 * hb.ln();
            if mean > 2.0 * ln_h0 {
                hb = (ln_h0 - blk.s1 / (2.0 * blk.n)).exp().max(1.0);
                wb = blk.a * hb + blk.b;
            }
        }

        let wi = (wb.round() as i32).max(1);
        let hi = (hb.round() as i32).max(1);
        place(blk, m, y, wi, hi, m, &mut out);
        y += hi + m;
    }
    (out, y)
}