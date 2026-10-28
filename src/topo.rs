//! Shared boundaries: results that fit their neighbours exactly.
//!
//! - **Snapping the tools.** A tool vertex within [`TOOL_SNAP_M`] of a target vertex moves
//!   onto it, else within that distance of a target edge onto the edge; a target vertex
//!   within that distance of a tool segment becomes a tool vertex. A line drawn along a
//!   boundary in a geographic view, or a cutter digitised on another layer's edge, then runs
//!   exactly along it instead of a fraction of a millimetre beside it, which would cut slivers.
//! - **New vertices on original edges.** A result vertex within [`EDGE_TOL_M`] of an edge of
//!   the edited features (and not at one of their vertices) is put exactly on that edge: at
//!   its crossing with the tool, computed from the edge's and the tool segment's end points in
//!   a fixed order, or else at its projection onto the edge. Boolean ops round their crossings
//!   to an integer grid, ~1e-7 m off the edge; a neighbour's straight edge would then leave a
//!   hairline gap. Computed this way, every piece that shares the vertex, also of another
//!   feature cut in the same command, gets bit-identical coordinates, and the results of the
//!   other edited features get it where it lies on their edges ([`share_within`]).
//! - **Topological editing.** Each such vertex is also inserted into the features that are
//!   not edited and have that edge (see [`plan`]), with `edit.vertex`, which moves nothing
//!   else.
//!
//! Edges are straight in the working frame (EPSG:2180 in Poland, the storage CRS of Polish
//! cadastral data; UTM elsewhere).

use std::cell::RefCell;
use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

use geo::Coord;

use crate::geojson::{Dim, Geom, P2};
use crate::host_io::{self, LAYER_READ_CAP, LayerInfo, Res, budget};
use crate::proj::{Frame, SNAP_TOL_M, VertexSnap};

/// Result vertices this close to an original edge are put on it. Boolean ops keep their
/// crossings within ~1e-7 m of the true position; the same as the vertex snapping distance.
pub const EDGE_TOL_M: f64 = SNAP_TOL_M;
/// Tools are snapped to the targets' vertices and edges within this distance (1 mm): far
/// above what a projection round trip or a line drawn in lon/lat leaves (a 100 m edge built
/// linearly in lon/lat bends by ~0.2 mm in the frame), far below any meaningful offset.
pub const TOOL_SNAP_M: f64 = 1e-3;

/// A vertex a command placed on an original edge, in lon/lat.
#[derive(Clone, Copy, Debug)]
pub struct NewVertex {
    /// The dimension of the feature it was placed in.
    pub dim: Dim,
    /// The edge's end points.
    pub a: Coord<f64>,
    pub b: Coord<f64>,
    pub at: Coord<f64>,
    /// The target whose result it is in: results of one target share their new vertices
    /// already.
    pub owner: usize,
}

// ---------- small geometry ----------

// sqrt is one WebAssembly instruction, hypot a libm call many times its cost
pub(crate) fn dist(a: P2, b: P2) -> f64 {
    let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
    (dx * dx + dy * dy).sqrt()
}

fn lerp(a: P2, b: P2, t: f64) -> P2 {
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t]
}

/// Distance from `q` to segment ab and the parameter of the nearest point (unclamped
/// parameter outside 0..1 means it lies beyond an end).
pub(crate) fn seg_dist(q: P2, a: P2, b: P2) -> (f64, f64) {
    let d = [b[0] - a[0], b[1] - a[1]];
    let l2 = d[0] * d[0] + d[1] * d[1];
    let t = if l2 > 0.0 { ((q[0] - a[0]) * d[0] + (q[1] - a[1]) * d[1]) / l2 } else { 0.0 };
    (dist(q, lerp(a, b, t.clamp(0.0, 1.0))), t)
}

/// Distance from `q` to the infinite line through a and b.
fn line_dist(q: P2, a: P2, b: P2) -> f64 {
    let d = [b[0] - a[0], b[1] - a[1]];
    let l = dist(a, b);
    if l == 0.0 {
        return dist(q, a);
    }
    ((q[0] - a[0]) * d[1] - (q[1] - a[1]) * d[0]).abs() / l
}

/// The segment with its end points in a fixed order, so that every caller computes the same
/// numbers for it.
fn ordered(s: [P2; 2]) -> [P2; 2] {
    if (s[0][0], s[0][1]) <= (s[1][0], s[1][1]) { s } else { [s[1], s[0]] }
}

/// Where the line of `edge` crosses the line of `cut`, as a point of the edge's line.
fn crossing(edge: [P2; 2], cut: [P2; 2]) -> Option<P2> {
    let ([a, b], [c, d]) = (ordered(edge), ordered(cut));
    let r = [b[0] - a[0], b[1] - a[1]];
    let s = [d[0] - c[0], d[1] - c[1]];
    let den = r[0] * s[1] - r[1] * s[0];
    if den.abs() <= 1e-12 * dist(a, b) * dist(c, d) {
        return None;
    }
    let t = ((c[0] - a[0]) * s[1] - (c[1] - a[1]) * s[0]) / den;
    Some(lerp(a, b, t))
}

// plain comparisons: f64::min/max also handle NaN, at several times the cost
fn lo(a: f64, b: f64) -> f64 {
    if a < b { a } else { b }
}

fn hi(a: f64, b: f64) -> f64 {
    if a > b { a } else { b }
}

fn p2(c: Coord<f64>) -> P2 {
    [c.x, c.y]
}

/// The paths of a geometry (rings and lines) with their `edit.vertex` part and ring numbers.
fn paths(g: &Geom) -> Vec<(usize, usize, &[Coord<f64>])> {
    match g {
        Geom::LineString(l) => vec![(0, 0, &l.0[..])],
        Geom::MultiLineString(m) => m.0.iter().enumerate().map(|(k, l)| (k, 0, &l.0[..])).collect(),
        Geom::Polygon(p) => rings(0, p),
        Geom::MultiPolygon(m) => m.0.iter().enumerate().flat_map(|(k, p)| rings(k, p)).collect(),
        Geom::Point(_) | Geom::MultiPoint(_) => vec![],
    }
}

fn rings(k: usize, p: &geo::Polygon<f64>) -> Vec<(usize, usize, &[Coord<f64>])> {
    std::iter::once(p.exterior()).chain(p.interiors()).enumerate().map(|(r, l)| (k, r, &l.0[..])).collect()
}

/// The lon/lat of a point on edge `e` (metres) whose projection lies closest to the edge's
/// line. The inverse series alone is off by up to ~3e-9 m; a Newton step on the forward
/// projection and a look at the neighbouring floating-point lon/lat values bring it down to
/// their spacing (~1e-9 m in latitude), so that the vertex lies on the neighbour's edge.
fn inv_onto(frame: Frame, p: P2, e: [P2; 2]) -> Coord<f64> {
    let mut c = frame.inv_coord(Coord { x: p[0], y: p[1] });
    let (x, y) = frame.fwd(c.x, c.y);
    let (rx, ry) = (p[0] - x, p[1] - y);
    if rx != 0.0 || ry != 0.0 {
        // a centimetre: the Jacobian to ~1e-8, far more than the step needs
        let h = 1e-7;
        let (xa, ya) = frame.fwd(c.x + h, c.y);
        let (xb, yb) = frame.fwd(c.x, c.y + h);
        let (j11, j21, j12, j22) = ((xa - x) / h, (ya - y) / h, (xb - x) / h, (yb - y) / h);
        let det = j11 * j22 - j12 * j21;
        if det.is_finite() && det != 0.0 {
            c = Coord { x: c.x + (j22 * rx - j12 * ry) / det, y: c.y + (j11 * ry - j21 * rx) / det };
        }
    }
    let off = |c: Coord<f64>| {
        let (x, y) = frame.fwd(c.x, c.y);
        (line_dist([x, y], e[0], e[1]), dist([x, y], p))
    };
    let mut best = (off(c), c);
    for (i, j) in [(-1, 0), (1, 0), (0, -1), (0, 1)] {
        let d = Coord { x: step(c.x, i), y: step(c.y, j) };
        let o = off(d);
        // on the line first, then near p
        if o < best.0 {
            best = (o, d);
        }
    }
    best.1
}

/// `x` moved by `n` units in the last place.
fn step(x: f64, n: i64) -> f64 {
    let b = x.to_bits() as i64;
    let b = if x >= 0.0 { b + n } else { b - n };
    f64::from_bits(b as u64)
}

/// Multiplicative hashing for the placement cache: SipHash costs more than what it caches.
#[derive(Default)]
struct KeyHasher(u64);

impl Hasher for KeyHasher {
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.write_u64(b as u64);
        }
    }

    fn write_u64(&mut self, x: u64) {
        self.0 = (self.0.rotate_left(5) ^ x).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    }

    fn write_usize(&mut self, x: usize) {
        self.write_u64(x as u64);
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

// ---------- a segment grid ----------

/// The finest grid cell: a few snapping distances.
const MIN_CELL_M: f64 = 8.0 * TOOL_SNAP_M;

/// The mean length of the segments: a grid of many short segments with cells this wide
/// registers each in a couple of cells.
fn mean_len(segs: &[[P2; 2]]) -> f64 {
    segs.iter().map(|s| dist(s[0], s[1])).sum::<f64>() / segs.len().max(1) as f64
}

/// Segments (points are segments of length 0) by grid cell, in flat arrays. A segment is
/// registered in the cells of sample points at most `cell / 2` apart, so each of its points
/// lies within `cell / 4` of a registered sample.
struct Grid {
    cell: f64,
    origin: P2,
    nx: i64,
    ny: i64,
    /// Items of cell `c` are `items[start[c]..start[c + 1]]`.
    start: Vec<u32>,
    items: Vec<u32>,
}

impl Grid {
    /// A grid of cells `cell` wide, or coarser when the extent would need too many.
    fn new(segs: &[[P2; 2]], cell: f64) -> Grid {
        let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
        for s in segs {
            for q in s {
                (x0, y0, x1, y1) = (x0.min(q[0]), y0.min(q[1]), x1.max(q[0]), y1.max(q[1]));
            }
        }
        let mut g = Grid { cell: 1.0, origin: [x0, y0], nx: 1, ny: 1, start: vec![0, 0], items: Vec::new() };
        if segs.is_empty() {
            return g;
        }
        g.cell = cell.max(MIN_CELL_M);
        // a coarser grid only costs more candidates per query
        let cap = 4.0 * segs.len() as f64 + 64.0;
        loop {
            g.nx = ((x1 - x0) / g.cell).floor() as i64 + 1;
            g.ny = ((y1 - y0) / g.cell).floor() as i64 + 1;
            if (g.nx as f64) * (g.ny as f64) <= cap {
                break;
            }
            g.cell *= 2.0;
        }
        let mut pairs: Vec<(u32, u32)> = Vec::new();
        let mut mine = Vec::new();
        for (k, s) in segs.iter().enumerate() {
            mine.clear();
            let steps = ((dist(s[0], s[1]) / (0.5 * g.cell)).ceil() as usize).max(1);
            for i in 0..=steps {
                let (cx, cy) = g.key(lerp(s[0], s[1], i as f64 / steps as f64));
                mine.push((cy.clamp(0, g.ny - 1) * g.nx + cx.clamp(0, g.nx - 1)) as u32);
            }
            mine.sort_unstable();
            mine.dedup();
            pairs.extend(mine.iter().map(|&c| (c, k as u32)));
        }
        let ncell = (g.nx * g.ny) as usize;
        g.start = vec![0; ncell + 1];
        for &(c, _) in &pairs {
            g.start[c as usize + 1] += 1;
        }
        for c in 0..ncell {
            g.start[c + 1] += g.start[c];
        }
        let mut fill = g.start.clone();
        g.items = vec![0; pairs.len()];
        for (c, k) in pairs {
            g.items[fill[c as usize] as usize] = k;
            fill[c as usize] += 1;
        }
        g
    }

    fn key(&self, q: P2) -> (i64, i64) {
        (((q[0] - self.origin[0]) / self.cell).floor() as i64, ((q[1] - self.origin[1]) / self.cell).floor() as i64)
    }

    /// Calls `f` with every segment that may lie within `r` of `q` (some several times).
    fn near(&self, q: P2, r: f64, f: impl FnMut(usize)) {
        self.in_box([q[0], q[1], q[0], q[1]], r, f);
    }

    /// Calls `f` with every segment that may lie within `r` of the box (x0, y0, x1, y1).
    fn in_box(&self, b: [f64; 4], r: f64, mut f: impl FnMut(usize)) {
        let e = r + 0.25 * self.cell;
        let (x0, y0) = self.key([b[0] - e, b[1] - e]);
        let (x1, y1) = self.key([b[2] + e, b[3] + e]);
        for cy in y0.max(0)..=y1.min(self.ny - 1) {
            for cx in x0.max(0)..=x1.min(self.nx - 1) {
                let c = (cy * self.nx + cx) as usize;
                for &k in &self.items[self.start[c] as usize..self.start[c + 1] as usize] {
                    f(k as usize);
                }
            }
        }
    }
}

// ---------- placing result vertices ----------

/// An edit target's edge near the tools: metric and lon/lat end points, in a fixed order.
struct Edge {
    m: [P2; 2],
    ll: [Coord<f64>; 2],
    dim: Dim,
    /// The target it belongs to, in [`Placer::add_target`] order.
    owner: usize,
}

/// Projects the edited features and the tools, snaps the tools to the targets, and turns
/// results back into lon/lat: original vertices bit-exact (as [`VertexSnap`]), new vertices
/// on original edges exactly on them, tool vertices back to the tool's coordinates.
///
/// Register every target and tool, then call [`Placer::snap_paths`] before
/// [`Placer::unproject`], or make a [`CutIndex`] for cut lines that are not snapped.
pub struct Placer {
    frame: Frame,
    targets: VertexSnap,
    tools: VertexSnap,
    /// Each target's dimension.
    owners: Vec<Dim>,
    ix: CutIndex,
}

/// The target edges near some cut lines, and the cut lines. Only those edges are indexed:
/// new vertices only arise where the cut lines cross.
pub struct CutIndex {
    /// Each target's metric bounding box (x0, y0, x1, y1).
    bboxes: Vec<[f64; 4]>,
    /// Placed positions by metric position and edge: the pieces on both sides of a cut
    /// share each crossing.
    placed: RefCell<HashMap<(u64, u64, usize), Coord<f64>, BuildHasherDefault<KeyHasher>>>,
    edges: Vec<Edge>,
    edge_grid: Grid,
    vertex_grid: Grid,
    cutters: Cutters,
}

/// Cut lines (metres) whose crossings with the edges a [`Placer`] computes exactly.
pub struct Cutters {
    segs: Vec<[P2; 2]>,
    /// (path, segment) of each of `segs`.
    ids: Vec<(usize, usize)>,
    /// x0, y0, x1, y1 of all of them.
    bb: [f64; 4],
    /// Only for many segments: a few are faster scanned.
    grid: Option<Grid>,
}

/// Up to this many segments a scan beats a grid.
const SCAN_SEGMENTS: usize = 32;

impl Cutters {
    pub fn new(paths: &[Vec<P2>]) -> Cutters {
        let (mut segs, mut ids) = (Vec::new(), Vec::new());
        let mut bb = [f64::MAX, f64::MAX, f64::MIN, f64::MIN];
        for (pi, p) in paths.iter().enumerate() {
            for (i, w) in p.windows(2).enumerate().filter(|(_, w)| w[0] != w[1]) {
                segs.push([w[0], w[1]]);
                ids.push((pi, i));
                for q in [w[0], w[1]] {
                    bb = [bb[0].min(q[0]), bb[1].min(q[1]), bb[2].max(q[0]), bb[3].max(q[1])];
                }
            }
        }
        // few long segments: cells as fine as the extent allows
        let grid = (segs.len() > SCAN_SEGMENTS).then(|| Grid::new(&segs, MIN_CELL_M));
        Cutters { segs, ids, bb, grid }
    }

    /// Calls `f` with every segment that may lie within `r` of the box (x0, y0, x1, y1).
    fn visit(&self, b: [f64; 4], r: f64, f: impl FnMut(usize)) {
        if b[2] < self.bb[0] - r || b[0] > self.bb[2] + r || b[3] < self.bb[1] - r || b[1] > self.bb[3] + r {
            return;
        }
        match &self.grid {
            Some(g) => g.in_box(b, r, f),
            None => (0..self.segs.len()).for_each(f),
        }
    }

    /// Whether segment ab comes within `r` of a cut line.
    fn near_segment(&self, a: P2, b: P2, r: f64) -> bool {
        let bx = [a[0].min(b[0]) - r, a[1].min(b[1]) - r, a[0].max(b[0]) + r, a[1].max(b[1]) + r];
        let mut hit = false;
        self.visit(bx, 0.0, |k| {
            let [c, d] = self.segs[k];
            hit = hit
                || !(c[0].max(d[0]) < bx[0] || c[0].min(d[0]) > bx[2] || c[1].max(d[1]) < bx[1] || c[1].min(d[1]) > bx[3])
                    && seg_seg_dist(a, b, c, d) <= r;
        });
        hit
    }
}

/// Distance between segments ab and cd.
fn seg_seg_dist(a: P2, b: P2, c: P2, d: P2) -> f64 {
    let side = |p: P2, q: P2, r: P2| (q[0] - p[0]) * (r[1] - p[1]) - (q[1] - p[1]) * (r[0] - p[0]);
    let (s1, s2, s3, s4) = (side(a, b, c), side(a, b, d), side(c, d, a), side(c, d, b));
    if (s1 > 0.0) != (s2 > 0.0) && (s3 > 0.0) != (s4 > 0.0) {
        return 0.0;
    }
    seg_dist(a, c, d).0.min(seg_dist(b, c, d).0).min(seg_dist(c, a, b).0).min(seg_dist(d, a, b).0)
}

impl CutIndex {
    /// The target vertex nearest to `q` within `r`.
    fn nearest_vertex(&self, q: P2, r: f64) -> Option<P2> {
        let mut best: Option<(f64, usize)> = None;
        self.vertex_grid.near(q, r, |k| {
            let d = dist(q, self.edges[k / 2].m[k % 2]);
            if d <= r && best.is_none_or(|(bd, bk)| d < bd || (d == bd && k < bk)) {
                best = Some((d, k));
            }
        });
        best.map(|(_, k)| self.edges[k / 2].m[k % 2])
    }

    /// The target edge nearest to `q` within `r`.
    fn nearest_edge(&self, q: P2, r: f64) -> Option<usize> {
        let mut best: Option<(f64, usize)> = None;
        self.edge_grid.near(q, r, |k| {
            let [a, b] = self.edges[k].m;
            let d = seg_dist(q, a, b).0;
            if d <= r && best.is_none_or(|(bd, bk)| d < bd || (d == bd && k < bk)) {
                best = Some((d, k));
            }
        });
        best.map(|(_, k)| k)
    }

    /// Whether segment ab lies along an edge of target `owner` (both ends within
    /// [`EDGE_TOL_M`] of it).
    fn along_edge_of(&self, owner: usize, a: P2, b: P2) -> bool {
        let mut hit = false;
        self.edge_grid.near(lerp(a, b, 0.5), EDGE_TOL_M, |k| {
            let e = &self.edges[k];
            hit = hit
                || (e.owner == owner
                    && seg_dist(a, e.m[0], e.m[1]).0 <= EDGE_TOL_M
                    && seg_dist(b, e.m[0], e.m[1]).0 <= EDGE_TOL_M);
        });
        hit
    }

    fn on_boundary_of(&self, owner: usize, q: P2) -> bool {
        let mut hit = false;
        self.edge_grid.near(q, EDGE_TOL_M, |k| {
            let e = &self.edges[k];
            hit = hit || (e.owner == owner && seg_dist(q, e.m[0], e.m[1]).0 <= EDGE_TOL_M);
        });
        hit
    }

    /// `q` projected onto edge `k`.
    fn onto(&self, k: usize, q: P2) -> P2 {
        let [a, b] = self.edges[k].m;
        lerp(a, b, seg_dist(q, a, b).1.clamp(0.0, 1.0))
    }

    /// `m` (near edge `k`) exactly on the edge: at a cutter vertex there (snapped onto the
    /// edge), else at a cutter's crossing with it, else at its projection.
    fn on_edge(&self, k: usize, m: P2) -> P2 {
        let e = self.edges[k].m;
        let mut best: Option<(u8, f64, usize, P2)> = None;
        let mut offer = |rank: u8, d: f64, i: usize, p: P2| {
            if d <= EDGE_TOL_M && best.is_none_or(|(br, bd, bi, _)| (rank, d, i) < (br, bd, bi)) {
                best = Some((rank, d, i, p));
            }
        };
        self.cutters.visit([m[0], m[1], m[0], m[1]], EDGE_TOL_M, |i| {
            let c = self.cutters.segs[i];
            for v in c {
                offer(0, dist(v, m), i, v);
            }
            if let Some(x) = crossing(e, c) {
                offer(1, dist(x, m), i, x);
            }
        });
        best.map_or_else(|| self.onto(k, m), |b| b.3)
    }
}

impl Placer {
    pub fn new(frame: Frame) -> Placer {
        Placer {
            frame,
            targets: VertexSnap::new(frame, SNAP_TOL_M),
            tools: VertexSnap::new(frame, SNAP_TOL_M),
            owners: Vec::new(),
            ix: CutIndex {
                bboxes: Vec::new(),
                placed: RefCell::default(),
                edges: Vec::new(),
                edge_grid: Grid::new(&[], 0.0),
                vertex_grid: Grid::new(&[], 0.0),
                cutters: Cutters::new(&[]),
            },
        }
    }

    pub fn frame(&self) -> Frame {
        self.frame
    }

    /// Projects an edited feature's lon/lat geometry and registers its vertices.
    pub fn add_target(&mut self, lonlat: &Geom) -> Geom {
        self.owners.push(lonlat.dim());
        self.targets.project(lonlat)
    }

    /// Projects a tool's lon/lat geometry and registers its vertices: result vertices at a
    /// tool vertex get the tool's own coordinates.
    pub fn add_tool(&mut self, lonlat: &Geom) -> Geom {
        self.tools.project(lonlat)
    }

    /// Indexes the edges of the targets (`metric`, as [`Placer::add_target`] returned them,
    /// in the same order) within `reach` of the cut lines.
    fn index(&self, metric: &[&Geom], cutters: Cutters, reach: f64) -> CutIndex {
        let cut = &cutters;
        let (mut edges, mut bboxes) = (Vec::new(), Vec::with_capacity(metric.len()));
        let c = cut.bb;
        let (x0, y0, x1, y1) = (c[0] - reach, c[1] - reach, c[2] + reach, c[3] + reach);
        let boxes: Vec<[f64; 4]> = cut
            .segs
            .iter()
            .map(|[p, q]| [lo(p[0], q[0]) - reach, lo(p[1], q[1]) - reach, hi(p[0], q[0]) + reach, hi(p[1], q[1]) + reach])
            .collect();
        // This runs over every target vertex, the far ones (most) with comparisons only. An
        // edge is near only if it overlaps the box of a cut segment.
        for (owner, g) in metric.iter().enumerate() {
            let mut bb = [f64::MAX, f64::MAX, f64::MIN, f64::MIN];
            for (_, _, cs) in paths(g) {
                let mut prev = 15u8;
                for i in 0..cs.len() {
                    let q = cs[i];
                    bb = [lo(bb[0], q.x), lo(bb[1], q.y), hi(bb[2], q.x), hi(bb[3], q.y)];
                    // which sides of the cut lines' box the vertex lies beyond
                    let side = (q.x < x0) as u8 | ((q.x > x1) as u8) << 1 | ((q.y < y0) as u8) << 2 | ((q.y > y1) as u8) << 3;
                    let far = prev & side != 0;
                    prev = side;
                    if far || i == 0 {
                        continue;
                    }
                    self.consider(owner, cut, &boxes, reach, [p2(cs[i - 1]), p2(q)], &mut edges);
                }
            }
            bboxes.push(bb);
        }
        let segs: Vec<[P2; 2]> = edges.iter().map(|e: &Edge| e.m).collect();
        let edge_grid = Grid::new(&segs, mean_len(&segs));
        let pts: Vec<[P2; 2]> = edges.iter().flat_map(|e| [[e.m[0]; 2], [e.m[1]; 2]]).collect();
        let vertex_grid = Grid::new(&pts, mean_len(&segs));
        CutIndex { bboxes, placed: RefCell::default(), edges, edge_grid, vertex_grid, cutters }
    }

    /// Registers target edge ab if it comes within `reach` of a cut line. Out of line: the
    /// scan in [`Placer::index`] runs over every target vertex and rarely gets here.
    #[inline(never)]
    fn consider(&self, owner: usize, cut: &Cutters, boxes: &[[f64; 4]], reach: f64, [a, b]: [P2; 2], edges: &mut Vec<Edge>) {
        if a == b {
            return;
        }
        let (ex0, ey0, ex1, ey1) = (lo(a[0], b[0]), lo(a[1], b[1]), hi(a[0], b[0]), hi(a[1], b[1]));
        let near = match &cut.grid {
            Some(_) => cut.near_segment(a, b, reach),
            None => boxes.iter().zip(&cut.segs).any(|(bx, [u, v])| {
                !(ex1 < bx[0] || ex0 > bx[2] || ey1 < bx[1] || ey0 > bx[3]) && seg_seg_dist(a, b, *u, *v) <= reach
            }),
        };
        if !near {
            return;
        }
        let ll = |c: P2| {
            let c = Coord { x: c[0], y: c[1] };
            self.targets.snapped(c).unwrap_or_else(|| self.frame.inv_coord(c))
        };
        let (la, lb) = (ll(a), ll(b));
        let dim = self.owners[owner];
        edges.push(if (la.x, la.y) <= (lb.x, lb.y) {
            Edge { m: [a, b], ll: [la, lb], dim, owner }
        } else {
            Edge { m: [b, a], ll: [lb, la], dim, owner }
        });
    }

    /// The index for cut lines (metres) that are not snapped, through targets `metric` (see
    /// [`Placer::snap_paths`]), for [`Placer::unproject_cut`].
    pub fn cut_index(&self, metric: &[&Geom], paths: &[Vec<P2>]) -> CutIndex {
        self.index(metric, Cutters::new(paths), EDGE_TOL_M)
    }

    /// The snapped tool paths as target `owner` (in [`Placer::add_target`] order) is to be
    /// cut by them: without the stretches that run along its edges, which split nothing but
    /// would leave the boolean ops a zero-width sliver to round, and with every open end on
    /// its boundary carried `reach` metres on, so that the end crosses the boundary however
    /// the boolean ops round it.
    pub fn cutters_for(&self, owner: usize, paths: &[Vec<P2>], reach: f64) -> Vec<Vec<P2>> {
        let (bb, ix) = (self.ix.bboxes[owner], &self.ix);
        let r = 2.0 * EDGE_TOL_M;
        let near = |q: P2| q[0] >= bb[0] - r && q[0] <= bb[2] + r && q[1] >= bb[1] - r && q[1] <= bb[3] + r;
        let mut out = Vec::new();
        for p in paths.iter().filter(|p| p.len() >= 2) {
            let mut cur = vec![p[0]];
            for w in p.windows(2) {
                if near(w[0]) && near(w[1]) && ix.along_edge_of(owner, w[0], w[1]) {
                    if cur.len() >= 2 {
                        out.push(std::mem::take(&mut cur));
                    }
                    cur = vec![w[1]];
                } else {
                    cur.push(w[1]);
                }
            }
            if cur.len() >= 2 {
                out.push(cur);
            }
        }
        for p in out.iter_mut() {
            let n = p.len();
            if p[0] == p[n - 1] {
                continue;
            }
            for (end, next) in [(0, 1), (n - 1, n - 2)] {
                let (q, o) = (p[end], p[next]);
                let l = dist(q, o);
                if l > 0.0 && near(q) && ix.on_boundary_of(owner, q) {
                    p[end] = [q[0] + (q[0] - o[0]) * reach / l, q[1] + (q[1] - o[1]) * reach / l];
                }
            }
        }
        out
    }

    /// Snaps tool paths (metres; rings closed) to the targets (`metric`, as
    /// [`Placer::add_target`] returned them, in the same order; see the module
    /// documentation) and registers them as the [`Cutters`] of [`Placer::unproject`].
    pub fn snap_paths(&mut self, metric: &[&Geom], paths: &mut [Vec<P2>]) {
        // the snapped paths stay within TOOL_SNAP_M of these
        let mut ix = self.index(metric, Cutters::new(paths), 2.0 * TOOL_SNAP_M);
        for p in paths.iter_mut() {
            for q in p.iter_mut() {
                if let Some(v) = ix.nearest_vertex(*q, TOOL_SNAP_M) {
                    *q = v;
                } else if let Some(k) = ix.nearest_edge(*q, TOOL_SNAP_M) {
                    *q = ix.onto(k, *q);
                }
            }
        }
        let tool = Cutters::new(paths);
        let r = TOOL_SNAP_M;
        let mut ins: Vec<(usize, usize, f64, P2)> = Vec::new();
        for e in &ix.edges {
            for v in e.m {
                tool.visit([v[0], v[1], v[0], v[1]], r, |k| {
                    let [a, b] = tool.segs[k];
                    if v[0] < a[0].min(b[0]) - r
                        || v[0] > a[0].max(b[0]) + r
                        || v[1] < a[1].min(b[1]) - r
                        || v[1] > a[1].max(b[1]) + r
                        || dist(v, a) <= r
                        || dist(v, b) <= r
                    {
                        return;
                    }
                    let (d, t) = seg_dist(v, a, b);
                    if d <= r && t > 0.0 && t < 1.0 {
                        ins.push((tool.ids[k].0, tool.ids[k].1, t, v));
                    }
                });
            }
        }
        ins.sort_by(|x, y| (x.0, x.1).cmp(&(y.0, y.1)).then(x.2.total_cmp(&y.2)));
        ins.dedup_by(|y, x| (x.0, x.1, x.3) == (y.0, y.1, y.3));
        let mut it = ins.into_iter().peekable();
        for (pi, p) in paths.iter_mut().enumerate() {
            let mut out = Vec::with_capacity(p.len());
            for i in 0..p.len() {
                out.push(p[i]);
                while let Some(&(_, _, _, v)) = it.peek().filter(|x| x.0 == pi && x.1 == i) {
                    out.push(v);
                    it.next();
                }
            }
            out.dedup();
            *p = out;
        }
        ix.cutters = Cutters::new(paths);
        self.ix = ix;
    }

    /// Metres -> lon/lat for one result position; a vertex placed on an original edge is
    /// added to `new`.
    fn place(&self, ix: &CutIndex, m: Coord<f64>, new: &mut Vec<NewVertex>) -> Coord<f64> {
        match self.targets.snapped(m) {
            Some(ll) => ll,
            None => self.place_new(ix, m, new),
        }
    }

    // out of line: `place` runs for every result vertex, almost all of them original
    #[inline(never)]
    fn place_new(&self, ix: &CutIndex, m: Coord<f64>, new: &mut Vec<NewVertex>) -> Coord<f64> {
        if let Some(k) = ix.nearest_edge(p2(m), EDGE_TOL_M) {
            let key = (m.x.to_bits(), m.y.to_bits(), k);
            let cached = ix.placed.borrow().get(&key).copied();
            let at = cached.unwrap_or_else(|| {
                let at = inv_onto(self.frame, ix.on_edge(k, p2(m)), ix.edges[k].m);
                ix.placed.borrow_mut().insert(key, at);
                at
            });
            let e = &ix.edges[k];
            new.push(NewVertex { dim: e.dim, a: e.ll[0], b: e.ll[1], at, owner: e.owner });
            return at;
        }
        self.tools.snapped(m).unwrap_or_else(|| self.frame.inv_coord(m))
    }

    /// A metric result back to lon/lat; the vertices placed on original edges are added to
    /// `new`.
    pub fn unproject(&self, metric: &Geom, new: &mut Vec<NewVertex>) -> Geom {
        self.unproject_cut(&self.ix, metric, new)
    }

    /// [`Placer::unproject`] for the cut lines of `ix` instead of the snapped tools.
    pub fn unproject_cut(&self, ix: &CutIndex, metric: &Geom, new: &mut Vec<NewVertex>) -> Geom {
        metric.map_coords_mut(|c| self.place(ix, c, new))
    }
}

// ---------- topological editing ----------

/// Where [`plan`] looks for the features that share the edges of the new vertices.
pub struct Scope<'a> {
    /// `host.layers`, as the command read them.
    pub layers: &'a [LayerInfo],
    /// The layers of the edited features: searched first, visible or not. The other visible
    /// layers of the same dimension follow.
    pub edited: Vec<i64>,
    /// (layer, feature id) of the features the command writes, deletes or uses as tools:
    /// never neighbours.
    pub exclude: Vec<(i64, i64)>,
    /// Estimated fuel the command spent reading and editing (see [`budget::edit_fuel`]).
    pub spent: u64,
    /// A layer the command already read whole to find its targets: reading it again costs
    /// as much again.
    pub read_whole: Option<i64>,
}

/// Fuel the command may have spent, reading neighbours included, before its geometry work:
/// what [`budget::check_edit`] allows plus a share of the rest.
const NEIGHBOUR_FUEL_LIMIT: u64 = budget::COMMAND_FUEL * 6 / 10;

/// Margin of the lon/lat pre-filter, in degrees (~0.1 m): far above [`EDGE_TOL_M`].
const LONLAT_MARGIN: f64 = 1e-6;

struct Insert {
    layer: i64,
    row: usize,
    part: usize,
    ring: usize,
    index: usize,
    t: f64,
    at: Coord<f64>,
}

/// The vertices to add to neighbouring features, from [`plan`].
#[derive(Default)]
pub struct Shared {
    inserts: Vec<Insert>,
    /// Names of the layers that were too large to read in the same command.
    pub skipped: Vec<String>,
}

impl Shared {
    pub fn vertices(&self) -> usize {
        self.inserts.len()
    }

    pub fn features(&self) -> usize {
        let mut f: Vec<(i64, usize)> = self.inserts.iter().map(|i| (i.layer, i.row)).collect();
        f.sort_unstable();
        f.dedup();
        f.len()
    }

    /// Inserts the vertices. Call before deleting any feature of the layers searched: the
    /// rows are those read by [`plan`].
    pub fn apply(mut self) -> Res<()> {
        // from the end of each ring, so that the indices before stay valid
        self.inserts.sort_by(|x, y| {
            (y.layer, y.row, y.part, y.ring, y.index).cmp(&(x.layer, x.row, x.part, x.ring, x.index)).then(y.t.total_cmp(&x.t))
        });
        for i in &self.inserts {
            host_io::insert_vertex(i.layer, i.row, i.part, i.ring, i.index, i.at.x, i.at.y)?;
        }
        Ok(())
    }

    /// What to add to the command's message ("" when nothing was shared or skipped).
    pub fn note(&self) -> String {
        note(self.vertices(), self.features(), &self.skipped)
    }
}

/// `message` with a [`note`] appended: a sentence ending in "." gets the note before its
/// full stop ("… m² (ellipsoidal); added 2 shared vertices …."), not after it.
pub fn with_note(message: &str, note: &str) -> String {
    if note.is_empty() {
        return message.to_string();
    }
    match message.strip_suffix('.') {
        Some(m) => format!("{m}{note}."),
        None => format!("{message}{note}"),
    }
}

/// The message suffix for `vertices` shared with `features` and the `skipped` layers.
pub fn note(vertices: usize, features: usize, skipped: &[String]) -> String {
    let mut s = String::new();
    if vertices > 0 {
        s += &format!("; added {vertices} shared vertices to {features} neighbouring feature(s)");
    }
    for name in skipped {
        s += &format!(
            "; shared vertices not added in '{name}': the layer is too large to read in the same command \
             (select fewer features or turn off Topological editing)"
        );
    }
    s
}

/// New vertices, deduplicated and sorted by longitude, with their edge and position in
/// metres, to find the segments of other features they lie on.
struct Lookup {
    frame: Frame,
    /// Sorted by longitude.
    vs: Vec<Near>,
    /// Metres per degree of longitude, for a rough distance before projecting.
    m_per_lon: f64,
}

/// Metres per degree of latitude (roughly).
const M_PER_DEG: f64 = 111_320.0;

/// A [`NewVertex`] for [`Lookup`].
struct Near {
    /// Metric edge end a, end b, position.
    m: [P2; 3],
    at: Coord<f64>,
    dim: Dim,
    owner: usize,
}

/// No owner: every vertex is looked at.
const ANY: usize = usize::MAX;

impl Lookup {
    fn new(frame: Frame, new: &[NewVertex]) -> Lookup {
        let mut new: Vec<NewVertex> = new.to_vec();
        new.sort_by(|x, y| x.at.x.total_cmp(&y.at.x).then(x.at.y.total_cmp(&y.at.y)).then((x.dim as u8).cmp(&(y.dim as u8))));
        new.dedup_by(|y, x| x.at == y.at && x.dim == y.dim);
        let f = |c: Coord<f64>| p2(frame.fwd_coord(c));
        let vs: Vec<Near> =
            new.iter().map(|v| Near { m: [f(v.a), f(v.b), f(v.at)], at: v.at, dim: v.dim, owner: v.owner }).collect();
        let lat = vs.iter().map(|v| v.at.y).sum::<f64>() / vs.len().max(1) as f64;
        Lookup { frame, vs, m_per_lon: M_PER_DEG * lat.to_radians().cos() }
    }

    /// Whether `g` may hold a vertex of its dimension not of `owner` on its boundary.
    fn may_touch(&self, g: &Geom, owner: usize) -> bool {
        let (m, dim) = (LONLAT_MARGIN, g.dim());
        let Some(b) = g.bbox() else { return false };
        let (x0, y0, x1, y1) = (b.min().x - m, b.min().y - m, b.max().x + m, b.max().y + m);
        let from = self.vs.partition_point(|v| v.at.x < x0);
        self.vs[from..]
            .iter()
            .take_while(|v| v.at.x <= x1)
            .any(|v| v.dim == dim && v.owner != owner && v.at.y >= y0 && v.at.y <= y1)
    }

    /// The vertices of `dim` that lie on segment cd (lon/lat) of another feature, strictly
    /// between its ends, while cd runs along the vertex's edge: as (parameter on cd, lon/lat).
    fn on_segment(&self, dim: Dim, owner: usize, c: Coord<f64>, d: Coord<f64>, out: &mut Vec<(f64, Coord<f64>)>) {
        let m = LONLAT_MARGIN;
        let (sx0, sx1) = (c.x.min(d.x) - m, c.x.max(d.x) + m);
        let (sy0, sy1) = (c.y.min(d.y) - m, c.y.max(d.y) + m);
        let from = self.vs.partition_point(|v| v.at.x < sx0);
        let mut metric: Option<(P2, P2)> = None;
        for v in self.vs[from..].iter().take_while(|v| v.at.x <= sx1) {
            let at = &v.at;
            if v.dim != dim || v.owner == owner || at.y < sy0 || at.y > sy1 || *at == c || *at == d {
                continue;
            }
            // a metre off the segment in lon/lat is far off it in the frame too: no need to project
            let (ux, uy) = ((d.x - c.x) * self.m_per_lon, (d.y - c.y) * M_PER_DEG);
            let (wx, wy) = ((at.x - c.x) * self.m_per_lon, (at.y - c.y) * M_PER_DEG);
            let l = (ux * ux + uy * uy).sqrt();
            if l == 0.0 || (ux * wy - uy * wx).abs() > l {
                continue;
            }
            let (cm, dm) = *metric.get_or_insert_with(|| (p2(self.frame.fwd_coord(c)), p2(self.frame.fwd_coord(d))));
            let [a, b, p] = v.m;
            if dist(p, cm) <= EDGE_TOL_M || dist(p, dm) <= EDGE_TOL_M {
                continue;
            }
            let (dd, t) = seg_dist(p, cm, dm);
            if dd > EDGE_TOL_M || t <= 0.0 || t >= 1.0 {
                continue;
            }
            if line_dist(cm, a, b) > TOOL_SNAP_M || line_dist(dm, a, b) > TOOL_SNAP_M {
                continue;
            }
            out.push((t, *at));
        }
    }
}

/// Adds the new vertices to the command's own results (lon/lat) where they lie on their
/// edges: a piece of one target then has the vertices the cut put on the edge it shares with
/// another target, as the unedited neighbours get them from [`plan`].
///
/// `results` are (owner, lon/lat result), owner as in [`NewVertex::owner`].
pub fn share_within<'a>(frame: Frame, results: impl IntoIterator<Item = (usize, &'a mut Geom)>, new: &[NewVertex]) {
    if new.is_empty() {
        return;
    }
    let look = Lookup::new(frame, new);
    let fix = |l: &mut geo::LineString<f64>, dim: Dim, owner: usize| {
        // (segment, parameter, position)
        let mut ins: Vec<(usize, f64, Coord<f64>)> = Vec::new();
        let mut hits = Vec::new();
        for i in 1..l.0.len() {
            hits.clear();
            look.on_segment(dim, owner, l.0[i - 1], l.0[i], &mut hits);
            ins.extend(hits.iter().map(|&(t, at)| (i, t, at)));
        }
        if ins.is_empty() {
            return;
        }
        ins.sort_by(|x, y| x.0.cmp(&y.0).then(x.1.total_cmp(&y.1)));
        let mut out = Vec::with_capacity(l.0.len() + ins.len());
        let mut it = ins.into_iter().peekable();
        for (i, c) in l.0.iter().enumerate() {
            while let Some(&(_, _, at)) = it.peek().filter(|x| x.0 == i) {
                out.push(at);
                it.next();
            }
            out.push(*c);
        }
        l.0 = out;
    };
    let poly = |p: &mut geo::Polygon<f64>, dim: Dim, o: usize| {
        p.exterior_mut(|l| fix(l, dim, o));
        p.interiors_mut(|ls| ls.iter_mut().for_each(|l| fix(l, dim, o)));
    };
    for (o, g) in results {
        let dim = g.dim();
        if !look.may_touch(g, o) {
            continue;
        }
        match g {
            Geom::LineString(l) => fix(l, dim, o),
            Geom::MultiLineString(m) => m.0.iter_mut().for_each(|l| fix(l, dim, o)),
            Geom::Polygon(p) => poly(p, dim, o),
            Geom::MultiPolygon(m) => m.0.iter_mut().for_each(|p| poly(p, dim, o)),
            Geom::Point(_) | Geom::MultiPoint(_) => {}
        }
    }
}

/// Finds, for each new vertex placed on an original edge, the features that are not edited
/// and whose boundary runs along that edge through the vertex: a segment of theirs lies on
/// the edge's line (within [`TOOL_SNAP_M`]) and passes within [`EDGE_TOL_M`] of the vertex,
/// between its end points. The vertex is to be inserted there.
///
/// The host cannot read by area, so each layer is read whole: the edited layers first, then
/// the other visible layers of the same dimension, as long as reading them fits the
/// command's budget (a layer that does not is named in [`Shared::skipped`]).
pub fn plan(on: bool, scope: &Scope, new: &[NewVertex]) -> Res<Shared> {
    let mut out = Shared::default();
    if !on || new.is_empty() {
        return Ok(out);
    }
    let frame = Frame::for_positions(new.iter().flat_map(|v| [p2(v.a), p2(v.b), p2(v.at)]))?;
    let dims: Vec<Dim> = new.iter().map(|v| v.dim).collect();
    let mut look: Option<Lookup> = None;
    let mut exclude = scope.exclude.clone();
    exclude.sort_unstable();

    let edited = scope.edited.iter().filter_map(|id| scope.layers.iter().find(|l| l.id == *id));
    let others = scope.layers.iter().filter(|l| l.visible && !scope.edited.contains(&l.id));
    let mut fuel_left = NEIGHBOUR_FUEL_LIMIT.saturating_sub(scope.spent);
    let mut hits = Vec::new();
    for layer in edited.chain(others) {
        let Some(dim) = layer.dim.filter(|_| layer.is_vector()) else { continue };
        // a layer whose every feature is edited has no neighbours to read
        let mine = exclude.iter().filter(|(l, _)| *l == layer.id).count();
        if !dims.contains(&dim) || layer.features <= mine {
            continue;
        }
        let per_byte = budget::READ_FUEL_PER_BYTE * if scope.read_whole == Some(layer.id) { 2 } else { 1 };
        let max_bytes = (fuel_left / per_byte).min(budget::MAX_READ_BYTES as u64) as usize;
        let Some((feats, bytes)) = host_io::all_features_within(layer, LAYER_READ_CAP, max_bytes)? else {
            out.skipped.push(layer.name.clone());
            continue;
        };
        fuel_left = fuel_left.saturating_sub(bytes as u64 * per_byte);
        let look = look.get_or_insert_with(|| Lookup::new(frame, new));
        for f in feats {
            if exclude.binary_search(&(layer.id, f.id)).is_ok() {
                continue;
            }
            let Some(g) = &f.geometry else { continue };
            if g.dim() != dim || !look.may_touch(g, ANY) {
                continue;
            }
            for (part, ring, cs) in paths(g) {
                for i in 1..cs.len() {
                    hits.clear();
                    look.on_segment(dim, ANY, cs[i - 1], cs[i], &mut hits);
                    out.inserts.extend(hits.iter().map(|&(t, at)| Insert {
                        layer: layer.id,
                        row: f.row,
                        part,
                        ring,
                        index: i - 1,
                        t,
                        at,
                    }));
                }
            }
        }
    }
    Ok(out)
}
