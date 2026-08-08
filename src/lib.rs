use wasm_bindgen::prelude::*;
use web_sys::WebGl2RenderingContext as Gl;

mod lung;
use lung::{cell_pos, in_lung, is_trachea};

/// Playfield size. Portrait, because the game is played on phones as often as
/// desktops and a 4:3 board can only ever fill a third of a portrait screen.
/// The lung grid is centred in it; the extra height below is travel space.
pub const W: f32 = 600.0;
pub const H: f32 = 800.0;

const COLS: usize = 40;
const ROWS: usize = 24;
const CELL: f32 = 13.0;
const GRID_X: f32 = (W - COLS as f32 * CELL) / 2.0;
const GRID_Y: f32 = 70.0;

const CIG_W: f32 = 90.0;
const CIG_H: f32 = 12.0;
const CIG_Y: f32 = H - 40.0;
const BALL_R: f32 = 7.0;
const BURN_TIME: f32 = 0.45;

/// Per-instance vertex attributes. Layout is a contract with the shader's
/// vertexAttribPointer offsets — reordering these silently corrupts geometry.
#[repr(C)]
#[derive(Clone, Copy)]
struct Instance {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    kind: f32,
    /// 0 for intact, 0->1 while burning. Its own attribute rather than packed
    /// into `kind`: an interpolated float can land exactly on the next integer
    /// and fall through to the wrong branch.
    burn: f32,
}

const FLOATS_PER_INSTANCE: usize = 6;

/// Lung cells. `span` is how many grid cells wide/tall this block covers;
/// merged (COPD) blocks have span > 1 and their absorbed cells are dead.
#[derive(Clone, Copy)]
struct Block {
    alive: bool,
    span: u8,
    airway: bool,
    /// Seconds left of the burn-up animation. Set on hit; the block keeps
    /// rendering until it reaches zero, then stops being emitted.
    burn: f32,
    /// What `burn` started at, so progress is burn/burn_full regardless of how
    /// the duration was chosen (a staggered collapse uses longer values).
    burn_full: f32,
}

/// What a falling pickup does when the cigarette catches it. One at a time on
/// screen, so no scheduling — catching resolves immediately.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    /// A carton of cigarettes. Catching it advances the COPD in the lungs:
    /// alveoli merge into big fragile sacs.
    Cigarettes,
    /// A pneumothorax: collapses an entire lobe at once.
    Pneumothorax,
    /// An asbestos fibre: needle-shaped, so the ball pierces instead of
    /// bouncing.
    Asbestos,
    /// A radiologist: images the lung, revealing the ball's path to the bottom.
    Radiologist,
}

impl Kind {
    /// The `kind` float the shader switches on. Kept away from the tissue
    /// values (0,1,5,6) and the cig/ball (2,3).
    fn sprite(self) -> f32 {
        match self {
            Kind::Cigarettes => 10.0,
            Kind::Pneumothorax => 11.0,
            Kind::Asbestos => 12.0,
            Kind::Radiologist => 13.0,
        }
    }

    /// Player-facing name, shown as a label on the falling pickup.
    fn name(self) -> &'static str {
        match self {
            Kind::Cigarettes => "cigarettes",
            Kind::Pneumothorax => "pneumothorax",
            Kind::Asbestos => "asbestos",
            Kind::Radiologist => "radiologist",
        }
    }

    /// One-line description of what catching it does.
    fn blurb(self) -> &'static str {
        match self {
            Kind::Cigarettes => "alveoli merge into fragile sacs",
            Kind::Pneumothorax => "collapses a whole lung",
            Kind::Asbestos => "the ball cuts straight through",
            Kind::Radiologist => "reveals the ball's path",
        }
    }

    fn size(self) -> (f32, f32) {
        match self {
            Kind::Cigarettes => (40.0, 30.0),
            Kind::Pneumothorax => (34.0, 34.0),
            Kind::Asbestos => (40.0, 24.0),
            Kind::Radiologist => (34.0, 34.0),
        }
    }

    fn fall_speed(self) -> f32 {
        match self {
            Kind::Cigarettes => 120.0,
            Kind::Pneumothorax => 150.0,
            Kind::Asbestos => 190.0,
            Kind::Radiologist => 135.0,
        }
    }

    /// Spawn weight. Pneumothorax only starts appearing once the lungs are
    /// already diseased, so early drops are the milder two.
    /// Spawn weight at a given scatter level (0 = intact lung, 1 = every
    /// remaining cell exposed).
    ///
    /// Early on the drops are the two that break tissue up — cigarettes to merge
    /// alveoli into fat targets, asbestos to cut channels. Those are what make
    /// the opening satisfying, and they are all the player gets. As the lung
    /// fragments the tools that help you finish take over: the radiologist to
    /// find the survivors, and rarely a pneumothorax to clear a whole lobe.
    ///
    /// Weights are ramped rather than switched at a threshold, so the mix drifts
    /// instead of flipping — a sudden change in what drops reads as a bug.
    fn weight(self, scatter: f32) -> u32 {
        // 0 at scatter <= 0.55, easing to 1 by 0.95: the back half of a game.
        let late = ((scatter - 0.55) / 0.40).clamp(0.0, 1.0);
        let lerp = |a: f32, b: f32| (a + (b - a) * late).round() as u32;
        match self {
            // 65/35 between them at the start, per the opening mix.
            Kind::Cigarettes => lerp(65.0, 10.0),
            Kind::Asbestos => lerp(35.0, 15.0),
            Kind::Radiologist => lerp(0.0, 40.0),
            // Rare even at its peak, and never an opening drop: collapsing a
            // lobe is a bigger swing than anything else in the game.
            Kind::Pneumothorax => lerp(0.0, 8.0),
        }
    }
}

const KINDS: [Kind; 4] =
    [Kind::Cigarettes, Kind::Pneumothorax, Kind::Asbestos, Kind::Radiologist];

/// Paddle bounces the predicted trajectory survives. One means "shows you the
/// path down to the paddle, then clears when you catch it".
const RADIOLOGIST_BOUNCES: u32 = 1;

/// Paddle bounces an asbestos fibre keeps the ball piercing. Counted in bounces
/// rather than blocks so one fibre cuts a whole channel per pass.
const PIERCE_BOUNCES: u32 = 4;

/// Consecutive paddle bounces without breaking a block before a radiologist is
/// sent. Two is the point where a rally has visibly stopped going anywhere —
/// the endgame's failure mode is fishing for scattered survivors, so the help
/// arrives exactly when the fishing starts.
const BARREN_BOUNCES: u32 = 2;

/// Hard cap on trail dots, so a long path can never overflow the instance
/// buffer the GPU side was sized for.
const MAX_TRAIL_DOTS: usize = 512;

/// Blocks destroyed per pickup drop. Only hits that could produce a drop count
/// toward it — see `tick` — so this really is one pickup per this many blocks,
/// not an upper bound the game rarely reaches.
const SPAWN_EVERY: u32 = 6;

/// Gap between x-ray trail dots. Fixed, never scaled to fit the budget: a path
/// with more dots than the budget allows is drawn as far as it reaches and then
/// stops, rather than being re-spaced to span the whole prediction.
const SPACING_PX: f32 = 15.0;

/// Thickness of the pleura lining the chest wall. The ball bounces off its
/// inner surface, so this is the playfield boundary, not decoration.
const PLEURA: f32 = 14.0;

/// Playfield bounds for the ball's centre. The pleura is the surface the ball
/// hits, not decoration behind an invisible wall — these must stay derived
/// from PLEURA so the drawn membrane and the physics agree.
const WALL_L: f32 = PLEURA + BALL_R;
const WALL_R: f32 = W - PLEURA - BALL_R;
const WALL_TOP: f32 = PLEURA + BALL_R;

#[derive(Clone, Copy)]
struct Pickup {
    x: f32,
    y: f32,
    kind: Kind,
}

pub struct Game {
    blocks: Vec<Block>,
    ball: [f32; 4],
    cig_x: f32,
    pickup: Option<Pickup>,
    /// Paddle bounces remaining while the ball pierces instead of reflecting.
    /// Counted in bounces, not block hits — a fibre cuts a whole channel per
    /// pass, so limiting it by blocks would stop it mid-flight.
    pierce: u32,
    /// Paddle bounces left showing the predicted trajectory.
    radiologist: u32,
    /// Consecutive paddle returns that broke nothing, and whether the current
    /// rally has broken anything yet. Together these send a radiologist once
    /// the ball is visibly fishing rather than clearing.
    barren: u32,
    broke_since_bounce: bool,
    /// Scratch buffers for `predict`, kept alive across frames. WASM can grow
    /// its heap freely, but every growth detaches the Float32Array view JS
    /// holds over linear memory — so the hot path allocates nothing.
    path: Vec<[f32; 2]>,
    broken: Vec<usize>,
    /// Blocks struck during the current tick's sweep. Same no-allocation rule
    /// as `path`/`broken`: reused across frames.
    hits: Vec<usize>,
    /// Deterministic PRNG — Math.random would mean a boundary crossing per call
    /// and non-reproducible bugs.
    rng: u32,
    spawn_counter: u32,
    /// Most recently caught pickup and a catch counter, for the announcement
    /// overlay. The counter is what JS watches — a flag could be missed.
    caught: Option<Kind>,
    caught_seq: u32,
    /// Diagnostic counters, surfaced in the HUD.
    bounces: u32,
    asbestos_caught: u32,
    pub copd: f32,
    pub lost: bool,
    /// Latched once the last block is destroyed. Separate from a live scan of
    /// `blocks` so the end state can't flicker while the final cells burn.
    pub won: bool,
    /// Live count of intact blocks, maintained incrementally. Scanning 960
    /// blocks per frame just to ask "are we done" is wasteful when every
    /// removal already passes through `ignite`.
    alive_count: usize,
    /// Live cells at kickoff, so the HUD can report what share of the lung is
    /// left. Fixed for the life of the game.
    starting_alive: usize,
    /// Blocks destroyed, and seconds elapsed — the score shown on victory.
    destroyed: u32,
    elapsed: f32,
    instances: Vec<Instance>,
}

/// Ball state during a bounce: position and velocity, mutated in place.
type Ball = [f32; 4];

/// Deflect a ball off the paddle, if it is in contact with one at `cig_x`.
/// Returns whether it bounced.
///
/// The angle scales with how far off centre the ball lands, so the paddle aims
/// rather than merely reflects. Shared by the live tick and the x-ray's
/// after-the-catch trace, for the same reason `bounce_off_block` is: two copies
/// of a bounce rule drift apart, and then the drawn path lies.
fn bounce_off_paddle(ball: &mut Ball, cig_x: f32) -> bool {
    let [x, y, vx, vy] = *ball;
    // Inclusive at the top face: a swept trace lands the ball exactly on
    // `CIG_Y - BALL_R`, which is contact, not a near miss. A strict test there
    // rejected the very bounce the trace had just solved for.
    if vy <= 0.0 || y + BALL_R < CIG_Y || y - BALL_R >= CIG_Y + CIG_H {
        return false;
    }
    let off = (x - cig_x) / (CIG_W / 2.0);
    if off.abs() >= 1.2 {
        return false;
    }
    let speed = (vx * vx + vy * vy).sqrt();
    // A dead-centre catch returns the ball perfectly vertically, and a vertical
    // ball bounces in the same column forever — it can never reach tissue to
    // either side, so the game stalls with the board half full. Nudge the angle
    // off zero so a return always has some sideways travel.
    const MIN_ANGLE: f32 = 0.06;
    let mut angle = off.clamp(-1.0, 1.0) * 1.05;
    if angle.abs() < MIN_ANGLE {
        angle = MIN_ANGLE.copysign(if angle == 0.0 { 1.0 } else { angle });
    }
    ball[1] = CIG_Y - BALL_R;
    ball[2] = speed * angle.sin();
    ball[3] = -speed * angle.cos();
    true
}

/// Reflect a ball off the face the swept solve says it crossed.
///
/// Both the live physics and the x-ray prediction call this, on an axis both
/// got from the same `next_impact`. Deriving the face here instead — from
/// penetration depth — is what used to make them disagree: on a near-corner hit
/// the two overlaps are within a rounding error of each other, so the live ball
/// would reflect off the side face while the trace had drawn the top, and the
/// drawn path went stale a bounce later.
///
/// No push-out: the caller has already placed the ball exactly on the surface
/// at the moment of contact, so there is no penetration to undo.
fn bounce_off_block(ball: &mut Ball, axis: Axis) {
    match axis {
        Axis::X => ball[2] = -ball[2],
        Axis::Y => ball[3] = -ball[3],
    }
}

/// Which axis a struck face lies on: the ball's velocity reverses on it.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Axis {
    X,
    Y,
}

/// What the ball reaches first, and when.
enum Impact {
    /// A block, by index, plus which axis its struck face lies on. The axis
    /// comes from the swept solve — which slab the ball entered last — not from
    /// penetration depth, so a near-corner hit resolves the same way every time.
    Block(usize, f32, Axis),
    /// A wall: `(flip_x, flip_y)` says which components reverse.
    Wall(bool, bool, f32),
    /// Nothing within `limit` — the ball travels freely for the whole span.
    None,
}

/// Time interval during which `pos + vel*t` lies within [lo, hi].
/// With zero velocity the answer is "always" or "never", depending on whether
/// we already start inside the range.
fn axis_span(pos: f32, vel: f32, lo: f32, hi: f32) -> (f32, f32) {
    if vel.abs() < 1e-6 {
        return if pos >= lo && pos <= hi {
            (f32::NEG_INFINITY, f32::INFINITY)
        } else {
            (f32::INFINITY, f32::NEG_INFINITY) // empty: enter > exit
        };
    }
    let a = (lo - pos) / vel;
    let b = (hi - pos) / vel;
    (a.min(b), a.max(b))
}


impl Game {
    pub fn new() -> Self {
        let mut blocks =
            vec![Block { alive: false, span: 1, airway: false, burn: 0.0, burn_full: 0.0 }; COLS * ROWS];
        for row in 0..ROWS {
            for col in 0..COLS {
                let b = &mut blocks[row * COLS + col];
                b.alive = in_lung(col, row);
                b.airway = is_trachea(cell_pos(col, row));
            }
        }
        // Cull cells left stranded where an airway grazes thin tissue — they
        // read as speckle, not anatomy. One pass is enough at this resolution.
        let seed = blocks.clone();
        for row in 0..ROWS {
            for col in 0..COLS {
                if !seed[row * COLS + col].alive {
                    continue;
                }
                let live = |dx: i32, dy: i32| {
                    let (c, r) = (col as i32 + dx, row as i32 + dy);
                    c >= 0
                        && r >= 0
                        && (c as usize) < COLS
                        && (r as usize) < ROWS
                        && seed[r as usize * COLS + c as usize].alive
                };
                // Needs support on both axes — a cell with only vertical
                // neighbours is a one-wide spike sticking into an airway.
                let horizontal = live(-1, 0) || live(1, 0);
                let vertical = live(0, -1) || live(0, 1);
                if !(horizontal && vertical) {
                    blocks[row * COLS + col].alive = false;
                }
            }
        }
        // Each cell emits at most one instance (live or burning), plus the
        // paddle, ball, pickup, three pleura strips and the x-ray trail.
        // Reserved once so the Vec never reallocates and the Float32Array
        // view can never detach.
        let starting_alive = blocks.iter().filter(|b| b.alive).count();
        let n = blocks.len() + 6 + MAX_TRAIL_DOTS;
        let mut g = Game {
            blocks,
            ball: [0.0; 4],
            cig_x: W / 2.0,
            pickup: None,
            pierce: 0,
            radiologist: 0,
            barren: 0,
            broke_since_bounce: false,
            path: Vec::with_capacity(256),
            broken: Vec::with_capacity(256),
            hits: Vec::with_capacity(16),
            rng: 0x2545F491,
            spawn_counter: 0,
            caught: None,
            caught_seq: 0,
            won: false,
            alive_count: starting_alive,
            starting_alive,
            destroyed: 0,
            elapsed: 0.0,
            bounces: 0,
            asbestos_caught: 0,
            copd: 0.0,
            lost: false,
            instances: Vec::with_capacity(n),
        };
        g.reset_ball();
        g
    }

    /// Serve from the carina, heading down into the lungs.
    ///
    /// Launching from just above the paddle instead sent the ball up through
    /// the soft underside of both lobes, carving most of a lung before the
    /// player had touched it. Starting at the airway means the ball enters the
    /// way air does — down the trachea, into the bronchi — and has to work
    /// outward from the middle.
    fn reset_ball(&mut self) {
        // The carina is at normalised y = -0.60; `cell_pos` maps rows onto
        // [-1, 1], so invert it to find that row. The trachea above it is solid
        // tissue, so the serve starts on the first clear row below — spawning
        // inside a block would have the sweep resolve a collision from within
        // it, which is the one case the swept solve cannot place correctly.
        let carina_row = ((-0.60 + 1.0) / 2.0 * ROWS as f32 - 0.5).round().max(0.0) as usize;
        let x = W / 2.0;
        let row = (carina_row..ROWS)
            .find(|&r| {
                let y = GRID_Y + r as f32 * CELL + CELL / 2.0;
                !(0..self.blocks.len()).any(|i| {
                    if !self.blocks[i].alive {
                        return false;
                    }
                    let (bx, by, bw, bh) = self.block_rect(i);
                    x + BALL_R > bx && x - BALL_R < bx + bw && y + BALL_R > by && y - BALL_R < by + bh
                })
            })
            .unwrap_or(carina_row);
        let y = GRID_Y + row as f32 * CELL + CELL / 2.0;
        // Slightly off-vertical so the first descent is not a straight drop
        // down the mediastinum, which is a gap all the way to the paddle.
        self.ball = [x, y, 90.0, 300.0];
    }

    fn rand(&mut self) -> u32 {
        // xorshift32
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 17;
        self.rng ^= self.rng << 5;
        self.rng
    }

    fn block_rect(&self, i: usize) -> (f32, f32, f32, f32) {
        let span = self.blocks[i].span as f32;
        (
            GRID_X + (i % COLS) as f32 * CELL,
            GRID_Y + (i / COLS) as f32 * CELL,
            CELL * span,
            CELL * span,
        )
    }

    /// Emphysema: collapse a square patch of live cells into one big fragile
    /// block. Patches grow with disease progression, so late-stage lungs turn
    /// into a few huge sacs — which is what emphysema actually does to alveoli.
    fn merge_alveoli(&mut self) {
        // Span grows with disease. Capped at 4: bigger patches need a clean
        // square of untouched cells, and a fresh lung only has 7 span-6 spots
        // to begin with, so a higher cap just falls through to the smaller
        // spans without ever firing.
        let span = if self.copd > 0.55 { 4 } else if self.copd > 0.25 { 3 } else { 2 };
        for span in (2..=span).rev() {
            // Random probes first: they scatter the sacs around the lung
            // instead of eating it in reading order. But probing is a search
            // that gets worse exactly as the board fragments, so once the
            // random budget is spent, fall back to a deterministic sweep for
            // the first clean patch. Without it a carton silently did nothing
            // on a late-stage lung — damage peaked mid-game and then fell off.
            let probes = 96;
            let sweep = (ROWS - span) * (COLS - span);
            for n in 0..probes + sweep {
                let (col, row) = if n < probes {
                    let r = self.rand() as usize;
                    (r % (COLS - span), (r / COLS) % (ROWS - span))
                } else {
                    let i = n - probes;
                    (i % (COLS - span), i / (COLS - span))
                };
                let patch: Vec<usize> = (0..span)
                    .flat_map(|dy| (0..span).map(move |dx| (row + dy) * COLS + col + dx))
                    .collect();
                // Only untouched tissue merges. Absorbing existing sacs was
                // tried and is not worth it: a patch of mixed spans is only
                // mergeable when its cells tile exactly, which almost never
                // happens once the board is fragmented, so the search froze
                // completely instead of degrading. Single cells always tile.
                if patch.iter().all(|&i| self.blocks[i].alive && self.blocks[i].span == 1) {
                    // Absorbed, not destroyed: they vanish into the big sac
                    // rather than burning, so no `ignite` and no score — but
                    // the live count still has to drop.
                    for &i in &patch[1..] {
                        self.blocks[i].alive = false;
                        self.alive_count -= 1;
                    }
                    self.blocks[patch[0]].span = span as u8;
                    self.copd = (self.copd + 0.05).min(1.0);
                    // The merged sac survives, so this can't reach zero — but
                    // keep the invariant local rather than assumed.
                    if self.alive_count == 0 {
                        self.won = true;
                    }
                    return;
                }
            }
        }
    }

    pub fn set_paddle(&mut self, x: f32) {
        self.cig_x = x.clamp(CIG_W / 2.0, W - CIG_W / 2.0);
    }

    pub fn cleared(&self) -> bool {
        self.alive_count == 0
    }

    pub fn instance_count(&self) -> i32 {
        self.instances.len() as i32
    }

    /// Upload the instance buffer straight from WASM linear memory into
    /// whatever is currently bound to ARRAY_BUFFER.
    pub fn upload(&self, gl: &Gl) {
        let floats = self.instances.len() * FLOATS_PER_INSTANCE;
        // SAFETY: the view aliases WASM linear memory and is invalidated by any
        // allocation. It is consumed by the upload below before anything can
        // allocate, and never stored.
        let view = unsafe {
            js_sys::Float32Array::view(std::slice::from_raw_parts(
                self.instances.as_ptr() as *const f32,
                floats,
            ))
        };
        gl.buffer_sub_data_with_i32_and_array_buffer_view(Gl::ARRAY_BUFFER, 0, &view);
    }

    /// The soonest surface the ball meets within `limit` seconds, ignoring any
    /// block listed in `skip`.
    ///
    /// This is the single source of truth for "what does the ball hit next".
    /// Both the live tick and the x-ray trace call it, so a prediction can only
    /// diverge from reality through the paddle — which the trace deliberately
    /// ignores — and never through a differently-resolved block or wall.
    fn next_impact(&self, ball: &Ball, limit: f32, skip: &[usize]) -> Impact {
        let [x, y, vx, vy] = *ball;

        // Walls. f32::INFINITY when travelling away, so it never wins the min().
        let t_left = if vx < 0.0 { (WALL_L - x) / vx } else { f32::INFINITY };
        let t_right = if vx > 0.0 { (WALL_R - x) / vx } else { f32::INFINITY };
        let t_top = if vy < 0.0 { (WALL_TOP - y) / vy } else { f32::INFINITY };

        let mut best = limit;
        let mut hit = Impact::None;

        // t == 0 is a real hit here, not a stale one: each of these times is
        // only finite when the velocity points *into* that wall, so a ball
        // resting exactly on the surface with inward velocity still has to
        // reflect. Rejecting t == 0 (as the block test rightly does) let such a
        // ball tunnel straight through the apex.
        let t_side = t_left.min(t_right);

        // A corner hit reaches both walls at the same instant, so both
        // components must flip; picking one would send the ball into the
        // membrane it just touched.
        let t_corner = t_side.min(t_top);
        if t_corner >= 0.0 && t_corner < best {
            best = t_corner;
            hit = Impact::Wall(
                (t_side - t_corner).abs() < 1e-4,
                (t_top - t_corner).abs() < 1e-4,
                t_corner,
            );
        }

        // Does a live block get in the way sooner? Slab test per block: the
        // ball's centre crosses the block's x-range during [tx0, tx1] and its
        // y-range during [ty0, ty1]; an overlap means an impact.
        for i in 0..self.blocks.len() {
            if !self.blocks[i].alive || skip.contains(&i) {
                continue;
            }
            let (bx, by, bw, bh) = self.block_rect(i);
            let (x0, x1) = (bx - BALL_R, bx + bw + BALL_R);
            let (y0, y1) = (by - BALL_R, by + bh + BALL_R);

            let (tx0, tx1) = axis_span(x, vx, x0, x1);
            let (ty0, ty1) = axis_span(y, vy, y0, y1);
            let enter = tx0.max(ty0);
            let exit = tx1.min(ty1);
            // The slab entered last is the face actually crossed: if the ball
            // was already inside the x-range when it met the y-range, it came
            // through a horizontal face, and vice versa.
            let axis = if tx0 > ty0 { Axis::X } else { Axis::Y };
            // `enter == 0` means the ball is sitting exactly on this face —
            // which is where the previous tick's sweep left it. That is a real
            // hit, not a stale one, so the entry time is admitted at zero
            // rather than behind an epsilon; rejecting it let the ball slip
            // through the very block it had just landed on and the drawn path
            // went stale from there. What must still be rejected is anything
            // genuinely behind the ball (enter < 0), and a block the ball is
            // already inside and on its way out of (exit <= 0) — both would
            // otherwise drag the sweep backwards along its own velocity.
            if enter <= exit && enter >= 0.0 && exit > 0.0 && enter < best {
                best = enter;
                hit = Impact::Block(i, enter, axis);
            }
        }

        hit
    }

    /// Trace where the ball will go, as a polyline from its current position
    /// down to the bottom edge.
    ///
    /// Segment-stepping, not time-stepping: solve for the soonest impact via
    /// `next_impact`, jump straight to it, reflect, repeat. That's a few dozen
    /// iterations instead of ~200 physics ticks per frame.
    ///
    /// Blocks the trace destroys are recorded in `broken` so the rest of the
    /// path accounts for their absence — otherwise the prediction diverges
    /// from reality at the very first impact.
    ///
    /// The trace holds all the way to the floor, not just for the first bounce
    /// or two: `tick` advances by the same swept solve, so there is no second
    /// collision rule for it to drift away from. The one thing the trace still
    /// ignores is the paddle, which is the player's to move.
    fn predict(&mut self) {
        const MAX_BOUNCES: usize = 160;
        let [mut x, mut y, mut vx, mut vy] = self.ball;
        // Reuse the buffers: clear() keeps the allocation, so no heap growth
        // and no chance of detaching the view built later in the frame.
        let mut path = std::mem::take(&mut self.path);
        let mut broken = std::mem::take(&mut self.broken);
        path.clear();
        broken.clear();
        path.push([x, y]);
        let pierce_left = self.pierce;

        // Set once the trace has been deflected by the paddle. The leg after the
        // catch is a preview of where the ball is being aimed, so it stops at
        // the first thing it meets rather than tracing the whole rally.
        let mut returned = false;

        for _ in 0..MAX_BOUNCES {
            // Time to fall out the bottom, which ends the trace rather than
            // deflecting it — so it bounds the search instead of being a hit.
            let t_floor = if vy > 0.0 { (H + BALL_R - y) / vy } else { f32::INFINITY };

            // Descending toward the paddle, cut the search off at its face: the
            // ball is caught there, not at the floor. Bounding the search rather
            // than testing after the fact keeps a block in front of the paddle
            // winning, which is what really happens.
            //
            // Only when the paddle is actually under the ball, though. Cutting
            // at the plane regardless would end the trace in mid-air on a ball
            // heading for the gap beside the paddle, which is exactly the shot
            // the player most needs to see is going to be missed.
            let t_paddle = if !returned && vy > 0.0 && y < CIG_Y - BALL_R {
                let t = (CIG_Y - BALL_R - y) / vy;
                let off = (x + vx * t - self.cig_x) / (CIG_W / 2.0);
                if off.abs() < 1.2 { t } else { f32::INFINITY }
            } else {
                f32::INFINITY
            };
            let limit = t_floor.min(t_paddle);

            let (t_hit, hit) = match self.next_impact(&[x, y, vx, vy], limit, &broken) {
                Impact::Block(i, t, axis) => (t, Some((i, axis))),
                Impact::Wall(fx, fy, t) => {
                    x += vx * t;
                    y += vy * t;
                    path.push([x, y]);
                    // The apex ends the return leg, same as a block: the shot
                    // has gone as far as it was aimed. A side wall is only a
                    // deflection, so the preview carries on around it.
                    if returned && fy {
                        break;
                    }
                    if fx {
                        vx = -vx;
                    }
                    if fy {
                        vy = -vy;
                    }
                    continue;
                }
                Impact::None => (limit, None),
            };

            if !t_hit.is_finite() {
                break;
            }

            x += vx * t_hit;
            y += vy * t_hit;
            // A zero-length step happens when the ball starts exactly on the
            // face it is about to cross — common while piercing, since nothing
            // moves it off the surface. Emitting a vertex there would leave a
            // duplicate point in the polyline, and a zero-length segment has no
            // direction, so anything reading the path's heading sees a spurious
            // turn. The block is recorded either way, just not drawn twice.
            if t_hit > 0.0 {
                path.push([x, y]);
            }

            if y >= H {
                break;
            }

            if let Some((i, axis)) = hit {
                // After the catch the trace stops at the first tissue it meets:
                // that block is the answer to "where is this shot going", and
                // continuing would draw a whole speculative rally over the lung.
                if returned {
                    break;
                }
                broken.push(i);
                // Same rule as the live physics, via the same function. While
                // piercing the ball cuts through everything; `pierce` is spent
                // at the paddle, not per block, so it doesn't decrement here.
                if pierce_left == 0 {
                    let mut ball = [x, y, vx, vy];
                    bounce_off_block(&mut ball, axis);
                    [x, y, vx, vy] = ball;
                }
                continue;
            }

            // Nothing in the way, so the ball reached whatever bounded the
            // search. At the paddle it gets returned, once; at the floor the
            // rally is over either way.
            let mut ball = [x, y, vx, vy];
            if !returned && bounce_off_paddle(&mut ball, self.cig_x) {
                [x, y, vx, vy] = ball;
                returned = true;
                continue;
            }
            break;
        }
        self.path = path;
        self.broken = broken;
    }

    /// Retire a block into its burn-up animation. `extra` delays the start for
    /// staggered effects; bigger blocks burn longer.
    fn ignite(&mut self, i: usize, extra: f32) {
        if !self.blocks[i].alive {
            return; // already burning; don't double-count
        }
        let full = BURN_TIME * (0.7 + 0.3 * self.blocks[i].span as f32) + extra;
        self.blocks[i].alive = false;
        self.blocks[i].burn = full;
        self.blocks[i].burn_full = full;

        // Every removal funnels through here, so this is the one place the
        // count and the win condition need to be maintained.
        self.alive_count -= 1;
        self.destroyed += 1;
        if self.alive_count == 0 {
            self.won = true;
        }
    }

    /// Pneumothorax: one lung deflates. Every block on the chosen side burns
    /// away at once, staggered by distance from the hilum so it reads as a
    /// collapse spreading outward rather than a single flash.
    fn collapse_lobe(&mut self) {
        // Pick the side with more tissue left, so it always lands as a blow.
        let mid = COLS / 2;
        let count_side = |g: &Game, left: bool| {
            g.blocks
                .iter()
                .enumerate()
                .filter(|(i, b)| b.alive && ((i % COLS) < mid) == left)
                .count()
        };
        let left = count_side(self, true) >= count_side(self, false);

        for i in 0..self.blocks.len() {
            if !self.blocks[i].alive || ((i % COLS) < mid) != left {
                continue;
            }
            let col = (i % COLS) as f32;
            let row = (i / COLS) as f32;
            // Distance from the midline, normalised — outer tissue goes last.
            let d = ((col - mid as f32).abs() / mid as f32).min(1.0);
            self.ignite(i, 0.10 + 0.55 * d + 0.10 * (row / ROWS as f32));
        }
        self.copd = (self.copd + 0.15).min(1.0);
    }

    fn apply(&mut self, kind: Kind) {
        self.caught = Some(kind);
        self.caught_seq += 1;
        match kind {
            Kind::Cigarettes => {
                // Each merge is one patch and +0.05 COPD, and the patches grow
                // as COPD rises — so the count is the damage knob, and the
                // compounding comes for free.
                for _ in 0..5 {
                    self.merge_alveoli();
                }
            }
            Kind::Pneumothorax => self.collapse_lobe(),
            Kind::Asbestos => {
                self.asbestos_caught += 1;
                // Refreshed, not stacked: catching a second fibre restarts the
                // four bounces rather than adding to them, so a lucky run of
                // drops can't leave the ball piercing for most of a game.
                self.pierce = PIERCE_BOUNCES;
            }
            Kind::Radiologist => self.radiologist = RADIOLOGIST_BOUNCES,
        }
    }

    /// Weighted pick over the kinds currently eligible. Uses the game's own
    /// xorshift so spawns stay deterministic and testable.
    /// How broken up the lung is, 0..1: the share of surviving cells with at
    /// least one exposed face.
    ///
    /// An intact lung sits near 0.47 — its outer rim is exposed by definition —
    /// and it climbs to 1.0 once nothing has a neighbour left. That is the shape
    /// the pickup schedule wants: it rises steadily as the board breaks up.
    ///
    /// Exposure rather than survivor count, because the two disagree in the
    /// case that matters: cigarettes cut `alive_count` hard — four cells become
    /// one — while only modestly opening the lung up. Counting survivors would
    /// read a carton as most of a game's worth of progress toward the endgame
    /// drops. Cartons do raise this too, since a merge leaves a hole behind,
    /// but in proportion to the tissue they actually expose.
    ///
    /// Walked on demand instead of maintained incrementally: it is only needed
    /// when a pickup spawns, so a merge or a lobe collapse never has to keep it
    /// in step.
    fn scatter(&self) -> f32 {
        let mut alive = 0;
        let mut exposed = 0;
        for i in 0..self.blocks.len() {
            if !self.blocks[i].alive {
                continue;
            }
            alive += 1;
            let (c, r) = ((i % COLS) as i32, (i / COLS) as i32);
            // A cell counts as exposed if any of its four faces meets a gap —
            // including the grid edge, which is as good as a gap to the ball.
            let open = [(0, 1), (0, -1), (1, 0), (-1, 0)].iter().any(|&(dc, dr)| {
                let (cc, rr) = (c + dc, r + dr);
                cc < 0
                    || rr < 0
                    || cc as usize >= COLS
                    || rr as usize >= ROWS
                    || !self.blocks[rr as usize * COLS + cc as usize].alive
            });
            if open {
                exposed += 1;
            }
        }
        if alive == 0 {
            return 1.0;
        }
        exposed as f32 / alive as f32
    }

    fn roll_kind(&mut self) -> Option<Kind> {
        let scatter = self.scatter();
        let total: u32 = KINDS.iter().map(|k| k.weight(scatter)).sum();
        if total == 0 {
            return None;
        }
        let mut n = self.rand() % total;
        for k in KINDS {
            let w = k.weight(scatter);
            if n < w {
                return Some(k);
            }
            n -= w;
        }
        None
    }

    pub fn tick(&mut self, dt: f32) {
        let dt = dt.min(0.05);

        // Burns keep running after the game ends, so the final collapse plays
        // out instead of freezing mid-animation.
        for b in &mut self.blocks {
            if b.burn > 0.0 {
                b.burn = (b.burn - dt).max(0.0);
            }
        }

        if self.lost || self.won {
            return;
        }
        self.elapsed += dt;

        if let Some(mut p) = self.pickup {
            p.y += p.kind.fall_speed() * dt;
            let (pw, ph) = p.kind.size();
            let caught = p.y + ph / 2.0 > CIG_Y
                && p.y - ph / 2.0 < CIG_Y + CIG_H
                && (p.x - self.cig_x).abs() < CIG_W / 2.0 + pw / 2.0;
            if caught {
                self.apply(p.kind);
                self.pickup = None;
            } else if p.y - ph / 2.0 > H {
                self.pickup = None;
            } else {
                self.pickup = Some(p);
            }
        }

        let [mut x, mut y, mut vx, mut vy] = self.ball;

        // Swept advance: consume `dt` by jumping to each impact in turn rather
        // than stepping blindly and cleaning up penetration afterwards. This is
        // the same solve the x-ray runs, so what the trace drew is what happens
        // — a stepped ball bounces from wherever it ended up *inside* a block,
        // which shifts every subsequent bounce and is what made the prediction
        // need recalculating after a few hops.
        let mut hits: Vec<usize> = std::mem::take(&mut self.hits);
        hits.clear();
        let mut left = dt;
        // Each iteration consumes time at an impact; the cap is a guard against
        // a degenerate corner trapping the ball, not an expected limit.
        for _ in 0..16 {
            if left <= 0.0 {
                break;
            }
            match self.next_impact(&[x, y, vx, vy], left, &hits) {
                Impact::Wall(fx, fy, t) => {
                    x += vx * t;
                    y += vy * t;
                    if fx {
                        vx = -vx;
                    }
                    if fy {
                        vy = -vy;
                    }
                    left -= t;
                }
                Impact::Block(i, t, axis) => {
                    x += vx * t;
                    y += vy * t;
                    left -= t;
                    hits.push(i);
                    if self.pierce == 0 {
                        let mut ball = [x, y, vx, vy];
                        bounce_off_block(&mut ball, axis);
                        [x, y, vx, vy] = ball;
                    }
                }
                Impact::None => {
                    x += vx * left;
                    y += vy * left;
                    break;
                }
            }
        }

        let mut ball = [x, y, vx, vy];
        if bounce_off_paddle(&mut ball, self.cig_x) {
            [x, y, vx, vy] = ball;
            // Asbestos lasts a fixed number of paddle bounces, so one
            // fibre cuts several full channels through the lung.
            self.bounces += 1;
            self.pierce = self.pierce.saturating_sub(1);
            // The radiologist's job is done once you've caught the ball it
            // was pointing at.
            self.radiologist = self.radiologist.saturating_sub(1);

            // A rally that returns the ball without breaking anything is the
            // endgame's failure mode: a handful of scattered survivors and no
            // way to see where the ball is going. Two barren returns in a row
            // and a radiologist is sent, so the help arrives exactly when the
            // fishing starts rather than being left to the drop table.
            if self.broke_since_bounce {
                self.barren = 0;
            } else {
                self.barren += 1;
            }
            self.broke_since_bounce = false;
            if self.barren >= BARREN_BOUNCES && self.pickup.is_none() {
                self.barren = 0;
                self.pickup = Some(Pickup {
                    x: self.cig_x.clamp(40.0, W - 40.0),
                    y: GRID_Y,
                    kind: Kind::Radiologist,
                });
            }
        }

        // Retire whatever the sweep ran into. The reflection already happened
        // above, at the true contact point; this is only the consequences.
        for idx in 0..hits.len() {
            let i = hits[idx];
            let (bx, by, bw, _) = self.block_rect(i);
            self.ignite(i, 0.0);
            self.broke_since_bounce = true;

            // Only count hits that could actually produce a drop. Counting
            // them all meant every block broken while a pickup was already
            // falling threw its progress away, so a game yielded ~12 pickups
            // instead of one per `SPAWN_EVERY` — the schedule barely got to
            // express itself.
            if self.pickup.is_none() {
                self.spawn_counter += 1;
                if self.spawn_counter % SPAWN_EVERY == 0 {
                    if let Some(kind) = self.roll_kind() {
                        self.pickup = Some(Pickup { x: bx + bw / 2.0, y: by, kind });
                    }
                }
            }
        }
        self.hits = hits; // hand the buffer back for next frame

        if y - BALL_R > H {
            self.lost = true;
        }
        self.ball = [x, y, vx, vy];
    }

    /// Rebuild the instance buffer in place. Capacity is reserved once in `new`,
    /// so this never grows WASM memory and never detaches the JS view.
    pub fn rebuild(&mut self) {
        self.instances.clear();

        // Pleura lining the chest wall: left, right and apex. Drawn first so
        // everything else composites over it. `burn` carries which edge it is
        // (0 vertical, 1 horizontal) so the shader can orient its gradient.
        self.instances.push(Instance {
            x: 0.0, y: 0.0, w: PLEURA, h: H, kind: 8.0, burn: 0.0,
        });
        self.instances.push(Instance {
            x: W - PLEURA, y: 0.0, w: PLEURA, h: H, kind: 8.0, burn: 0.0,
        });
        self.instances.push(Instance {
            x: 0.0, y: 0.0, w: W, h: PLEURA, kind: 8.0, burn: 1.0,
        });

        for i in 0..self.blocks.len() {
            let b = self.blocks[i];
            if !b.alive && b.burn <= 0.0 {
                continue;
            }
            let (x, y, w, h) = self.block_rect(i);
            let base = if b.airway {
                5.0
            } else if b.span > 1 {
                1.0
            } else {
                0.0
            };
            // Burning cells render as kind 6 regardless of what they were, so
            // the shader has an explicit flag. Progress legitimately starts at
            // 0.0, so `burn > 0.0` cannot be used to detect "is burning".
            let (kind, burn) = if b.alive {
                (base, 0.0)
            } else {
                (6.0, (1.0 - b.burn / b.burn_full.max(1e-4)).clamp(0.0, 1.0))
            };
            self.instances.push(Instance { x, y, w, h, kind, burn });
        }
        self.instances.push(Instance {
            x: self.cig_x - CIG_W / 2.0,
            y: CIG_Y,
            w: CIG_W,
            h: CIG_H,
            kind: 2.0,
            burn: 0.0,
        });
        self.instances.push(Instance {
            x: self.ball[0] - BALL_R,
            y: self.ball[1] - BALL_R,
            w: BALL_R * 2.0,
            h: BALL_R * 2.0,
            kind: 3.0,
            burn: 0.0,
        });
        // X-ray trail: dots along the predicted polyline. Dots rather than a
        // line because a rotated line would need a rotation attribute, and the
        // dotted look reads as "prediction" rather than "solid object".
        if self.radiologist > 0 {
            const DOT: f32 = 4.0;
            self.predict();
            let path = std::mem::take(&mut self.path);
            // Total length first, so each dot can fade with distance travelled.
            let total: f32 = path
                .windows(2)
                .map(|s| ((s[1][0] - s[0][0]).powi(2) + (s[1][1] - s[0][1]).powi(2)).sqrt())
                .sum();
            let dot_budget = self.instances.len() + MAX_TRAIL_DOTS;

            // Fixed spacing, in path order, until the budget runs out. A path
            // too long for the dots simply stops being drawn part way along —
            // the trail shows as much of the prediction as it can afford.
            let mut walked = 0.0;
            // Carried across segments so the rhythm continues through each
            // bounce instead of restarting, which would bunch a dot at every
            // vertex and waste up to a full spacing per segment.
            let mut carry = 0.0f32;
            for seg in path.windows(2) {
                if self.instances.len() >= dot_budget {
                    break;
                }
                let (a, b) = (seg[0], seg[1]);
                let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
                let len = (dx * dx + dy * dy).sqrt();
                if len < 1e-3 {
                    continue;
                }
                let mut t = carry;
                while t < len && self.instances.len() < dot_budget {
                    let px = a[0] + dx * (t / len);
                    let py = a[1] + dy * (t / len);
                    // Fraction along the whole path, packed into `burn` so the
                    // shader can fade the tail without another attribute.
                    let f = if total > 0.0 { (walked + t) / total } else { 0.0 };
                    self.instances.push(Instance {
                        x: px - DOT / 2.0,
                        y: py - DOT / 2.0,
                        w: DOT,
                        h: DOT,
                        kind: 7.0,
                        burn: f.clamp(0.0, 1.0),
                    });
                    t += SPACING_PX;
                }
                carry = (t - len).max(0.0);
                walked += len;
            }
            self.path = path; // hand the buffer back for next frame
        }

        if let Some(p) = self.pickup {
            let (w, h) = p.kind.size();
            self.instances.push(Instance {
                x: p.x - w / 2.0,
                y: p.y - h / 2.0,
                w,
                h,
                kind: p.kind.sprite(),
                burn: 0.0,
            });
        }
    }
}

#[wasm_bindgen]
pub struct GameHandle {
    inner: Game,
    gl: Gl,
}

#[wasm_bindgen]
impl GameHandle {
    #[wasm_bindgen(constructor)]
    pub fn new(gl: Gl) -> GameHandle {
        GameHandle { inner: Game::new(), gl }
    }

    pub fn set_paddle(&mut self, x: f32) {
        self.inner.cig_x = x.clamp(CIG_W / 2.0, W - CIG_W / 2.0);
    }

    pub fn restart(&mut self) {
        self.inner = Game::new();
    }

    #[wasm_bindgen(getter)]
    pub fn copd(&self) -> f32 {
        self.inner.copd
    }

    /// Share of the lung destroyed so far, 0..1.
    ///
    /// This is the honest damage figure: it counts tissue actually gone, where
    /// `copd` only moves when a carton is caught and so sits frozen through
    /// most of a game.
    #[wasm_bindgen(getter)]
    pub fn damage(&self) -> f32 {
        let start = self.inner.starting_alive.max(1) as f32;
        1.0 - self.inner.alive_count as f32 / start
    }

    #[wasm_bindgen(getter)]
    pub fn lost(&self) -> bool {
        self.inner.lost
    }

    #[wasm_bindgen(getter)]
    pub fn pierce(&self) -> u32 {
        self.inner.pierce
    }

    #[wasm_bindgen(getter)]
    pub fn radiologist(&self) -> bool {
        self.inner.radiologist > 0
    }

    /// Diagnostics for the HUD: paddle bounces and asbestos pickups caught, so
    /// a "stuck" counter can be told apart from one being topped back up.
    #[wasm_bindgen(getter)]
    pub fn bounces(&self) -> u32 {
        self.inner.bounces
    }

    #[wasm_bindgen(getter)]
    pub fn asbestos_caught(&self) -> u32 {
        self.inner.asbestos_caught
    }

    /// Increments on every catch. JS watches it for changes to fire the
    /// announcement — a counter rather than a flag, so a catch can't be missed
    /// if two land within one frame.
    #[wasm_bindgen(getter)]
    pub fn caught_seq(&self) -> u32 {
        self.inner.caught_seq
    }

    /// Name and blurb of the most recently caught pickup, for the slam text.
    #[wasm_bindgen(getter)]
    pub fn caught_name(&self) -> String {
        self.inner.caught.map_or(String::new(), |k| k.name().into())
    }

    #[wasm_bindgen(getter)]
    pub fn caught_blurb(&self) -> String {
        self.inner.caught.map_or(String::new(), |k| k.blurb().into())
    }

    #[wasm_bindgen(getter)]
    pub fn cleared(&self) -> bool {
        self.inner.won
    }

    #[wasm_bindgen(getter)]
    pub fn destroyed(&self) -> u32 {
        self.inner.destroyed
    }

    /// Seconds of play, stopped once the game ends.
    #[wasm_bindgen(getter)]
    pub fn elapsed(&self) -> f32 {
        self.inner.elapsed
    }

    /// Step physics, then upload instances straight from WASM linear memory.
    /// Returns the instance count for the caller's drawArraysInstanced.
    pub fn frame(&mut self, dt: f32) -> i32 {
        self.inner.tick(dt);
        self.inner.rebuild();
        self.inner.upload(&self.gl);
        self.inner.instance_count()
    }
}

#[wasm_bindgen]
pub fn dimensions() -> Vec<f32> {
    vec![W, H]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lung::dist_to_segment;

    #[test]
    fn ball_clears_blocks_and_buffer_never_grows() {
        let mut g = Game::new();
        let cap = g.instances.capacity();
        let start = g.blocks.iter().filter(|b| b.alive).count();
        assert!(start > 50, "lung should have blocks, got {start}");

        for _ in 0..20_000 {
            g.cig_x = g.ball[0]; // perfect autoplay, never lose
            g.tick(1.0 / 60.0);
            g.rebuild();
            assert!(g.instances.capacity() == cap, "reallocated: view would detach");
            assert!(g.instances.len() <= cap, "overflowed the GPU buffer");
        }
        assert!(!g.lost);
        assert!(g.blocks.iter().filter(|b| b.alive).count() < start);
    }

    #[test]
    fn merging_frees_cells_and_widens_one() {
        let mut g = Game::new();
        let before = g.blocks.iter().filter(|b| b.alive).count();
        g.merge_alveoli();
        let merged: Vec<u8> = g.blocks.iter().filter(|b| b.alive && b.span > 1).map(|b| b.span).collect();
        assert_eq!(merged.len(), 1, "exactly one merged block");
        let span = merged[0] as usize;
        let freed = before - g.blocks.iter().filter(|b| b.alive).count();
        assert_eq!(freed, span * span - 1, "{span}x{span} collapses to 1 block");
    }

    #[test]
    fn airways_are_carved_and_lung_is_two_lobes() {
        // Test the built grid, not raw in_lung — the speckle cull runs after.
        let g = Game::new();
        let live = |c: usize, r: usize| g.blocks[r * COLS + c].alive;
        let total: usize = (0..ROWS).map(|r| (0..COLS).filter(|&c| live(c, r)).count()).sum();
        assert!(total > 300, "lung too sparse: {total}");

        // Trachea occupies the midline at the top, then the mediastinum below
        // the carina is clear.
        assert!(live(COLS / 2, 0), "no trachea at the top of the midline");
        assert!(
            (ROWS / 2..ROWS).all(|r| !live(COLS / 2, r)),
            "mediastinum should be clear below the carina"
        );

        // Both lobes present at mid-height.
        let mid = ROWS / 2;
        assert!((0..COLS / 2).any(|c| live(c, mid)), "no left lobe");
        assert!((COLS / 2..COLS).any(|c| live(c, mid)), "no right lobe");

        // Airways actually remove tissue: some row has an interior gap inside a lobe.
        let has_interior_gap = (0..ROWS).any(|r| {
            let cells: Vec<usize> = (0..COLS / 2).filter(|&c| live(c, r)).collect();
            cells.len() > 2 && cells.last().unwrap() - cells[0] + 1 > cells.len()
        });
        assert!(has_interior_gap, "airways carved nothing out of the left lobe");
    }

    #[test]
    fn hit_blocks_burn_then_disappear() {
        let mut g = Game::new();
        let live = |g: &Game| g.blocks.iter().filter(|b| b.alive).count();
        let start = live(&g);

        // Run until the ball destroys something.
        let mut hit_at = None;
        for f in 0..2_000 {
            g.cig_x = g.ball[0];
            g.tick(1.0 / 60.0);
            if live(&g) < start {
                hit_at = Some(f);
                break;
            }
        }
        assert!(hit_at.is_some(), "ball never hit a block");

        let burning = g.blocks.iter().filter(|b| !b.alive && b.burn > 0.0).count();
        assert!(burning > 0, "destroyed block should be burning");

        // A burning block is emitted as kind 6, with progress starting at 0.
        g.rebuild();
        let b = g.instances.iter().find(|i| i.kind == 6.0).expect("no burning instance");
        assert!((0.0..=1.0).contains(&b.burn), "progress out of range: {}", b.burn);
        // kind must stay a clean integer — nothing packed into its fraction.
        assert!(
            g.instances.iter().all(|i| i.kind.fract() == 0.0),
            "kind must stay integral"
        );

        // After the burn window those blocks stop rendering. Tracked by index
        // rather than by "no burning instance anywhere": the serve keeps
        // breaking tissue on its way down, so there is almost always something
        // else burning, and a blanket check would fail on the newcomers.
        let burning_now: Vec<usize> = (0..g.blocks.len())
            .filter(|&i| !g.blocks[i].alive && g.blocks[i].burn > 0.0)
            .collect();
        let longest = burning_now
            .iter()
            .map(|&i| g.blocks[i].burn)
            .fold(0.0f32, f32::max);
        for _ in 0..((longest * 60.0) as usize + 10) {
            g.tick(1.0 / 60.0);
        }
        assert!(
            burning_now.iter().all(|&i| g.blocks[i].burn <= 0.0),
            "burn outlived its window"
        );
    }

    /// The stride and offsets in index.html's vertexAttribPointer calls are
    /// derived from this layout. If it changes, they must change too.
    #[test]
    fn instance_layout_matches_shader() {
        assert_eq!(std::mem::size_of::<Instance>(), FLOATS_PER_INSTANCE * 4);
        assert_eq!(std::mem::size_of::<Instance>(), 24, "STRIDE in index.html");
        let i = Instance { x: 1.0, y: 2.0, w: 3.0, h: 4.0, kind: 5.0, burn: 6.0 };
        let raw: &[f32; 6] = unsafe { std::mem::transmute(&i) };
        // rect <- bytes 0..16, kind <- 16..20, burn <- 20..24
        assert_eq!(*raw, [1.0, 2.0, 3.0, 4.0, 5.0, 6.0], "field order changed");
    }

    #[test]
    fn pneumothorax_collapses_one_side_only() {
        let mut g = Game::new();
        let mid = COLS / 2;
        let side_counts = |g: &Game| {
            let l = g.blocks.iter().enumerate().filter(|(i, b)| b.alive && i % COLS < mid).count();
            let r = g.blocks.iter().enumerate().filter(|(i, b)| b.alive && i % COLS >= mid).count();
            (l, r)
        };
        let (l0, r0) = side_counts(&g);
        g.apply(Kind::Pneumothorax);
        let (l1, r1) = side_counts(&g);

        // Exactly one side is wiped; the other is untouched.
        assert!(
            (l1 == 0 && r1 == r0) || (r1 == 0 && l1 == l0),
            "expected one side cleared, got {l0}->{l1} and {r0}->{r1}"
        );
        // The collapsed side is mid-burn, staggered rather than uniform.
        let durations: Vec<f32> =
            g.blocks.iter().filter(|b| b.burn > 0.0).map(|b| b.burn_full).collect();
        assert!(durations.len() > 20, "collapse should ignite many blocks");
        let min = durations.iter().cloned().fold(f32::MAX, f32::min);
        let max = durations.iter().cloned().fold(0.0, f32::max);
        assert!(max - min > 0.2, "collapse should be staggered, spread {}", max - min);
    }

    /// Fibres refresh the piercing window, they do not stack. Adding to it let
    /// a run of drops leave the ball cutting through tissue for most of a game,
    /// which removes the bouncing the game is made of.
    #[test]
    fn asbestos_refreshes_rather_than_stacks() {
        let mut g = Game::new();
        g.apply(Kind::Asbestos);
        assert_eq!(g.pierce, PIERCE_BOUNCES);

        // A second fibre caught at full charge adds nothing.
        g.apply(Kind::Asbestos);
        assert_eq!(g.pierce, PIERCE_BOUNCES, "asbestos stacked");

        // And one caught part-way through restores the full window rather than
        // extending past it.
        g.pierce = 1;
        g.apply(Kind::Asbestos);
        assert_eq!(g.pierce, PIERCE_BOUNCES, "a top-up should refresh to the full window");
    }

    #[test]
    fn asbestos_lasts_four_paddle_bounces() {
        let mut g = Game::new();
        g.apply(Kind::Asbestos);
        assert_eq!(g.pierce, PIERCE_BOUNCES);

        let mut bounces = 0;
        let mut destroyed_while_piercing = 0;
        for _ in 0..20_000 {
            g.cig_x = g.ball[0]; // perfect autoplay
            // Suppress drops so the run measures one fibre from full. Catching
            // another would refresh the count — legitimately, but this test is
            // about how long a single one lasts.
            g.pickup = None;
            let piercing = g.pierce > 0;
            let vy_before = g.ball[3];
            let live_before = g.blocks.iter().filter(|b| b.alive).count();
            let pierce_before = g.pierce;
            g.tick(1.0 / 60.0);

            if g.pierce < pierce_before {
                bounces += 1;
            }
            // While piercing, destroying blocks must not flip vertical travel.
            let destroyed = live_before - g.blocks.iter().filter(|b| b.alive).count();
            if piercing && destroyed > 0 {
                destroyed_while_piercing += destroyed;
                if vy_before < 0.0 {
                    assert!(g.ball[3] < 0.0, "piercing ball reflected off a block");
                }
            }
            if g.pierce == 0 && bounces == 4 {
                break;
            }
        }
        assert_eq!(bounces, 4, "pierce should be spent by exactly 4 bounces");
        assert!(
            destroyed_while_piercing > 6,
            "a fibre should cut whole channels, only got {destroyed_while_piercing}"
        );
    }

    #[test]
    fn spawn_weights_gate_pneumothorax_until_scattered() {
        let mut g = Game::new();
        // An intact lung drops only the two tissue-breaking kinds.
        for _ in 0..500 {
            let k = g.roll_kind();
            assert!(
                k == Some(Kind::Cigarettes) || k == Some(Kind::Asbestos),
                "intact lung rolled {k:?}; opening drops should be cigarettes or asbestos"
            );
        }

        // Break the lung up until it is mostly exposed, then the endgame kinds
        // become reachable. Killing every other column does it directly.
        for i in 0..g.blocks.len() {
            if g.blocks[i].alive && (i % COLS) % 2 == 0 {
                g.ignite(i, 0.0);
            }
        }
        assert!(g.scatter() > 0.95, "setup should be fully scattered, got {}", g.scatter());

        let mut saw_pneumo = false;
        let mut saw_radiologist = false;
        for _ in 0..2_000 {
            match g.roll_kind() {
                Some(Kind::Pneumothorax) => saw_pneumo = true,
                Some(Kind::Radiologist) => saw_radiologist = true,
                _ => {}
            }
        }
        assert!(saw_radiologist, "radiologist never rolled on a scattered lung");
        assert!(saw_pneumo, "pneumothorax never rolled on a scattered lung");
    }

    /// The ball must bounce off the inner face of the drawn pleura. If the
    /// walls and the membrane drift apart the ball visibly passes through it.
    #[test]
    fn ball_bounces_off_the_pleura_not_the_canvas_edge() {
        let mut g = Game::new();
        // Fire it flat and fast at the right wall; autoplay alone keeps the
        // ball in a central column and never tests the sides.
        g.ball = [W / 2.0, 520.0, 320.0, -40.0];
        let mut touched_left = false;
        let mut touched_right = false;
        for _ in 0..20_000 {
            g.cig_x = g.ball[0];
            g.tick(1.0 / 60.0);
            // Never inside the membrane.
            assert!(
                g.ball[0] - BALL_R >= PLEURA - 0.5,
                "ball entered the left pleura: x={}",
                g.ball[0]
            );
            assert!(
                g.ball[0] + BALL_R <= W - PLEURA + 0.5,
                "ball entered the right pleura: x={}",
                g.ball[0]
            );
            if g.ball[0] <= WALL_L + 1.0 {
                touched_left = true;
            }
            if g.ball[0] >= WALL_R - 1.0 {
                touched_right = true;
            }
        }
        // At least one side wall must actually get hit, or the containment
        // assertions above passed vacuously.
        assert!(touched_left || touched_right, "never reached a side wall");
    }

    /// `alive_count` replaced a full scan, so it must never drift from the
    /// truth — every removal path has to funnel through `ignite`.
    #[test]
    fn alive_count_tracks_the_blocks() {
        let mut g = Game::new();
        let real = |g: &Game| g.blocks.iter().filter(|b| b.alive).count();
        assert_eq!(g.alive_count, real(&g), "wrong at start");

        for f in 0..3_000 {
            g.cig_x = g.ball[0];
            g.tick(1.0 / 60.0);
            if f % 200 == 0 {
                g.apply(Kind::Pneumothorax); // bulk removal path
                g.apply(Kind::Cigarettes); // merge path
            }
            assert_eq!(g.alive_count, real(&g), "drifted at frame {f}");
        }
    }

    #[test]
    fn destroying_every_block_wins_and_stops_the_game() {
        let mut g = Game::new();
        assert!(!g.won);

        // Collapse both lungs: two pneumothoraces clear each side in turn.
        for _ in 0..8 {
            if g.alive_count == 0 {
                break;
            }
            g.apply(Kind::Pneumothorax);
        }
        assert_eq!(g.alive_count, 0, "setup failed to clear the lungs");
        assert!(g.won, "clearing every block must win");
        assert!(!g.lost, "winning is not losing");
        assert!(g.cleared());

        // The clock stops and the ball freezes once won.
        let ball = g.ball;
        let t = g.elapsed;
        for _ in 0..120 {
            g.tick(1.0 / 60.0);
        }
        assert_eq!(g.elapsed, t, "clock kept running after the win");
        assert_eq!(g.ball, ball, "ball kept moving after the win");

        // But burns still finish, so the final collapse animates out.
        assert!(
            g.blocks.iter().all(|b| b.burn == 0.0),
            "burns should have completed"
        );
    }

    #[test]
    fn catching_records_the_kind_and_bumps_the_sequence() {
        let mut g = Game::new();
        assert_eq!(g.caught_seq, 0);
        assert!(g.caught.is_none());

        for (n, kind) in KINDS.iter().enumerate() {
            g.apply(*kind);
            assert_eq!(g.caught, Some(*kind), "wrong kind recorded");
            assert_eq!(g.caught_seq, n as u32 + 1, "sequence must bump per catch");
        }
        // Two catches of the same kind still advance it, or the overlay would
        // not retrigger.
        let before = g.caught_seq;
        g.apply(Kind::Cigarettes);
        g.apply(Kind::Cigarettes);
        assert_eq!(g.caught_seq, before + 2);
    }

    /// The ball must fit through the channel between the pleura and the
    /// outermost block column. Narrower than a ball diameter and it wedges,
    /// jitters, and the x-ray prediction diverges wildly.
    #[test]
    fn ball_fits_between_the_pleura_and_the_grid() {
        let gap = GRID_X - PLEURA;
        assert!(
            gap >= BALL_R * 2.0,
            "only {gap}px between pleura and grid, ball is {}px wide",
            BALL_R * 2.0
        );
        // And the grid must not run under the membrane at all.
        assert!(GRID_X >= PLEURA, "grid starts inside the pleura");
        let right = GRID_X + COLS as f32 * CELL;
        assert!(right <= W - PLEURA, "grid ends inside the right pleura");
    }

    #[test]
    fn every_kind_is_named_for_the_player() {
        let mut names: Vec<&str> = KINDS.iter().map(|k| k.name()).collect();
        for k in KINDS {
            assert!(!k.name().is_empty(), "{k:?} has no name");
            assert!(!k.blurb().is_empty(), "{k:?} has no blurb");
            // Labels sit under a falling sprite; keep them short enough to read.
            assert!(k.name().len() <= 16, "{k:?} name too long: {}", k.name());
            assert!(k.blurb().len() <= 40, "{k:?} blurb too long: {}", k.blurb());
        }
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), KINDS.len(), "names must be distinct");
    }

    #[test]
    fn every_kind_has_a_distinct_sprite() {
        let mut ids: Vec<u32> = KINDS.iter().map(|k| k.sprite() as u32).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), KINDS.len(), "sprite ids must be unique");
        // Must not collide with tissue/cig/ball/burn kinds used by the shader.
        for id in ids {
            assert!(id >= 10, "pickup sprite {id} collides with entity kinds");
        }
    }

    /// The whole point of the x-ray: the drawn path must be where the ball
    /// actually goes. Simulate forward and check the real ball stays on the
    /// predicted polyline until it reaches the bottom.
    #[test]
    fn prediction_matches_real_physics() {
        let mut g = Game::new();
        // Park the paddle out of reach before tracing: this test is about the
        // descent matching the real ball, and a paddle under the ball would end
        // the path at the catch instead of the floor.
        g.cig_x = -1000.0;
        g.predict();
        let path = g.path.clone();
        assert!(path.len() > 2, "path should bounce at least once");
        assert!(
            path.last().unwrap()[1] >= H - 1.0,
            "path must reach the bottom, ended at y={}",
            path.last().unwrap()[1]
        );

        // Distance from a point to the polyline.
        let dist_to_path = |p: [f32; 2]| {
            path.windows(2)
                .map(|s| dist_to_segment(p, s[0], s[1]))
                .fold(f32::MAX, f32::min)
        };

        let first_impact = path[1];
        let mut worst: f32 = 0.0;
        for _ in 0..4_000 {
            g.tick(1.0 / 240.0); // fine steps: discrete ticks overshoot corners
            let p = [g.ball[0], g.ball[1]];
            // Only up to the first impact; past it, pocket bounces diverge.
            if (p[0] - first_impact[0]).hypot(p[1] - first_impact[1]) < 12.0 {
                break;
            }
            worst = worst.max(dist_to_path(p));
            if g.ball[1] > H {
                break;
            }
        }
        assert!(worst < 12.0, "ball drifted {worst:.1}px before the first impact");
    }

    /// While asbestos is active the ball cuts straight through tissue, so the
    /// predicted path must be a straight line to the floor — pierce is spent at
    /// the paddle, not per block.
    /// Accuracy at the real frame rate, for the part of the path the player
    /// actually acts on.
    ///
    /// The prediction cannot stay exact indefinitely: a discrete 60fps step
    /// leaves a few pixels of residual error at each bounce, and in a dense
    /// block field ~4 bounces is enough for the real ball to meet a different
    /// block than the trace did, after which the two paths are unrelated. So
    /// the guarantee is scoped to the near term — measured over the first
    /// three segments, which is what you steer by.
    #[test]
    fn prediction_is_accurate_for_the_first_few_bounces() {
        let mut g = Game::new();
        g.ball = [W / 2.0, H * 0.5, -160.0, -300.0];
        g.predict();
        let path = g.path.clone();
        assert!(path.len() >= 5, "need a multi-bounce path, got {}", path.len());

        // Only up to the first impact. Past that the ball can land in a
        // concave pocket of tissue and bounce on consecutive frames, which
        // the segment-stepping trace models as one clean reflection — see
        // `ponytail:` note on `predict`.
        let near: Vec<[f32; 2]> = path.iter().take(2).copied().collect();
        let end = near[near.len() - 1];

        g.cig_x = -10_000.0; // prediction ignores the paddle
        let mut worst: f32 = 0.0;
        for _ in 0..4_000 {
            g.tick(1.0 / 60.0); // the rate the game actually runs at
            let p = [g.ball[0], g.ball[1]];
            // Stop once the ball reaches the end of the segment under test.
            if (p[0] - end[0]).hypot(p[1] - end[1]) < 12.0 {
                break;
            }
            let d = near
                .windows(2)
                .map(|s| dist_to_segment(p, s[0], s[1]))
                .fold(f32::MAX, f32::min);
            worst = worst.max(d);
            if g.ball[1] > H {
                break;
            }
        }
        assert!(worst < 12.0, "near path diverged by {worst:.1}px at 60fps");
    }

    #[test]
    fn prediction_honours_asbestos() {
        let mut g = Game::new();
        // Aim up into the middle of the lung, where it will meet many blocks.
        g.ball = [400.0, 300.0, -160.0, -300.0];

        g.predict();
        let normal_broken = g.broken.len();
        assert!(normal_broken > 1, "setup should hit several blocks");

        g.apply(Kind::Asbestos);
        g.predict();
        assert!(
            g.broken.len() > normal_broken,
            "piercing should cut more blocks ({} vs {normal_broken})",
            g.broken.len()
        );
        assert!(
            g.path.last().unwrap()[1] >= H,
            "piercing path must still reach the floor"
        );

        // The real invariant: direction only changes at walls. Vertices are
        // recorded at every impact, including pierced blocks, so check the
        // heading rather than the vertex position — a block the ball cuts
        // through must leave the direction untouched.
        let path = g.path.clone();
        let dir = |a: [f32; 2], b: [f32; 2]| {
            let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
            let len = (dx * dx + dy * dy).sqrt().max(1e-6);
            [dx / len, dy / len]
        };
        for i in 1..path.len() - 1 {
            let before = dir(path[i - 1], path[i]);
            let after = dir(path[i], path[i + 1]);
            let turned = (before[0] - after[0]).abs() > 1e-3
                || (before[1] - after[1]).abs() > 1e-3;
            if !turned {
                continue; // cut straight through, as piercing should
            }
            let v = path[i];
            let at_wall =
                v[0] <= WALL_L + 1.0 || v[0] >= WALL_R - 1.0 || v[1] <= WALL_TOP + 1.0;
            assert!(at_wall, "piercing path turned at a block: vertex {i} = {v:?}");
        }
    }

    /// The whole point of the radiologist: the drawn path must hold all the way
    /// to the floor, not just to the first block. Before the live tick and the
    /// trace shared `next_impact`, the stepped ball bounced from *inside* each
    /// block it penetrated, so the two diverged after a few hops and the path
    /// visibly re-drew itself mid-flight.
    #[test]
    fn prediction_holds_for_the_whole_path() {
        let mut g = Game::new();
        g.ball = [400.0, 300.0, -160.0, -300.0];
        g.predict();
        let path = g.path.clone();
        assert!(path.len() > 6, "need a long multi-bounce path, got {}", path.len());

        let dist_to_path = |p: [f32; 2]| {
            path.windows(2)
                .map(|s| dist_to_segment(p, s[0], s[1]))
                .fold(f32::MAX, f32::min)
        };

        g.cig_x = -10_000.0; // the trace deliberately ignores the paddle
        let mut worst: f32 = 0.0;
        for _ in 0..4_000 {
            g.tick(1.0 / 60.0);
            worst = worst.max(dist_to_path([g.ball[0], g.ball[1]]));
            if g.ball[1] > H {
                break;
            }
        }
        // Tight on purpose: the two solves are now the same code on the same
        // state, so the only error left is the sub-pixel residue of stepping in
        // 1/60s chunks. A regression that reintroduces a second collision rule
        // shows up here as tens of pixels, not tenths.
        assert!(worst < 1.0, "ball left the drawn path by {worst:.1}px");
    }

    /// Mean area of the blocks the ball can still hit — the size of the average
    /// target the player is aiming at.
    fn mean_target(g: &Game) -> f32 {
        let live: Vec<usize> = (0..g.blocks.len()).filter(|&i| g.blocks[i].alive).collect();
        let area: f32 = live
            .iter()
            .map(|&i| {
                let (_, _, w, h) = g.block_rect(i);
                w * h
            })
            .sum();
        area / live.len() as f32
    }

    /// Cigarettes make the field easier to clear: fewer blocks, each a bigger
    /// target. That is the mechanic, so both halves are pinned here.
    ///
    /// Every carton must also actually land. The merge used to give up after 96
    /// random probes, which fail more and more often as the lung fragments, so
    /// late cartons quietly did nothing — the effect peaked mid-game and then
    /// tailed off instead of compounding.
    #[test]
    fn cartons_consolidate_the_lung() {
        let mut g = Game::new();
        let (blocks0, target0) = (g.alive_count, mean_target(&g));
        for n in 1..=6 {
            let before = g.alive_count;
            g.apply(Kind::Cigarettes);
            assert!(
                g.alive_count < before,
                "carton {n} merged nothing (still {before} blocks, copd {:.2})",
                g.copd
            );
        }
        // Measured at ~193 blocks and ~1.98x mean target from 382/169. The
        // thresholds sit just under that: enough headroom for the merge search
        // to land differently, tight enough to catch the effect being weakened.
        assert!(
            g.alive_count < blocks0 / 2 + 10,
            "six cartons should roughly halve the block count: {blocks0} -> {}",
            g.alive_count
        );
        assert!(
            mean_target(&g) > target0 * 1.7,
            "targets should grow substantially: {target0:.0} -> {:.0}",
            mean_target(&g)
        );
    }

    #[test]
    fn dbg_curve() {
        // Does a span-6 patch even exist in a fresh lung? The lobes are carved
        // out of a 40x24 grid, so a 6x6 square of live cells may never fit.
        let g = Game::new();
        for span in 2..=6usize {
            let mut found = 0;
            for row in 0..(ROWS - span) {
                for col in 0..(COLS - span) {
                    let ok = (0..span).all(|dy| (0..span).all(|dx|
                        g.blocks[(row+dy)*COLS + col+dx].alive));
                    if ok { found += 1; }
                }
            }
            println!("span {span}: {found} clean patches in a fresh lung");
        }
    }

    /// The x-ray previews one paddle return: where the shot you are lining up
    /// will actually go, ending at the first tissue it meets.
    #[test]
    fn prediction_previews_the_paddle_return() {
        let mut g = Game::new();
        g.ball = [W / 2.0, H * 0.72, 40.0, 300.0];
        g.cig_x = g.ball[0]; // under the ball, so it is caught rather than missed

        g.predict();
        let path = g.path.clone();
        let caught = path
            .iter()
            .position(|v| (v[1] - (CIG_Y - BALL_R)).abs() < 0.5)
            .expect("path should reach the paddle face");
        assert!(caught + 1 < path.len(), "path stops at the catch, no return leg drawn");

        // The return climbs: every vertex after the catch is above it.
        for v in &path[caught + 1..] {
            assert!(v[1] < CIG_Y - BALL_R, "return leg dips back below the paddle at {v:?}");
        }

        // And it ends on tissue or at the apex, not in mid-air.
        let end = *path.last().unwrap();
        let at_apex = end[1] <= WALL_TOP + 1.0;
        let on_block = (0..g.blocks.len()).any(|i| {
            if !g.blocks[i].alive {
                return false;
            }
            let (bx, by, bw, bh) = g.block_rect(i);
            end[0] >= bx - BALL_R - 1.0
                && end[0] <= bx + bw + BALL_R + 1.0
                && end[1] >= by - BALL_R - 1.0
                && end[1] <= by + bh + BALL_R + 1.0
        });
        assert!(at_apex || on_block, "return leg ended in mid-air at {end:?}");
    }

    /// A ball heading for the gap beside the paddle must still be drawn all the
    /// way down. Ending the trace at the paddle plane regardless of where the
    /// paddle is would hide exactly the shot the player needs to see missed.
    #[test]
    fn prediction_does_not_fake_a_catch_it_will_miss() {
        let mut g = Game::new();
        g.ball = [W / 2.0, H * 0.72, 0.0, 300.0];
        g.cig_x = CIG_W / 2.0; // hard left, nowhere near the ball

        g.predict();
        let end = *g.path.last().unwrap();
        assert!(end[1] >= H - 1.0, "missed ball should fall to the floor, ended at {end:?}");
    }

    /// Scatter is the pickup schedule's clock, so its shape matters: it must
    /// start low on an intact lung and rise as the board breaks up. It is
    /// deliberately not `alive_count` — cigarettes cut the survivor count
    /// without scattering anything, which would fake progress to the endgame.
    #[test]
    fn scatter_rises_as_the_lung_breaks_up() {
        let mut g = Game::new();
        let intact = g.scatter();
        assert!(
            (0.3..0.6).contains(&intact),
            "an intact lung should read as mostly unexposed, got {intact:.2}"
        );

        // Cartons raise it too, and should: a merge leaves its absorbed cells
        // dead, which punches a hole and exposes the neighbours. That is real
        // scattering, not the metric being fooled.
        g.apply(Kind::Cigarettes);
        g.apply(Kind::Cigarettes);
        assert!(g.scatter() > intact, "cartons should expose tissue");

        // Punching holes through it must too, and much harder.
        for i in 0..g.blocks.len() {
            if g.blocks[i].alive && (i % COLS) % 2 == 0 {
                g.ignite(i, 0.0);
            }
        }
        assert!(g.scatter() > 0.95, "a combed lung should be fully exposed, got {:.2}", g.scatter());
    }

    /// The mix the player actually feels: only tissue-breakers early, the
    /// finishing tools late, drifting rather than switching at a threshold.
    #[test]
    fn pickup_mix_follows_the_schedule() {
        let pct = |s: f32, k: Kind| {
            let tot: u32 = KINDS.iter().map(|x| x.weight(s)).sum();
            100.0 * k.weight(s) as f32 / tot as f32
        };

        // Opening: 65/35 cigarettes to asbestos, nothing else.
        assert_eq!(pct(0.47, Kind::Cigarettes).round(), 65.0);
        assert_eq!(pct(0.47, Kind::Asbestos).round(), 35.0);
        assert_eq!(pct(0.47, Kind::Radiologist), 0.0);
        assert_eq!(pct(0.47, Kind::Pneumothorax), 0.0);

        // Endgame: the radiologist leads, pneumothorax stays rare.
        assert!(pct(1.0, Kind::Radiologist) > 45.0, "radiologist should lead late");
        assert!(pct(1.0, Kind::Pneumothorax) < 15.0, "pneumothorax should stay rare");

        // And it drifts: each step toward a scattered lung raises the radiologist's
        // share and lowers the cigarettes', with no jump between neighbours.
        let steps = [0.55f32, 0.65, 0.75, 0.85, 0.95];
        for w in steps.windows(2) {
            assert!(
                pct(w[1], Kind::Radiologist) > pct(w[0], Kind::Radiologist),
                "radiologist share should keep rising ({:.2} -> {:.2})", w[0], w[1]
            );
            assert!(
                pct(w[1], Kind::Cigarettes) < pct(w[0], Kind::Cigarettes),
                "cigarette share should keep falling ({:.2} -> {:.2})", w[0], w[1]
            );
            assert!(
                (pct(w[1], Kind::Radiologist) - pct(w[0], Kind::Radiologist)).abs() < 25.0,
                "mix jumps too hard between {:.2} and {:.2}", w[0], w[1]
            );
        }
    }

    /// A dead-centre catch used to return the ball perfectly vertically, and a
    /// vertical ball bounces in one column forever — it can never reach tissue
    /// to either side, so the board stalls half full.
    #[test]
    fn a_centred_catch_never_returns_the_ball_vertically() {
        let mut ball = [W / 2.0, CIG_Y - BALL_R, 0.0, 300.0];
        assert!(bounce_off_paddle(&mut ball, W / 2.0), "dead centre should still bounce");
        assert!(ball[2].abs() > 1.0, "vertical return: vx = {}", ball[2]);
        // Speed is preserved, so the nudge cannot creep the ball faster.
        let speed = (ball[2] * ball[2] + ball[3] * ball[3]).sqrt();
        assert!((speed - 300.0).abs() < 1.0, "speed changed to {speed}");
    }

    /// The endgame's failure mode is fishing: a few scattered survivors, rallies
    /// that return the ball without breaking anything, and no way to see where
    /// it is going. Two barren returns in a row send a radiologist, so the help
    /// arrives when the fishing starts instead of waiting on the drop table.
    #[test]
    fn barren_rallies_send_a_radiologist() {
        let mut g = Game::new();
        // Clear the lung so no bounce can break anything, then rally.
        for i in 0..g.blocks.len() {
            if g.blocks[i].alive {
                g.ignite(i, 0.0);
            }
        }
        g.won = false;
        g.pickup = None;
        g.ball = [W / 2.0, CIG_Y - BALL_R - 50.0, 30.0, 300.0];

        let mut bounces = 0;
        for _ in 0..3_000 {
            g.cig_x = g.ball[0].clamp(CIG_W / 2.0, W - CIG_W / 2.0);
            let before = g.bounces;
            g.tick(1.0 / 60.0);
            if g.bounces != before {
                bounces += 1;
                if bounces < BARREN_BOUNCES {
                    assert!(g.pickup.is_none(), "sent help after only {bounces} barren return(s)");
                } else {
                    assert_eq!(
                        g.pickup.map(|p| p.kind),
                        Some(Kind::Radiologist),
                        "no radiologist after {bounces} barren returns"
                    );
                    return;
                }
            }
        }
        panic!("never completed {BARREN_BOUNCES} paddle returns");
    }

    /// Breaking a block resets the count, so a productive rally never triggers
    /// the pity drop.
    #[test]
    fn breaking_blocks_resets_the_barren_count() {
        let mut g = Game::new();
        g.ball = [W / 2.0, H * 0.5, 90.0, -260.0];
        for _ in 0..4_000 {
            g.cig_x = g.ball[0].clamp(CIG_W / 2.0, W - CIG_W / 2.0);
            g.tick(1.0 / 60.0);
            // A full lung is being cleared throughout, so the ball is breaking
            // things and the counter must never reach the threshold.
            assert!(
                g.barren < BARREN_BOUNCES,
                "barren count reached {} while the ball was still clearing tissue",
                g.barren
            );
            if g.lost || g.won {
                break;
            }
        }
    }

    /// The serve comes down the airway, not up through the lungs.
    ///
    /// Launching from just above the paddle sent the ball up through the soft
    /// underside of both lobes, carving a large share of the lung before the
    /// player had touched it. Starting at the carina means the ball enters the
    /// way air does and has to work outward from the middle.
    #[test]
    fn the_serve_starts_at_the_airway_heading_down() {
        let mut g = Game::new();
        let [x, y, _, vy] = g.ball;

        assert!(vy > 0.0, "serve should head down into the lungs, vy = {vy}");
        assert!((x - W / 2.0).abs() < 1.0, "serve should start on the midline, x = {x}");
        // Near the top of the grid: the airway, not the belly of the lungs.
        assert!(
            y < GRID_Y + ROWS as f32 * CELL * 0.4,
            "serve should start high in the airway, y = {y}"
        );

        // And not embedded in tissue: the swept solve cannot resolve a
        // collision that starts inside a block.
        let overlapping = (0..g.blocks.len()).any(|i| {
            if !g.blocks[i].alive {
                return false;
            }
            let (bx, by, bw, bh) = g.block_rect(i);
            x + BALL_R > bx && x - BALL_R < bx + bw && y + BALL_R > by && y - BALL_R < by + bh
        });
        assert!(!overlapping, "serve starts inside a live block");

        // The opening descent should cost the lung a little, not a lot.
        let before = g.alive_count;
        for _ in 0..2_000 {
            g.cig_x = g.ball[0].clamp(CIG_W / 2.0, W - CIG_W / 2.0);
            let b0 = g.bounces;
            g.tick(1.0 / 60.0);
            if g.bounces != b0 {
                break;
            }
        }
        let carved = before - g.alive_count;
        assert!(carved > 0, "serve should reach tissue on the way down");
        assert!(
            carved < before / 10,
            "serve carved {carved} of {before} blocks before the player acted"
        );
    }

    /// The meter reports tissue actually destroyed, so it fills as the board
    /// clears and reads exactly 100% on a win. It used to track `copd`, which
    /// only moves when a carton is caught — measured frozen for 42% of a game.
    #[test]
    fn damage_tracks_the_board_not_the_disease() {
        let mut g = Game::new();
        let start = g.starting_alive;
        assert!(start > 0, "a fresh lung should have tissue");

        let damage = |g: &Game| 1.0 - g.alive_count as f32 / g.starting_alive as f32;
        assert_eq!(damage(&g), 0.0, "an untouched lung should read no damage");

        // Destroying tissue moves it; catching a carton without destroying
        // tissue does not have to.
        let before = damage(&g);
        for i in 0..g.blocks.len() {
            if g.blocks[i].alive {
                g.ignite(i, 0.0);
                break;
            }
        }
        assert!(damage(&g) > before, "destroying a block should raise damage");

        // Clearing the lung reads exactly full.
        for i in 0..g.blocks.len() {
            if g.blocks[i].alive {
                g.ignite(i, 0.0);
            }
        }
        assert_eq!(g.alive_count, 0);
        assert!((damage(&g) - 1.0).abs() < 1e-6, "a cleared lung should read 100%");
        assert_eq!(start, g.starting_alive, "the denominator must not drift");
    }

    #[test]
    fn radiologist_clears_on_paddle_bounce() {
        let mut g = Game::new();
        g.apply(Kind::Radiologist);
        assert_eq!(g.radiologist, RADIOLOGIST_BOUNCES);

        for _ in 0..20_000 {
            g.cig_x = g.ball[0];
            g.pickup = None; // no top-ups
            g.tick(1.0 / 60.0);
            if g.radiologist == 0 {
                break;
            }
        }
        assert_eq!(g.radiologist, 0, "radiologist should end at the first paddle bounce");
    }

    #[test]
    fn trail_respects_the_instance_budget() {
        let mut g = Game::new();
        let cap = g.instances.capacity();
        g.apply(Kind::Radiologist);
        for _ in 0..600 {
            g.cig_x = g.ball[0];
            g.radiologist = 1; // keep it on for the whole run
            g.tick(1.0 / 60.0);
            g.rebuild();
            assert!(g.instances.len() <= cap, "overflowed instance buffer");
            assert_eq!(g.instances.capacity(), cap, "reallocated: view would detach");
        }
    }

    #[test]
    fn no_stranded_cells() {
        let g = Game::new();
        for row in 0..ROWS {
            for col in 0..COLS {
                if !g.blocks[row * COLS + col].alive {
                    continue;
                }
                let live = |dx: i32, dy: i32| {
                    let (c, r) = (col as i32 + dx, row as i32 + dy);
                    c >= 0
                        && r >= 0
                        && (c as usize) < COLS
                        && (r as usize) < ROWS
                        && g.blocks[r as usize * COLS + c as usize].alive
                };
                assert!(
                    (live(-1, 0) || live(1, 0)) && (live(0, -1) || live(0, 1)),
                    "stranded cell at ({col}, {row})"
                );
            }
        }
    }

    #[test]
    fn ball_stays_in_bounds() {
        let mut g = Game::new();
        for _ in 0..5_000 {
            g.cig_x = g.ball[0];
            g.tick(1.0 / 60.0);
            // The pleura is the wall, not the canvas edge.
            assert!(
                g.ball[0] >= WALL_L - 0.5 && g.ball[0] <= WALL_R + 0.5,
                "ball escaped the pleura: x={}", g.ball[0]
            );
            assert!(g.ball[1] >= WALL_TOP - 0.5, "ball escaped the apex: y={}", g.ball[1]);
        }
    }
}
