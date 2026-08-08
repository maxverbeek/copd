//! Lung geometry: the silhouette, the airway tree, and the grid mask derived
//! from them. Pure functions of (col, row) — no game state, no rendering.

use crate::{COLS, ROWS};

/// Distance from point p to segment a-b, in grid cells.
pub(crate) fn dist_to_segment(p: [f32; 2], a: [f32; 2], b: [f32; 2]) -> f32 {
    let (abx, aby) = (b[0] - a[0], b[1] - a[1]);
    let (apx, apy) = (p[0] - a[0], p[1] - a[1]);
    let len2 = abx * abx + aby * aby;
    let t = if len2 > 0.0 { ((apx * abx + apy * aby) / len2).clamp(0.0, 1.0) } else { 0.0 };
    let (dx, dy) = (apx - abx * t, apy - aby * t);
    (dx * dx + dy * dy).sqrt()
}

/// Airway tree in normalised lung space: trachea down the midline, carina split
/// into two main bronchi, then one generation of lobar branches per side.
/// Each entry is (start, end, radius) with radius tapering by generation.
/// Radii are in normalised-x units; ~0.025 is one cell, so these are 1-3 cells
/// wide. Anything thinner aliases into speckle on a grid this coarse.
pub(crate) const AIRWAYS: [([f32; 2], [f32; 2], f32); 9] = [
    // main bronchi from the carina, angling down and out
    ([0.0, -0.58], [-0.32, -0.20], 0.062),
    ([0.0, -0.58], [0.32, -0.20], 0.062),
    // upper lobar branches
    ([-0.24, -0.28], [-0.44, -0.38], 0.048),
    ([0.24, -0.28], [0.44, -0.38], 0.048),
    // descending branches into the lower lobes, kept inboard so they run
    // through tissue instead of eating the outer edge
    ([-0.28, -0.22], [-0.34, 0.42], 0.050),
    ([0.28, -0.22], [0.34, 0.42], 0.050),
    // outward segmental twigs
    ([-0.32, 0.12], [-0.56, 0.22], 0.042),
    ([0.32, 0.12], [0.56, 0.22], 0.042),
    ([0.33, 0.34], [0.50, 0.44], 0.040),
];

/// Reflect a ball off a block, on the axis of shallowest penetration, and push
/// it back out to the contact surface.
///
/// Both the live physics and the x-ray prediction call this. They used to carry
/// separate copies of the rule, which is how they silently drifted apart — the

/// Trachea: solid tissue down the midline, from the top edge to the carina
/// where it splits. Drawn rather than carved — it's the one airway you see.
pub(crate) fn is_trachea(p: [f32; 2]) -> bool {
    dist_to_segment(p, [0.0, -1.0], [0.0, -0.60]) < 0.048
}

/// Normalised grid coords for a cell: x and y both in [-1, 1].
pub(crate) fn cell_pos(col: usize, row: usize) -> [f32; 2] {
    [
        (col as f32 - (COLS as f32 - 1.0) / 2.0) / (COLS as f32 / 2.0),
        (row as f32 - (ROWS as f32 - 1.0) / 2.0) / (ROWS as f32 / 2.0),
    ]
}

/// True where a lung cell should exist: inside a lobe silhouette and not
/// carved out by an airway.
pub(crate) fn in_lung(col: usize, row: usize) -> bool {
    let p = cell_pos(col, row);
    let (nx, ny) = (p[0], p[1]);
    let side = nx.signum();
    let ax = nx.abs();

    if is_trachea(p) {
        return true;
    }

    // Mediastinum: central gap housing heart and great vessels.
    if ax < 0.10 {
        return false;
    }

    // Lobe body. Apex is narrow and inboard, base wide and rounded, so width
    // grows with depth — that taper is what reads as "lung".
    let apex = -0.86;
    let base = 0.96;
    if ny < apex || ny > base {
        return false;
    }
    let depth = (ny - apex) / (base - apex); // 0 at apex, 1 at base
    let half_width = 0.22 + 0.70 * (depth * 1.30).min(1.0).powf(0.50);
    let inner = 0.10 + 0.09 * (1.0 - depth);
    if ax > half_width || ax < inner {
        return false;
    }

    // Rounded costophrenic angle: clip the bottom outer corner.
    let from_base = (base - ny) / (base - apex);
    if from_base < 0.14 && ax > half_width - (0.14 - from_base) * 3.8 {
        return false;
    }

    // Cardiac notch: the heart displaces the lower-inner left lobe. Viewed from
    // the front (standard anatomical view), the patient's left lung is on
    // screen-left, so the notch goes on side < 0.
    if side < 0.0 {
        let d = ((ax - 0.14) / 0.26).powi(2) + ((ny - 0.46) / 0.42).powi(2);
        if d < 1.0 {
            return false;
        }
    }

    // Carve the airway tree out of the tissue.
    !AIRWAYS.iter().any(|(a, b, r)| dist_to_segment(p, *a, *b) < *r)
}
