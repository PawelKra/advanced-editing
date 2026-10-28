//! Parcel division: equal parts, a cut-off share, and a tool that proposes a division as a
//! new layer.
//!
//! All cuts are parallel straight lines at a bearing (degrees clockwise from north, the
//! direction of the lines themselves). The parts are numbered from the LEFT of the line
//! direction: with 0° (north-south lines) from the west, with 90° (east-west lines) from the
//! north; `reverse` starts from the other side. Cut Off Percent cuts its share off that start
//! side. Bearings are grid bearings of the working frame (EPSG:2180 or UTM), which differ
//! from true north by the meridian convergence.
//!
//! Each cut position is found by root finding on an area functional that clips the rings
//! against a half-plane (Sutherland-Hodgman) and integrates them with Green's theorem; the
//! geometry is then cut once, by all lines together. The cut positions are cumulative (the
//! k-th cut leaves k/n of the area behind it), so errors do not add up along the sweep. A straight
//! cut through a concave or multi-part parcel can give a part made of several pieces: such a
//! part stays one feature, a MultiPolygon.
//!
//! Areas are ellipsoidal by default: the plane of the working frame (EPSG:2180 or UTM) is
//! conformal, so the ellipsoid area of an element is its frame area divided by k², the point
//! scale factor squared. 1/k² is linearised around the parcel, which makes the integral exact
//! for a linear density: the frame area divided by k² at each part's own centroid. Planar
//! mode measures in the frame.

use geo::{Coord, MultiPolygon, Polygon};
use i_overlay::core::fill_rule::FillRule;
use i_overlay::float::slice::FloatSlice;
use rustgis_plugin::{Map, Request, ToolOutput, Value, json};

use crate::geojson::{self, Dim, Geom, P2};
use crate::host_io::{self, Res, budget};
use crate::proj::{Frame, FrameKind, GRS80_A, GRS80_F, WGS84_F};
use crate::selection::{EditSpec, Selection, active_features};
use crate::topo::{self, NewVertex, Placer};
use crate::Ctx;

const USAGE: &str = "Select the polygons to divide in the active layer";

/// Most parts one parcel may be divided into (a guard against a tiny area or width).
pub const MAX_PARTS: usize = 10_000;

/// Most parts a script may ask Divide Into Equal Parts for (the panel slider goes to 50).
const MAX_COMMAND_PARTS: usize = 1000;

/// A number of equal parts given as `x`: refused unless it is a whole number from 2 to `max`.
fn whole_parts(name: &str, x: f64, max: usize) -> Res<usize> {
    if x.fract() != 0.0 || !(2.0..=max as f64).contains(&x) {
        return Err(format!("{name} must be a whole number from 2 to {max}, not {x}"));
    }
    Ok(x as usize)
}

/// Fuel to divide one polygon of `v` vertices into `p` parts, without reading or writing it:
/// an upper bound fitted to `diag-bench` runs on wavy and comb-shaped rings of 20 to 12 000
/// vertices cut into 2 to 50 parts. Cuts through a ring that zigzags across them cost the
/// most, hence the large `v * p` term.
fn divide_fuel(v: usize, p: usize) -> u64 {
    let (v, p) = (v as u64, p as u64);
    22_000 * v + 3_000 * v * p + 200_000 * p
}

/// Refuses dividing `parcels` (vertices, parts) when reading them, dividing them and writing
/// the parts would not fit in `fuel`. Each cut adds about two vertices to each side.
fn check_division(parcels: &[(usize, usize)], fuel: u64, too_much: impl Fn(usize, usize, usize) -> String) -> Res<()> {
    let vertices: usize = parcels.iter().map(|&(v, _)| v).sum();
    let parts: usize = parcels.iter().map(|&(_, p)| p).sum();
    let compute: u64 = parcels.iter().map(|&(v, p)| divide_fuel(v, p)).sum();
    let need = budget::edit_fuel(parcels.len(), vertices) + compute + budget::edit_fuel(parts, vertices + 4 * parts);
    if need <= fuel { Ok(()) } else { Err(too_much(parcels.len(), vertices, parts)) }
}

/// Fields that identify a feature and are not copied to the new parts.
const ID_FIELDS: [&str; 3] = ["id", "fid", "ogc_fid"];

/// How areas are measured.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AreaMode {
    /// On the ellipsoid (GRS80 / WGS84): what a cadastral area means.
    Ellipsoidal,
    /// In the working frame's plane (EPSG:2180 or UTM).
    Planar,
}

impl AreaMode {
    pub fn name(self) -> &'static str {
        match self {
            AreaMode::Ellipsoidal => "ellipsoidal",
            AreaMode::Planar => "planar",
        }
    }
}

/// The accuracy every part's area must reach: 0.01 m² or 1e-6 of the part, the larger.
pub fn area_tolerance(area: f64) -> f64 {
    (1e-6 * area.abs()).max(0.01)
}

/// The unit normal of cut lines at `bearing_deg`, pointing along the sweep: the parts follow
/// each other in increasing `n·p`, starting on the left of the line direction.
pub fn sweep_normal(bearing_deg: f64, reverse: bool) -> P2 {
    let b = bearing_deg.to_radians();
    let n = [b.cos(), -b.sin()];
    if reverse { [-n[0], -n[1]] } else { n }
}

// ---------- area functional ----------

/// Signed area and first moments of a closed (cyclic) list of positions.
#[derive(Clone, Copy, Debug, Default)]
struct Moments {
    a: f64,
    sx: f64,
    sy: f64,
}

impl Moments {
    fn of(pts: &[Coord<f64>]) -> Moments {
        let mut m = Moments::default();
        let n = pts.len();
        if n < 3 {
            return m;
        }
        for i in 0..n {
            let (p, q) = (pts[i], pts[(i + 1) % n]);
            let c = p.x * q.y - q.x * p.y;
            m.a += c;
            m.sx += (p.x + q.x) * c;
            m.sy += (p.y + q.y) * c;
        }
        Moments { a: m.a / 2.0, sx: m.sx / 6.0, sy: m.sy / 6.0 }
    }

    fn scaled(self, w: f64) -> Moments {
        Moments { a: self.a * w, sx: self.sx * w, sy: self.sy * w }
    }

    fn add(&mut self, o: Moments) {
        self.a += o.a;
        self.sx += o.sx;
        self.sy += o.sy;
    }
}

/// Area density of the frame plane, linear in local coordinates: ∫ g dA is the area.
#[derive(Clone, Copy, Debug)]
struct Density {
    g0: f64,
    gx: f64,
    gy: f64,
}

impl Density {
    const PLANAR: Density = Density { g0: 1.0, gx: 0.0, gy: 0.0 };

    /// 1/k² around `origin` (frame metres), from central differences over `step` metres.
    fn ellipsoidal(frame: &Frame, origin: Coord<f64>, step: f64) -> Density {
        let g = |dx: f64, dy: f64| 1.0 / scale_factor(frame, origin.x + dx, origin.y + dy).powi(2);
        Density {
            g0: g(0.0, 0.0),
            gx: (g(step, 0.0) - g(-step, 0.0)) / (2.0 * step),
            gy: (g(0.0, step) - g(0.0, -step)) / (2.0 * step),
        }
    }

    fn measure(&self, m: Moments) -> f64 {
        self.g0 * m.a + self.gx * m.sx + self.gy * m.sy
    }

    /// The density at a local point (for converting ground lengths).
    fn at(&self, x: f64, y: f64) -> f64 {
        self.g0 + self.gx * x + self.gy * y
    }
}

/// The point scale factor of the frame at (x, y) metres, from the east-west length of a
/// short parallel arc (the projection is conformal, so any direction gives the same k).
fn scale_factor(frame: &Frame, x: f64, y: f64) -> f64 {
    let f = match frame.kind {
        FrameKind::Epsg2180 => GRS80_F,
        FrameKind::Utm { .. } => WGS84_F,
    };
    let e2 = f * (2.0 - f);
    let (lon, lat) = frame.inv(x, y);
    let h = 1e-4;
    let (x1, y1) = frame.fwd(lon - h, lat);
    let (x2, y2) = frame.fwd(lon + h, lat);
    let phi = lat.to_radians();
    let nu = GRS80_A / (1.0 - e2 * phi.sin().powi(2)).sqrt();
    let ground = nu * phi.cos() * (2.0 * h).to_radians();
    (x2 - x1).hypot(y2 - y1) / ground
}

/// The rings of a multipolygon with the weight that makes outer rings count positive and
/// holes negative whatever their orientation.
fn weighted_rings(mp: &MultiPolygon<f64>) -> Vec<(Vec<Coord<f64>>, f64)> {
    let mut out = Vec::new();
    for p in &mp.0 {
        for (k, r) in std::iter::once(p.exterior()).chain(p.interiors()).enumerate() {
            let mut pts = r.0.clone();
            if pts.len() > 1 && pts.first() == pts.last() {
                pts.pop();
            }
            let s = Moments::of(&pts).a.signum();
            if s != 0.0 {
                out.push((pts, if k == 0 { s } else { -s }));
            }
        }
    }
    out
}

/// Clips a cyclic ring to {n·p <= t} (Sutherland-Hodgman). The result may run along the cut
/// line back and forth for concave rings, which Green's theorem integrates correctly.
fn clip_ring(pts: &[Coord<f64>], n: P2, t: f64, out: &mut Vec<Coord<f64>>) {
    out.clear();
    let f = |c: &Coord<f64>| n[0] * c.x + n[1] * c.y - t;
    let len = pts.len();
    for i in 0..len {
        let (a, b) = (pts[i], pts[(i + 1) % len]);
        let (fa, fb) = (f(&a), f(&b));
        if fa <= 0.0 {
            out.push(a);
        }
        if (fa < 0.0 && fb > 0.0) || (fa > 0.0 && fb < 0.0) {
            let s = fa / (fa - fb);
            out.push(Coord { x: a.x + (b.x - a.x) * s, y: a.y + (b.y - a.y) * s });
        }
    }
}

// ---------- one parcel ----------

/// A parcel prepared for division: projected to its frame and moved to local coordinates
/// (around its bounding-box centre) so that root finding and slicing work on small numbers.
pub struct Parcel {
    placer: Placer,
    /// The parcel in the frame (as `local`, but not moved).
    metric: Geom,
    origin: Coord<f64>,
    local: MultiPolygon<f64>,
    rings: Vec<(Vec<Coord<f64>>, f64)>,
    dens: Density,
    /// The parcel's area (per the area mode).
    pub area: f64,
}

/// How the parts are cut.
#[derive(Clone, Copy, Debug)]
pub enum Plan {
    /// `n` parts of equal area.
    Equal(usize),
    /// One part of this share (0..1) of the area, then the remainder.
    Share(f64),
    /// Parts of this area (m²), the remainder last.
    Area(f64),
    /// Strips this wide (m, across the lines), the remainder last.
    Width(f64),
}

/// One finished part.
pub struct Part {
    /// Lon/lat, ready to write.
    pub geom: Geom,
    /// Its area per the area mode, measured on the cut geometry.
    pub area: f64,
    /// The area it was cut for (`None` for a remainder or a strip).
    pub target: Option<f64>,
    /// The vertices the cuts put on the parcel's edges.
    pub new_vertices: Vec<NewVertex>,
}

impl Part {
    pub fn error(&self) -> f64 {
        self.target.map_or(0.0, |t| (self.area - t).abs())
    }
}

impl Parcel {
    pub fn new(g: &Geom, mode: AreaMode) -> Res<Parcel> {
        if g.dim() != Dim::Polygon {
            return Err(format!("a {} is not a polygon", g.type_name()));
        }
        let frame = Frame::for_geoms(std::iter::once(g))?;
        let mut placer = Placer::new(frame);
        let metric_geom = placer.add_target(g);
        let metric = metric_geom.to_multi_polygon().expect("a polygon");
        let (mut lo, mut hi) = ([f64::INFINITY; 2], [f64::NEG_INFINITY; 2]);
        for p in &metric.0 {
            for c in &p.exterior().0 {
                lo = [lo[0].min(c.x), lo[1].min(c.y)];
                hi = [hi[0].max(c.x), hi[1].max(c.y)];
            }
        }
        if !lo[0].is_finite() {
            return Err("the polygon is empty".into());
        }
        let origin = Coord { x: 0.5 * (lo[0] + hi[0]), y: 0.5 * (lo[1] + hi[1]) };
        let local = MultiPolygon(
            metric.0.iter().map(|p| geo::MapCoords::map_coords(p, |c| c - origin)).collect::<Vec<Polygon<f64>>>(),
        );
        let dens = match mode {
            AreaMode::Planar => Density::PLANAR,
            AreaMode::Ellipsoidal => {
                let step = (0.5 * (hi[0] - lo[0]).max(hi[1] - lo[1])).max(10.0);
                Density::ellipsoidal(&frame, origin, step)
            }
        };
        let rings = weighted_rings(&local);
        let mut m = Moments::default();
        for (r, w) in &rings {
            m.add(Moments::of(r).scaled(*w));
        }
        let area = dens.measure(m);
        if !(area > 0.0) {
            return Err("the polygon has no area".into());
        }
        Ok(Parcel { placer, metric: metric_geom, origin, local, rings, dens, area })
    }

    /// The area of the parcel on the start side of the line {n·p = t}.
    fn area_before(&self, n: P2, t: f64, buf: &mut Vec<Coord<f64>>) -> f64 {
        let mut m = Moments::default();
        for (r, w) in &self.rings {
            clip_ring(r, n, t, buf);
            m.add(Moments::of(buf).scaled(*w));
        }
        self.dens.measure(m)
    }

    /// The range of n·p over the parcel.
    fn extent(&self, n: P2) -> (f64, f64) {
        let mut e = (f64::INFINITY, f64::NEG_INFINITY);
        for (r, _) in &self.rings {
            for c in r {
                let t = n[0] * c.x + n[1] * c.y;
                e = (e.0.min(t), e.1.max(t));
            }
        }
        e
    }

    /// The cut position in [lo, hi] that leaves `target` of the area behind it: regula falsi
    /// with the Illinois modification (the area grows smoothly with the position), falling
    /// back to bisection when the bracket shrinks too slowly.
    fn solve(&self, n: P2, target: f64, (mut lo, alo): (f64, f64), (mut hi, ahi): (f64, f64), buf: &mut Vec<Coord<f64>>) -> f64 {
        // Well below the required accuracy, well above the rounding of the area sums.
        let eps = 1e-3 * area_tolerance(target);
        let (mut flo, mut fhi) = (alo - target, ahi - target);
        if flo >= 0.0 {
            return lo;
        }
        if fhi <= 0.0 {
            return hi;
        }
        let mut side = 0i8;
        for i in 0..200 {
            let width = hi - lo;
            let mut t = if i % 4 == 3 { 0.5 * (lo + hi) } else { lo - flo * (hi - lo) / (fhi - flo) };
            if !(t > lo && t < hi) {
                t = 0.5 * (lo + hi);
                if !(t > lo && t < hi) {
                    break;
                }
            }
            let f = self.area_before(n, t, buf) - target;
            if f.abs() <= eps {
                return t;
            }
            if f < 0.0 {
                (lo, flo) = (t, f);
                if side == -1 {
                    fhi *= 0.5;
                }
                side = -1;
            } else {
                (hi, fhi) = (t, f);
                if side == 1 {
                    flo *= 0.5;
                }
                side = 1;
            }
            if hi - lo >= width {
                break;
            }
        }
        if -flo < fhi { lo } else { hi }
    }

    /// The cut positions and the target area of each part (`None` where none is aimed at).
    fn cuts(&self, n: P2, plan: Plan) -> Res<(Vec<f64>, Vec<Option<f64>>)> {
        let (tmin, tmax) = self.extent(n);
        let mut buf = Vec::new();
        let total = self.area;
        // A table of the area along the sweep brackets every cut closely, so each root takes
        // a few evaluations however many parts there are.
        let cumulative = |targets: &[f64], buf: &mut Vec<Coord<f64>>| {
            let k = (2 * targets.len()).clamp(8, 64);
            let ts: Vec<f64> = (0..=k).map(|i| tmin + (tmax - tmin) * i as f64 / k as f64).collect();
            let fs: Vec<f64> = ts
                .iter()
                .enumerate()
                .map(|(i, &t)| match i {
                    0 => 0.0,
                    i if i == k => total,
                    _ => self.area_before(n, t, buf),
                })
                .collect();
            let mut cuts: Vec<f64> = Vec::with_capacity(targets.len());
            for &c in targets {
                let i = fs.partition_point(|&f| f < c).clamp(1, k);
                let mut lo = (ts[i - 1], fs[i - 1]);
                if let Some(&prev) = cuts.last()
                    && prev > lo.0
                {
                    lo = (prev, self.area_before(n, prev, buf));
                }
                cuts.push(self.solve(n, c, lo, (ts[i], fs[i]), buf));
            }
            cuts
        };
        Ok(match plan {
            Plan::Equal(parts) => {
                if !(2..=MAX_PARTS).contains(&parts) {
                    return Err(format!("the number of parts must be 2 to {MAX_PARTS}"));
                }
                let each = total / parts as f64;
                let targets: Vec<f64> = (1..parts).map(|k| total * k as f64 / parts as f64).collect();
                (cumulative(&targets, &mut buf), vec![Some(each); parts])
            }
            Plan::Share(s) => {
                if !(s > 0.0 && s < 1.0) {
                    return Err("the share must be between 0 and 100 %".into());
                }
                let a = total * s;
                (cumulative(&[a], &mut buf), vec![Some(a), Some(total - a)])
            }
            Plan::Area(a) => {
                if !(a > 0.0) {
                    return Err("the part area must be positive".into());
                }
                let tol = area_tolerance(a);
                if a >= total - tol {
                    return Err(format!("the parcel ({total:.2} m²) is not larger than one part ({a:.2} m²)"));
                }
                let full = ((total + tol) / a).floor() as usize;
                if full >= MAX_PARTS {
                    return Err(format!("{a} m² would give more than {MAX_PARTS} parts"));
                }
                // A remainder below the tolerance is no part of its own.
                let rem = total - full as f64 * a;
                let ncuts = if rem <= tol { full - 1 } else { full };
                let targets: Vec<f64> = (1..=ncuts).map(|k| a * k as f64).collect();
                let mut aims = vec![Some(a); ncuts];
                aims.push(if rem <= tol { Some(a) } else { None });
                (cumulative(&targets, &mut buf), aims)
            }
            Plan::Width(w) => {
                if !(w > 0.0) {
                    return Err("the strip width must be positive".into());
                }
                // Ground width in the ellipsoidal mode: frame lengths are k times longer.
                let c = self.centroid();
                let w = w / self.dens.at(c.x, c.y).sqrt();
                let span = tmax - tmin;
                let k = (span / w * (1.0 - 1e-12)).ceil().max(1.0) as usize;
                if k > MAX_PARTS {
                    return Err(format!("{w} m strips would give more than {MAX_PARTS} parts"));
                }
                if k < 2 {
                    return Err(format!("the parcel is only {span:.2} m wide across the lines"));
                }
                ((1..k).map(|i| tmin + w * i as f64).collect(), vec![None; k])
            }
        })
    }

    /// How many parts `plan` makes at most (for the budget, before cutting).
    fn parts_estimate(&self, n: P2, plan: Plan) -> usize {
        let p = match plan {
            Plan::Equal(k) => k,
            Plan::Share(_) => 2,
            Plan::Area(a) => (self.area / a).ceil() as usize + 1,
            Plan::Width(w) => {
                let (lo, hi) = self.extent(n);
                // a strip is at most 1 % narrower in the frame than on the ground
                ((hi - lo) / (0.99 * w)).ceil() as usize + 1
            }
        };
        p.clamp(2, MAX_PARTS)
    }

    fn centroid(&self) -> Coord<f64> {
        let mut m = Moments::default();
        for (r, w) in &self.rings {
            m.add(Moments::of(r).scaled(*w));
        }
        Coord { x: m.sx / m.a, y: m.sy / m.a }
    }

    /// The cut lines {n·p = t}, each reaching 1 m beyond the parcel on both sides.
    fn cut_lines(&self, n: P2, cuts: &[f64]) -> Vec<Vec<P2>> {
        let d = [-n[1], n[0]];
        let (mut s0, mut s1) = (f64::INFINITY, f64::NEG_INFINITY);
        for (r, _) in &self.rings {
            for c in r {
                let s = d[0] * c.x + d[1] * c.y;
                (s0, s1) = (s0.min(s), s1.max(s));
            }
        }
        let p = |t: f64, s: f64| [n[0] * t + d[0] * s, n[1] * t + d[1] * s];
        cuts.iter().map(|&t| vec![p(t, s0 - 1.0), p(t, s1 + 1.0)]).collect()
    }

    /// Divides the parcel. Parts follow the sweep; a strip that misses the parcel (between
    /// the parts of a multi-part parcel) gives no part.
    ///
    /// All cut lines go into one i_overlay slice: one pass over the parcel, and neighbouring
    /// parts get bit-identical vertices on their common cut. Each slice belongs to the strip
    /// its centroid lies in.
    pub fn divide(&self, n: P2, plan: Plan) -> Res<Vec<Part>> {
        let (cuts, aims) = self.cuts(n, plan)?;
        let shapes: Vec<Vec<Vec<P2>>> = self.local.0.iter().map(geojson::contours).collect();
        let lines = self.cut_lines(n, &cuts);
        let slices = shapes.slice_by(&lines, FillRule::EvenOdd);
        let o = [self.origin.x, self.origin.y];
        let metric_lines: Vec<Vec<P2>> =
            lines.iter().map(|l| l.iter().map(|q| [q[0] + o[0], q[1] + o[1]]).collect()).collect();
        let ix = self.placer.cut_index(&[&self.metric], &metric_lines);
        let mut strips: Vec<Vec<Polygon<f64>>> = vec![Vec::new(); aims.len()];
        for sh in &slices {
            let poly = geojson::shape_to_polygon(sh);
            let rings = weighted_rings(&MultiPolygon(vec![poly.clone()]));
            let mut m = Moments::default();
            for (r, w) in &rings {
                m.add(Moments::of(r).scaled(*w));
            }
            if !(m.a > 0.0) {
                continue;
            }
            let t = n[0] * m.sx / m.a + n[1] * m.sy / m.a;
            strips[cuts.partition_point(|&c| c < t)].push(poly);
        }
        let mut parts = Vec::with_capacity(aims.len());
        for (k, (polys, aim)) in strips.into_iter().zip(aims).enumerate() {
            let local = MultiPolygon(polys);
            let mut m = Moments::default();
            for (r, w) in weighted_rings(&local) {
                m.add(Moments::of(&r).scaled(w));
            }
            let metric = geojson::from_polygons(
                local.0.iter().map(|p| geo::MapCoords::map_coords(p, |c| c + self.origin)).collect(),
            );
            let mut new_vertices = Vec::new();
            let geom = match geojson::prepare_output(self.placer.unproject_cut(&ix, &metric, &mut new_vertices)) {
                Ok(g) => g,
                Err(_) if aim.is_none() => continue,
                Err(e) => return Err(format!("part {}: {e}", k + 1)),
            };
            parts.push(Part { geom, area: self.dens.measure(m), target: aim, new_vertices });
        }
        Ok(parts)
    }
}

// ---------- commands ----------

/// The attributes a new part inherits.
fn inherited(props: &Map<String, Value>) -> Map<String, Value> {
    props.iter().filter(|(k, _)| !ID_FIELDS.contains(&k.to_ascii_lowercase().as_str())).map(|(k, v)| (k.clone(), v.clone())).collect()
}

fn round4(x: f64) -> f64 {
    (x * 1e4).round() / 1e4
}

fn areas_text(parts: &[Part]) -> String {
    let v: Vec<String> = parts.iter().map(|p| format!("{:.2}", p.area)).collect();
    v.join(", ")
}

/// Divides every selected polygon with `plan` and writes the parts: the first part (the last
/// with `keep_last`) replaces the original's geometry, the others are new features with its
/// attributes.
fn divide_selected(ctx: &Ctx, plan: Plan, keep_last: bool) -> Res<(Vec<(i64, Vec<Part>)>, Value)> {
    let spec = EditSpec { properties: true, ..EditSpec::new(USAGE, &[Dim::Polygon]) };
    let ed = active_features(&spec)?;
    let parts = match plan {
        Plan::Equal(k) => k,
        _ => 2,
    };
    let parcels: Vec<(usize, usize)> =
        ed.features.iter().map(|f| (f.geometry.as_ref().map_or(0, Geom::num_coords), parts)).collect();
    check_division(&parcels, budget::COMMAND_FUEL, |n, v, _| {
        format!(
            "Dividing {n} polygon(s) with {} vertices into {parts} parts is more than one command can do. \
             Select fewer polygons, or use the Divide Parcel tool (it handles whole layers).",
            budget::count(v)
        )
    })?;
    let n = sweep_normal(ctx.settings.angle_deg, ctx.settings.reverse);
    let mut done = Vec::with_capacity(ed.features.len());
    for f in &ed.features {
        let fail = |e: String| format!("{USAGE}. (feature {}: {e})", f.id);
        let parcel = Parcel::new(f.geom()?, AreaMode::Ellipsoidal).map_err(fail)?;
        let parts = parcel.divide(n, plan).map_err(fail)?;
        if parts.len() < 2 {
            return Err(fail("the cut misses it".into()));
        }
        done.push((f, parts));
    }
    let layer = ed.layer.id;
    // each parcel is the owner of its parts' new vertices
    let new_vertices: Vec<NewVertex> = done
        .iter()
        .enumerate()
        .flat_map(|(i, (_, ps))| ps.iter().flat_map(move |p| p.new_vertices.iter().map(move |v| NewVertex { owner: i, ..*v })))
        .collect();
    let results = done.iter_mut().enumerate().flat_map(|(i, (_, ps))| ps.iter_mut().map(move |p| (i, &mut p.geom)));
    topo::share_within(ed.frame()?, results, &new_vertices);
    let scope = topo::Scope {
        layers: &ed.layers,
        edited: vec![layer],
        exclude: done.iter().map(|(f, _)| (layer, f.id)).collect(),
        spent: budget::edit_fuel(ed.features.len(), budget::vertices(&ed.features)),
        read_whole: None,
    };
    let shared = topo::plan(ctx.settings.topological, &scope, &new_vertices)?;
    let mut rows = Vec::new();
    let mut created = 0usize;
    for (f, parts) in &done {
        let keep = if keep_last { parts.len() - 1 } else { 0 };
        let attrs = inherited(&f.properties);
        for (i, p) in parts.iter().enumerate() {
            if i == keep {
                host_io::set_geometry(layer, f.row, &p.geom)?;
                rows.push(f.row);
            } else {
                rows.push(host_io::add_feature(layer, &p.geom, &attrs)?);
                created += 1;
            }
        }
    }
    let (sv, sf, skipped) = (shared.vertices(), shared.features(), shared.skipped.clone());
    shared.apply()?;
    Selection::select_results(&[(layer, rows)])?;
    let max_err = done.iter().flat_map(|(_, ps)| ps.iter().map(Part::error)).fold(0.0, f64::max);
    let counts = json!({
        "divided": done.len(),
        "created": created,
        "areaMode": AreaMode::Ellipsoidal.name(),
        "areas": done.iter().map(|(_, ps)| ps.iter().map(|p| round4(p.area)).collect::<Vec<_>>()).collect::<Vec<_>>(),
        "maxError": max_err,
        "sharedVertices": sv,
        "sharedFeatures": sf,
        "sharedSkipped": skipped,
    });
    Ok((done.into_iter().map(|(f, ps)| (f.id, ps)).collect(), counts))
}

/// The shared vertices of [`divide_selected`]'s counts, for the message.
fn shared_note(counts: &Value) -> String {
    let n = |k: &str| counts[k].as_u64().unwrap_or(0) as usize;
    let skipped: Vec<String> =
        counts["sharedSkipped"].as_array().into_iter().flatten().filter_map(|v| v.as_str().map(String::from)).collect();
    topo::note(n("sharedVertices"), n("sharedFeatures"), &skipped)
}

/// Divides each selected polygon of the active layer into `parts` equal-area parts with
/// parallel lines at the panel angle.
pub fn divide_equal(ctx: &Ctx) -> Res<Value> {
    // The settings round and clamp a script's value; a wrong count is refused instead.
    let parts = match ctx.req.f64("parts") {
        Some(x) => whole_parts("The number of parts", x, MAX_COMMAND_PARTS)?,
        None => ctx.settings.parts as usize,
    };
    let (done, counts) = divide_selected(ctx, Plan::Equal(parts), false)?;
    let msg = match done.as_slice() {
        [(id, ps)] => ctx.tr(
            "Divided feature {id} into {n} parts: {areas} m² (ellipsoidal).",
            &[("id", &id.to_string()), ("n", &ps.len().to_string()), ("areas", &areas_text(ps))],
        ),
        _ => ctx.tr(
            "Divided {count} polygons into {n} equal parts each (ellipsoidal area, largest error {err} m²).",
            &[("count", &done.len().to_string()), ("n", &parts.to_string()), ("err", &format!("{:.4}", counts["maxError"].as_f64().unwrap_or(0.0)))],
        ),
    };
    ctx.finish(&topo::with_note(&msg, &shared_note(&counts)), counts)
}

/// Cuts `percent` of the area off each selected polygon with a line at the panel angle.
pub fn divide_percent(ctx: &Ctx) -> Res<Value> {
    let pct = match ctx.req.f64("percent") {
        Some(x) if !(x > 0.0 && x < 100.0) => {
            return Err(format!("The cut-off share must be a percent greater than 0 and less than 100, not {x}"));
        }
        Some(x) => x,
        None => ctx.settings.percent,
    };
    let (done, counts) = divide_selected(ctx, Plan::Share(pct / 100.0), true)?;
    let msg = match done.as_slice() {
        [(id, ps)] => ctx.tr(
            "Cut {pct} % ({cut} m²) off feature {id}; {rest} m² remain (ellipsoidal).",
            &[
                ("pct", &pct.to_string()),
                ("cut", &format!("{:.2}", ps[0].area)),
                ("id", &id.to_string()),
                ("rest", &format!("{:.2}", ps[1].area)),
            ],
        ),
        _ => ctx.tr(
            "Cut {pct} % off {count} polygons (ellipsoidal area, largest error {err} m²).",
            &[("pct", &pct.to_string()), ("count", &done.len().to_string()), ("err", &format!("{:.4}", counts["maxError"].as_f64().unwrap_or(0.0)))],
        ),
    };
    ctx.finish(&topo::with_note(&msg, &shared_note(&counts)), counts)
}

// ---------- the tool ----------

fn tool_plan(req: &Request) -> Res<Plan> {
    let mode = req.str("mode").unwrap_or("Equal parts").to_ascii_lowercase();
    let num = |k: &str| req.f64(k).ok_or_else(|| format!("'{k}' is required for the division '{mode}'"));
    if mode.starts_with("equal") {
        Ok(Plan::Equal(whole_parts("Count", num("count")?, MAX_PARTS)?))
    } else if mode.contains("area") {
        let a = num("area")?;
        if !(a > 0.0) {
            return Err(format!("Area must be positive, not {a}"));
        }
        Ok(Plan::Area(a))
    } else if mode.contains("width") || mode.contains("strip") {
        let w = num("width")?;
        if !(w > 0.0) {
            return Err(format!("Width must be positive, not {w}"));
        }
        Ok(Plan::Width(w))
    } else {
        Err(format!("unknown division '{mode}'"))
    }
}

/// The `divide-parcel` geoprocessing tool: returns the division proposal as a new layer.
/// Tools make no host requests; the input layer's features are in `req.layers` (only the
/// selected ones when the layer has a selection) and come as lon/lat GeoJSON.
///
/// Every part gets the source's attributes, then `src_id` (the source feature's id, or its
/// index when it has none), `part` (1, 2… along the sweep) and `area_m2` (per the area
/// mode), which override source fields of the same names.
pub fn divide_parcel_tool(req: &Request) -> Res<Value> {
    let input = req.str("input").ok_or("Choose the parcels layer ('input')")?.to_string();
    let feats = req.features("input")?;
    let plan = tool_plan(req)?;
    let angle = req.f64("angle").unwrap_or(0.0).rem_euclid(180.0);
    let mode = match req.str("areaMode").map(str::to_ascii_lowercase) {
        Some(m) if m.starts_with("plan") => AreaMode::Planar,
        Some(m) if !m.starts_with("ellips") => return Err(format!("unknown area mode '{m}'")),
        _ => AreaMode::Ellipsoidal,
    };
    let n = sweep_normal(angle, req.bool("reverse").unwrap_or(false));
    let prepared: Vec<Res<(Parcel, usize)>> = feats
        .iter()
        .map(|f| {
            let g = f.get("geometry").filter(|g| !g.is_null()).ok_or_else(|| "no geometry".to_string())?;
            let g = geojson::from_geojson(g)?;
            Ok((Parcel::new(&g, mode)?, g.num_coords()))
        })
        .collect();
    let parcels: Vec<(usize, usize)> =
        prepared.iter().flatten().map(|(p, v)| (*v, p.parts_estimate(n, plan))).collect();
    check_division(&parcels, budget::TOOL_FUEL, |count, v, parts| {
        format!(
            "Dividing {count} polygon(s) with {} vertices into about {} parts is more than one run of the tool can do. \
             Select fewer polygons or ask for fewer parts.",
            budget::count(v),
            budget::count(parts)
        )
    })?;
    let mut out = Vec::new();
    let mut messages = Vec::new();
    let (mut divided, mut max_err) = (0usize, 0.0f64);
    for (i, (f, prep)) in feats.iter().zip(prepared).enumerate() {
        let src = f.get("id").filter(|v| !v.is_null()).cloned().unwrap_or(json!(i));
        let label = match &src {
            Value::String(s) => s.clone(),
            v => v.to_string(),
        };
        let parts = match prep.and_then(|(p, _)| p.divide(n, plan)) {
            Ok(p) => p,
            Err(e) => {
                messages.push(format!("Parcel {label} skipped: {e}."));
                continue;
            }
        };
        let base = f.get("properties").and_then(Value::as_object).cloned().unwrap_or_default();
        for (k, p) in parts.iter().enumerate() {
            let mut props = base.clone();
            props.insert("src_id".into(), src.clone());
            props.insert("part".into(), json!(k + 1));
            props.insert("area_m2".into(), json!(round4(p.area)));
            out.push(json!({"type": "Feature", "geometry": geojson::to_geojson(&p.geom), "properties": props}));
            max_err = max_err.max(p.error());
        }
        divided += 1;
        if messages.len() < 150 {
            messages.push(format!("Parcel {label}: {} parts, {} m².", parts.len(), areas_text(&parts)));
        }
    }
    if out.is_empty() {
        let why = messages.first().cloned().unwrap_or_else(|| "the layer has no features".into());
        return Err(format!("Nothing to divide in '{input}': {why}"));
    }
    let summary = format!(
        "Divided {divided} of {} parcels into {} parts ({} area, largest error {max_err:.4} m²).",
        feats.len(),
        out.len(),
        mode.name()
    );
    let mut t = ToolOutput::new().layer(&format!("{input} divided"), json!({"type": "FeatureCollection", "features": out}));
    t = t.message(summary);
    for m in messages {
        t = t.message(m);
    }
    Ok(t.into())
}
