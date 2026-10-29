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
//! scale factor squared. For placing the cuts, 1/k² is interpolated over the parcel by a
//! polynomial of degree 4 across and 2 along the meridian (k grows with the squared distance
//! from the central meridian, so a linear model leaves errors of 1e-4 on 200 km); Green's
//! theorem then integrates it exactly with a 4-point Gauss rule per edge. The finished parts
//! are measured independently: on the cylindrical equal-area projection of the ellipsoid
//! (authalic latitude), each edge as the curve a straight frame line makes there. The parts'
//! areas and errors are reported from that measure. Planar mode measures in the frame.

use geo::{Coord, MultiPolygon, Polygon};
use i_overlay::core::fill_rule::FillRule;
use i_overlay::float::slice::FloatSlice;
use rustgis_plugin::{Request, ToolOutput, Value, json};

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
/// an upper bound fitted to `diag-bench` runs on wavy and comb-shaped rings of 20 to 9000
/// vertices cut into 2 to 50 parts. Cuts through a ring that zigzags across them cost the
/// most, hence the large `v * p` term. Measuring the parts on the ellipsoid (an inverse
/// projection per edge) takes about 4500 per vertex of it; `pieces` are the further pieces
/// long edges are measured in ([`Parcel::authalic_pieces`]).
fn divide_fuel(v: usize, p: usize, pieces: f64) -> u64 {
    let (v, p) = (v as u64, p as u64);
    29_000 * v + 4_500 * v * p + 325_000 * p + (AUTHALIC_PIECE_FUEL * pieces).ceil() as u64
}

/// Fuel per piece of an edge measured by [`Authalic`] (two inverse projections and two
/// authalic latitudes): `diag-bench` measures 4400 near the equator, 6700 at 45° and 13 000
/// above 57°, where the projection's cosine needs libm.
const AUTHALIC_PIECE_FUEL: f64 = 14_000.0;

/// Fuel per crossing of a cut with the parcel's edges beyond two per cut: solving for the
/// cuts across it, slicing, placing the vertex it makes on the edge in the parts on both
/// sides and measuring them. `diag-bench` runs on combs of 40 to 2400 vertices cut across
/// their teeth into 2 to 50 parts measure 130 000 to 260 000.
const CROSSING_FUEL: u64 = 350_000;

/// A parcel to check: its vertices, parts, the further pieces its edges are measured in and
/// the further crossings of its edges with the cuts ([`Parcel::extra_crossings`]; 0 before
/// the cuts are placed).
struct Load {
    vertices: usize,
    parts: usize,
    pieces: f64,
    extra: usize,
}

/// Fuel to read and divide `parcels`, without writing the parts.
fn division_fuel(parcels: &[Load]) -> u64 {
    let vertices: usize = parcels.iter().map(|l| l.vertices).sum();
    let compute: u64 =
        parcels.iter().map(|l| divide_fuel(l.vertices, l.parts, l.pieces) + l.extra as u64 * CROSSING_FUEL).sum();
    budget::edit_fuel(parcels.len(), vertices) + compute
}

/// Refuses dividing `parcels` when reading them, dividing them and writing the parts would
/// not fit in `fuel`. Each cut adds about two vertices to each side; every further crossing
/// of a cut with an edge adds a vertex to each side, to be shared with the neighbours too.
fn check_division(parcels: &[Load], fuel: u64, too_much: impl Fn(usize, usize, usize) -> String) -> Res<()> {
    let vertices: usize = parcels.iter().map(|l| l.vertices).sum();
    let parts: usize = parcels.iter().map(|l| l.parts).sum();
    let extra = parcels.iter().fold(0usize, |s, l| s.saturating_add(l.extra)).saturating_mul(2);
    let need = division_fuel(parcels)
        + budget::edit_fuel(parts, (vertices + 4 * parts).saturating_add(extra))
        + topo::share_fuel(extra, extra);
    if need <= fuel { Ok(()) } else { Err(too_much(parcels.len(), vertices, parts)) }
}

/// The positions of a GeoJSON geometry, counted without decoding it.
fn positions(g: &Value) -> usize {
    match g.as_array() {
        Some(a) if a.first().is_some_and(Value::is_number) => 1,
        Some(a) => a.iter().map(positions).sum(),
        None => g.get("coordinates").map_or(0, positions),
    }
}

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

/// The largest parcel (bounding box side in the frame, m) divided by ellipsoidal area. Up to
/// this size the density model below stays within 3e-8 of 1/k² anywhere in a UTM zone or in
/// Poland, and parts of squares this large 2° from the central meridian come out within
/// 1.4e-8 of their share by GeographicLib's geodesic area, far inside the 1e-6 promised.
/// Larger parcels are refused rather than cut beyond the range that was checked.
pub const MAX_ELLIPSOIDAL_EXTENT_M: f64 = 1_000_000.0;

/// Polynomial degrees of the density model across (x) and along (y) the meridian.
const DEG_X: usize = 4;
const DEG_Y: usize = 2;

/// Gauss-Legendre nodes and weights on [0, 1], exact up to degree 7: the antiderivative of
/// the density along an edge has degree DEG_X + 1 + DEG_Y.
const GAUSS4: [(f64, f64); 4] = [
    (0.5 - 0.5 * 0.861_136_311_594_052_6, 0.5 * 0.347_854_845_137_453_86),
    (0.5 - 0.5 * 0.339_981_043_584_856_3, 0.5 * 0.652_145_154_862_546_1),
    (0.5 + 0.5 * 0.339_981_043_584_856_3, 0.5 * 0.652_145_154_862_546_1),
    (0.5 + 0.5 * 0.861_136_311_594_052_6, 0.5 * 0.347_854_845_137_453_86),
];

/// Monomial coefficients of the Chebyshev polynomials T0..T4.
const CHEB: [[f64; 5]; 5] = [
    [1.0, 0.0, 0.0, 0.0, 0.0],
    [0.0, 1.0, 0.0, 0.0, 0.0],
    [-1.0, 0.0, 2.0, 0.0, 0.0],
    [0.0, -3.0, 0.0, 4.0, 0.0],
    [1.0, 0.0, -8.0, 0.0, 8.0],
];

/// Area density g of the frame plane around a parcel, so that ∫ g dA over a region of the
/// frame is its area: a polynomial in u = x / h, v = y / h (local metres).
#[derive(Clone, Copy, Debug)]
struct Density {
    h: f64,
    /// g = Σ g[j][i] uⁱ vʲ
    g: [[f64; DEG_X + 1]; DEG_Y + 1],
    /// ∫₀ᵘ g du = Σ ga[j][i] uⁱ⁺¹ vʲ
    ga: [[f64; DEG_X + 1]; DEG_Y + 1],
}

impl Density {
    const PLANAR: Density = Density {
        h: 1.0,
        g: [[1.0, 0.0, 0.0, 0.0, 0.0], [0.0; 5], [0.0; 5]],
        ga: [[1.0, 0.0, 0.0, 0.0, 0.0], [0.0; 5], [0.0; 5]],
    };

    /// 1/k² interpolated at Chebyshev nodes of the square of half side `h` around `origin`
    /// (frame metres).
    fn ellipsoidal(frame: &Frame, origin: Coord<f64>, h: f64) -> Density {
        const NX: usize = DEG_X + 1;
        const NY: usize = DEG_Y + 1;
        // tx[k][i] = T_k(u_i) = cos(k θ_i) at the nodes u_i = cos θ_i
        let table = |n: usize| {
            let mut t = [[0.0; NX]; NX];
            for (k, row) in t.iter_mut().enumerate().take(n) {
                for (i, x) in row.iter_mut().enumerate().take(n) {
                    *x = (k as f64 * std::f64::consts::PI * (i as f64 + 0.5) / n as f64).cos();
                }
            }
            t
        };
        let (tx, ty) = (table(NX), table(NY));
        let mut f = [[0.0; NX]; NY];
        for (j, row) in f.iter_mut().enumerate() {
            for (i, x) in row.iter_mut().enumerate() {
                *x = scale_factor(frame, origin.x + h * tx[1][i], origin.y + h * ty[1][j]).powi(-2);
            }
        }
        // Chebyshev coefficients by the discrete cosine transform, then monomials
        let mut g = [[0.0; NX]; NY];
        for l in 0..NY {
            for k in 0..NX {
                let mut c = 0.0;
                for (j, row) in f.iter().enumerate() {
                    for (i, x) in row.iter().enumerate() {
                        c += x * tx[k][i] * ty[l][j];
                    }
                }
                c *= (2.0 / NX as f64) * (2.0 / NY as f64);
                if k == 0 {
                    c *= 0.5;
                }
                if l == 0 {
                    c *= 0.5;
                }
                for (b, row) in g.iter_mut().enumerate() {
                    for (a, x) in row.iter_mut().enumerate() {
                        *x += c * CHEB[k][a] * CHEB[l][b];
                    }
                }
            }
        }
        Density::new(h, g)
    }

    fn new(h: f64, g: [[f64; DEG_X + 1]; DEG_Y + 1]) -> Density {
        let mut ga = g;
        for row in &mut ga {
            for (i, c) in row.iter_mut().enumerate() {
                *c /= (i + 1) as f64;
            }
        }
        Density { h, g, ga }
    }

    /// ∫₀ᵘ g du at (u, v).
    fn antiderivative(&self, u: f64, v: f64) -> f64 {
        let mut r = 0.0;
        for row in self.ga.iter().rev() {
            r = r * v + row.iter().rev().fold(0.0, |p, c| p * u + c) * u;
        }
        r
    }

    /// The edge p -> q's term of the area by Green's theorem (∮ G dy, ∂G/∂x = g).
    fn edge(&self, p: Coord<f64>, q: Coord<f64>) -> f64 {
        let dy = q.y - p.y;
        if dy == 0.0 {
            return 0.0;
        }
        let (pu, pv) = (p.x / self.h, p.y / self.h);
        let (du, dv) = ((q.x - p.x) / self.h, dy / self.h);
        let s: f64 = GAUSS4.iter().map(|&(t, w)| w * self.antiderivative(pu + t * du, pv + t * dv)).sum();
        self.h * dy * s
    }

    /// The density at a local point (for converting ground lengths).
    fn at(&self, x: f64, y: f64) -> f64 {
        let (u, v) = (x / self.h, y / self.h);
        let mut r = 0.0;
        for row in self.g.iter().rev() {
            r = r * v + row.iter().rev().fold(0.0, |p, c| p * u + c);
        }
        r
    }
}

/// Ellipsoidal areas measured on the cylindrical equal-area projection (authalic latitude),
/// independently of [`Density`]: it reports what the cuts achieved.
struct Authalic {
    frame: Frame,
    e2: f64,
    e: f64,
}

/// Edges are measured in pieces of at most this length (m) of the frame. A straight frame
/// line is a curve on the equal-area projection; the parabola through each piece's midpoint
/// measures it to 0.002 m² on a 200 km parcel (0.5 m² with 20 km pieces), and to 1e-4 m²
/// on edges up to a few km.
const AUTHALIC_PIECE_M: f64 = 5_000.0;

impl Authalic {
    fn new(frame: Frame) -> Authalic {
        let f = match frame.kind {
            FrameKind::Epsg2180 => GRS80_F,
            FrameKind::Utm { .. } => WGS84_F,
        };
        let e2 = f * (2.0 - f);
        Authalic { frame, e2, e: e2.sqrt() }
    }

    /// Equal-area (x, y) of lon/lat degrees, x from the meridian `lon0`, y from `y0`.
    fn xy(&self, c: Coord<f64>, lon0: f64, y0: f64) -> (f64, f64) {
        let mut dl = c.x - lon0;
        if dl.abs() > 180.0 {
            dl -= 360.0f64.copysign(dl);
        }
        let s = c.y.to_radians().sin();
        let q = (1.0 - self.e2) * (s / (1.0 - self.e2 * s * s) + (self.e * s).atanh() / self.e);
        (GRS80_A * dl.to_radians(), 0.5 * GRS80_A * q - y0)
    }

    /// The signed area of a ring given twice: in frame metres and in lon/lat (the same
    /// vertices, as unprojected). Its edges are straight in the frame.
    fn ring(&self, metric: &[Coord<f64>], lonlat: &[Coord<f64>]) -> f64 {
        let n = metric.len();
        if n < 3 || lonlat.len() != n {
            return 0.0;
        }
        let lon0 = lonlat[0].x;
        let (_, y0) = self.xy(lonlat[0], lon0, 0.0);
        let at = |m: Coord<f64>| {
            let (lon, lat) = self.frame.inv(m.x, m.y);
            self.xy(Coord { x: lon, y: lat }, lon0, y0)
        };
        let mut s = 0.0;
        let mut a = self.xy(lonlat[0], lon0, y0);
        for i in 0..n {
            let j = (i + 1) % n;
            let (p, q) = (metric[i], metric[j]);
            let end = self.xy(lonlat[j], lon0, y0);
            let k = ((q.x - p.x).hypot(q.y - p.y) / AUTHALIC_PIECE_M).ceil().max(1.0);
            let pieces = k as usize;
            for step in 1..=pieces {
                let b = if step == pieces {
                    end
                } else {
                    let t = step as f64 / k;
                    at(Coord { x: p.x + (q.x - p.x) * t, y: p.y + (q.y - p.y) * t })
                };
                let t = (step as f64 - 0.5) / k;
                let m = at(Coord { x: p.x + (q.x - p.x) * t, y: p.y + (q.y - p.y) * t });
                // the chord, then the parabolic segment between chord and curve (4/3 of the
                // triangle through the midpoint)
                s += 0.5 * (a.0 * b.1 - b.0 * a.1);
                s += (2.0 / 3.0) * ((m.0 - a.0) * (b.1 - a.1) - (b.0 - a.0) * (m.1 - a.1));
                a = b;
            }
        }
        s
    }

    /// The area of polygons given in frame metres and in lon/lat (as unprojected), m².
    fn area(&self, metric: &MultiPolygon<f64>, lonlat: &MultiPolygon<f64>) -> f64 {
        let mut total = 0.0;
        for (pm, pl) in metric.0.iter().zip(&lonlat.0) {
            let rings_m = std::iter::once(pm.exterior()).chain(pm.interiors());
            let rings_l = std::iter::once(pl.exterior()).chain(pl.interiors());
            for (k, (rm, rl)) in rings_m.zip(rings_l).enumerate() {
                let n = rm.0.len() - usize::from(rm.0.len() > 1 && rm.0.first() == rm.0.last());
                let a = self.ring(&rm.0[..n], &rl.0[..n.min(rl.0.len())]).abs();
                total += if k == 0 { a } else { -a };
            }
        }
        total
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
    /// Each ring's edges' terms of the area (edge i runs from vertex i to the next).
    edge_terms: Vec<Vec<f64>>,
    dens: Density,
    mode: AreaMode,
    /// The parcel's area per the area mode, from the density model (within 3e-8 of the
    /// ellipsoidal area up to [`MAX_ELLIPSOIDAL_EXTENT_M`]).
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
    /// Its area per the area mode, measured on the cut geometry (ellipsoidal: by
    /// [`Authalic`], not by the model that placed the cuts).
    pub area: f64,
    /// The area it was cut for: its share of the parts' total, or the requested area (`None`
    /// for a remainder or a strip).
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
        let side = (hi[0] - lo[0]).max(hi[1] - lo[1]);
        let dens = match mode {
            AreaMode::Planar => Density::PLANAR,
            AreaMode::Ellipsoidal if side > MAX_ELLIPSOIDAL_EXTENT_M => {
                return Err(format!(
                    "the parcel is {:.0} km across in {}; ellipsoidal areas are guaranteed to 1 ppm on parcels up to \
                     {:.0} km. Split it first, or divide it with the Divide Parcel tool and areaMode \"Planar\" \
                     (areas in the frame's plane)",
                    side / 1000.0,
                    frame.name(),
                    MAX_ELLIPSOIDAL_EXTENT_M / 1000.0
                ));
            }
            AreaMode::Ellipsoidal => Density::ellipsoidal(&frame, origin, (0.5 * side).max(1.0)),
        };
        let rings = weighted_rings(&local);
        let edge_terms: Vec<Vec<f64>> = rings
            .iter()
            .map(|(r, _)| (0..r.len()).map(|i| dens.edge(r[i], r[(i + 1) % r.len()])).collect())
            .collect();
        let area: f64 = rings.iter().zip(&edge_terms).map(|((_, w), e)| w * e.iter().sum::<f64>()).sum();
        if !(area > 0.0) {
            return Err("the polygon has no area".into());
        }
        Ok(Parcel { placer, metric: metric_geom, origin, local, rings, edge_terms, dens, mode, area })
    }

    /// The area of the parcel on the start side of the line {n·p = t}: each ring clipped to
    /// that half-plane (Sutherland-Hodgman) and integrated with Green's theorem. The clipped
    /// ring may run along the cut line back and forth for concave rings, which the integral
    /// handles. Whole edges add their stored terms; only the pieces the line makes are
    /// integrated here.
    fn area_before(&self, n: P2, t: f64) -> f64 {
        let f = |c: Coord<f64>| n[0] * c.x + n[1] * c.y - t;
        let mut total = 0.0;
        for ((pts, w), terms) in self.rings.iter().zip(&self.edge_terms) {
            let len = pts.len();
            // clipped ring points, with Some(i) for vertex i of the ring
            let link = |a: (Coord<f64>, Option<usize>), b: (Coord<f64>, Option<usize>)| match (a.1, b.1) {
                (Some(i), Some(j)) if j == (i + 1) % len => terms[i],
                _ => self.dens.edge(a.0, b.0),
            };
            let (mut first, mut prev) = (None, None);
            let mut s = 0.0;
            let mut emit = |p: (Coord<f64>, Option<usize>)| {
                match prev {
                    Some(q) => s += link(q, p),
                    None => first = Some(p),
                }
                prev = Some(p);
            };
            let mut fa = f(pts[0]);
            for i in 0..len {
                let (a, b) = (pts[i], pts[(i + 1) % len]);
                let fb = f(b);
                if fa <= 0.0 {
                    emit((a, Some(i)));
                }
                if (fa < 0.0 && fb > 0.0) || (fa > 0.0 && fb < 0.0) {
                    let r = fa / (fa - fb);
                    emit((Coord { x: a.x + (b.x - a.x) * r, y: a.y + (b.y - a.y) * r }, None));
                }
                fa = fb;
            }
            if let (Some(p), Some(q)) = (prev, first) {
                s += link(p, q);
            }
            total += w * s;
        }
        total
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
    fn solve(&self, n: P2, target: f64, (mut lo, alo): (f64, f64), (mut hi, ahi): (f64, f64)) -> f64 {
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
            let f = self.area_before(n, t) - target;
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
        let total = self.area;
        // A table of the area along the sweep brackets every cut closely, so each root takes
        // a few evaluations however many parts there are.
        let cumulative = |targets: &[f64]| {
            let k = (2 * targets.len()).clamp(8, 64);
            let ts: Vec<f64> = (0..=k).map(|i| tmin + (tmax - tmin) * i as f64 / k as f64).collect();
            let fs: Vec<f64> = ts
                .iter()
                .enumerate()
                .map(|(i, &t)| match i {
                    0 => 0.0,
                    i if i == k => total,
                    _ => self.area_before(n, t),
                })
                .collect();
            let mut cuts: Vec<f64> = Vec::with_capacity(targets.len());
            for &c in targets {
                let i = fs.partition_point(|&f| f < c).clamp(1, k);
                let mut lo = (ts[i - 1], fs[i - 1]);
                if let Some(&prev) = cuts.last()
                    && prev > lo.0
                {
                    lo = (prev, self.area_before(n, prev));
                }
                cuts.push(self.solve(n, c, lo, (ts[i], fs[i])));
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
                (cumulative(&targets), vec![Some(each); parts])
            }
            Plan::Share(s) => {
                if !(s > 0.0 && s < 1.0) {
                    return Err("the share must be between 0 and 100 %".into());
                }
                let a = total * s;
                (cumulative(&[a]), vec![Some(a), Some(total - a)])
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
                (cumulative(&targets), aims)
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

    /// How many pieces beyond one per edge [`Authalic`] measures `parts` parts cut at `n` in
    /// (an upper bound): the parcel's edges fall into pieces of [`AUTHALIC_PIECE_M`], and each
    /// cut is an edge of the parts on both sides, inside the parcel for at most the parcel's
    /// extent along the cut. Planar parts are not measured so.
    fn authalic_pieces(&self, n: P2, parts: usize) -> f64 {
        if self.mode == AreaMode::Planar {
            return 0.0;
        }
        let mut perimeter = 0.0;
        for (r, _) in &self.rings {
            for (i, a) in r.iter().enumerate() {
                let b = r[(i + 1) % r.len()];
                perimeter += ((b.x - a.x).powi(2) + (b.y - a.y).powi(2)).sqrt();
            }
        }
        let (lo, hi) = self.extent([-n[1], n[0]]);
        (perimeter + 2.0 * parts.saturating_sub(1) as f64 * (hi - lo)) / AUTHALIC_PIECE_M
    }

    /// How many more times the cut lines {n·p = t} for `cuts` cross the parcel's edges than
    /// twice each, as across a convex parcel: a comb or a serpentine lying across the cuts is
    /// crossed by each of them many times.
    fn extra_crossings(&self, n: P2, cuts: &[f64]) -> usize {
        let mut sorted = cuts.to_vec();
        sorted.sort_by(f64::total_cmp);
        let mut crossings = 0usize;
        for (r, _) in &self.rings {
            for (i, a) in r.iter().enumerate() {
                let b = r[(i + 1) % r.len()];
                let (ta, tb) = (n[0] * a.x + n[1] * a.y, n[0] * b.x + n[1] * b.y);
                let (lo, hi) = (ta.min(tb), ta.max(tb));
                crossings += sorted.partition_point(|&c| c < hi) - sorted.partition_point(|&c| c <= lo);
            }
        }
        crossings.saturating_sub(2 * cuts.len())
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
        self.divide_at(n, plan, cuts, aims)
    }

    /// [`Parcel::divide`] at the cut positions and with the targets [`Parcel::cuts`] found.
    fn divide_at(&self, n: P2, plan: Plan, cuts: Vec<f64>, aims: Vec<Option<f64>>) -> Res<Vec<Part>> {
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
        let authalic = Authalic::new(self.placer.frame());
        let mut parts = Vec::with_capacity(aims.len());
        for (k, (polys, aim)) in strips.into_iter().zip(aims).enumerate() {
            let local = MultiPolygon(polys);
            let metric_polys: Vec<Polygon<f64>> =
                local.0.iter().map(|p| geo::MapCoords::map_coords(p, |c| c + self.origin)).collect();
            let metric = geojson::from_polygons(metric_polys.clone());
            let mut new_vertices = Vec::new();
            let lonlat = self.placer.unproject_cut(&ix, &metric, &mut new_vertices);
            // measured on what is written, independently of the model that placed the cuts
            let area = match (self.mode, lonlat.to_multi_polygon()) {
                (AreaMode::Ellipsoidal, Some(ll)) => authalic.area(&MultiPolygon(metric_polys), &ll),
                _ => {
                    let mut m = Moments::default();
                    for (r, w) in weighted_rings(&local) {
                        m.add(Moments::of(&r).scaled(w));
                    }
                    m.a
                }
            };
            let geom = match geojson::prepare_output(lonlat) {
                Ok(g) => g,
                Err(_) if aim.is_none() => continue,
                Err(e) => return Err(format!("part {}: {e}", k + 1)),
            };
            parts.push(Part { geom, area, target: aim, new_vertices });
        }
        // Shares of the area are shares of what the parts measure; a part of a given area
        // keeps its target.
        if let Plan::Equal(_) | Plan::Share(_) = plan {
            let scale = parts.iter().map(|p| p.area).sum::<f64>() / self.area;
            for p in &mut parts {
                p.target = p.target.map(|t| t * scale);
            }
        }
        Ok(parts)
    }
}

// ---------- commands ----------

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
    let mut ed = active_features(&spec)?;
    let parts = match plan {
        Plan::Equal(k) => k,
        _ => 2,
    };
    let too_much = |n: usize, v: usize, _: usize| {
        format!(
            "Dividing {n} polygon(s) with {} vertices into {parts} parts is more than one command can do. \
             Select fewer polygons, or use the Divide Parcel tool (it handles whole layers).",
            budget::count(v)
        )
    };
    let vertices = |f: &host_io::Feature| f.geometry.as_ref().map_or(0, Geom::num_coords);
    // by the vertices first, so that preparing the parcels fits; then with their edges' lengths
    let loads: Vec<Load> =
        ed.features.iter().map(|f| Load { vertices: vertices(f), parts, pieces: 0.0, extra: 0 }).collect();
    check_division(&loads, budget::COMMAND_FUEL, too_much)?;
    let n = sweep_normal(ctx.settings.angle_deg, ctx.settings.reverse);
    let mut parcels = Vec::with_capacity(ed.features.len());
    for f in &ed.features {
        let fail = |e: String| format!("{USAGE}. (feature {}: {e})", f.id);
        parcels.push(Parcel::new(f.geom()?, AreaMode::Ellipsoidal).map_err(fail)?);
    }
    // where the cuts go: how often they cross the edges is known before cutting
    let mut planned = Vec::with_capacity(parcels.len());
    for (f, parcel) in ed.features.iter().zip(&parcels) {
        let fail = |e: String| format!("{USAGE}. (feature {}: {e})", f.id);
        planned.push(parcel.cuts(n, plan).map_err(fail)?);
    }
    let loads: Vec<Load> = ed
        .features
        .iter()
        .zip(&parcels)
        .zip(&planned)
        .map(|((f, p), (cuts, _))| Load {
            vertices: vertices(f),
            parts,
            pieces: p.authalic_pieces(n, parts),
            extra: p.extra_crossings(n, cuts),
        })
        .collect();
    check_division(&loads, budget::COMMAND_FUEL, too_much)?;
    let mut done = Vec::with_capacity(ed.features.len());
    for ((f, parcel), (cuts, aims)) in ed.features.iter().zip(&parcels).zip(planned) {
        let fail = |e: String| format!("{USAGE}. (feature {}: {e})", f.id);
        let parts = parcel.divide_at(n, plan, cuts, aims).map_err(fail)?;
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
    // The parts as cut: the vertices they have beyond the parcels' are written, and the new
    // ones shared between parcels, within what reading and cutting left of the budget. The
    // parts of one parcel come from one slice and have each other's vertices already.
    let read = budget::vertices(&ed.features);
    let count: usize = done.iter().map(|(_, ps)| ps.len()).sum();
    let coords: usize = done.iter().flat_map(|(_, ps)| ps.iter().map(|p| p.geom.num_coords())).sum();
    let mut spent = division_fuel(&loads) + budget::write_fuel(count, coords.saturating_sub(read));
    let left = budget::COMMAND_FUEL.saturating_sub(spent);
    let share = done.len() > 1;
    if left == 0 || (share && topo::share_fuel(new_vertices.len(), coords) > left) {
        return Err(too_much(ed.features.len(), read, parts));
    }
    if share {
        let results = done.iter_mut().enumerate().flat_map(|(i, (_, ps))| ps.iter_mut().map(move |p| (i, &mut p.geom)));
        let mut fuel = budget::Meter::new(left, too_much(ed.features.len(), read, parts));
        topo::share_within(ed.frame()?, results, &new_vertices, &mut fuel)?;
        spent += fuel.spent;
    }
    let scope = topo::Scope {
        layers: &ed.layers,
        edited: vec![layer],
        exclude: done.iter().map(|(f, _)| (layer, f.id)).collect(),
        // the parcels were read and divided, and the parts' writes and sharing charged: the
        // neighbours get what is left of the command, with the margin Cut keeps
        spent,
        limit: crate::polygons::CUT_WORK_FUEL,
        read_whole: None,
    };
    let shared = topo::plan(ctx.settings.topological, &scope, &new_vertices, &mut ed.read)?;
    let mut rows = Vec::new();
    let mut created = 0usize;
    for (f, parts) in &done {
        let keep = if keep_last { parts.len() - 1 } else { 0 };
        let attrs = host_io::inherited_attributes(&f.properties);
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

/// Most lines about single parcels in the tool's answer: the parcels divided beyond them are
/// in the summary, the skipped ones counted in one more line.
const MAX_PARCEL_MESSAGES: usize = 150;

/// The largest layer the tool returns, by its estimated GeoJSON text: the parts are built as
/// JSON values (somewhat larger than their text) and then serialized, with the input and the
/// prepared parcels still held, all within the module's 512 MB.
pub const MAX_TOOL_OUTPUT_BYTES: usize = 64_000_000;

/// GeoJSON text of a part besides its coordinates and its parcel's attributes: the feature's
/// frame, `src_id`, `part` and `area_m2`.
const PART_FRAME_BYTES: usize = 150;

/// Estimated GeoJSON text of a parcel of `v` vertices, whose attributes take `props` bytes,
/// cut into `p` parts: every part has the attributes and about four vertices of the cuts
/// besides its share of the parcel's.
fn output_bytes(v: usize, p: usize, props: usize) -> usize {
    let per_vertex = budget::BYTES_PER_VERTEX as usize;
    p.saturating_mul(props + PART_FRAME_BYTES + 4 * per_vertex).saturating_add(v.saturating_mul(per_vertex))
}

/// The length of `v` as JSON text, counted without writing it.
fn json_len(v: &Value) -> usize {
    struct Count(usize);
    impl std::io::Write for Count {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0 += b.len();
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut c = Count(0);
    let _ = rustgis_plugin::serde_json::to_writer(&mut c, v);
    c.0
}

/// Parcels the tool has started preparing since the counter was reset (diagnostic build):
/// shows which checks come before any preparation.
#[cfg(feature = "diagnostics")]
pub static PREPARED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

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
    let too_much = |count: usize, v: usize, parts: usize| {
        format!(
            "Dividing {count} polygon(s) with {} vertices into about {} parts is more than one run of the tool can do. \
             Select fewer polygons or ask for fewer parts.",
            budget::count(v),
            budget::count(parts)
        )
    };
    // Every part carries its parcel's attributes: the layer is checked by its size too.
    let too_big = |count: usize, parts: usize, bytes: usize| -> Res<()> {
        if bytes <= MAX_TOOL_OUTPUT_BYTES {
            return Ok(());
        }
        Err(format!(
            "Dividing {count} polygon(s) into about {} parts would make a layer of about {} MB, every part with \
             its parcel's attributes; one run of the tool makes at most {} MB. Select fewer polygons, ask for \
             fewer parts, or leave large attributes out of the input.",
            budget::count(parts),
            budget::count(bytes.div_ceil(1_000_000)),
            MAX_TOOL_OUTPUT_BYTES / 1_000_000
        ))
    };
    // Before decoding anything: every feature's vertices, cut into the fewest parts the plan
    // may give.
    let fewest = match plan {
        Plan::Equal(k) => k,
        _ => 2,
    };
    let sizes: Vec<usize> = feats.iter().map(|f| f.get("geometry").map_or(0, positions)).collect();
    let props: Vec<usize> = feats.iter().map(|f| f.get("properties").map_or(0, json_len)).collect();
    let loads: Vec<Load> = sizes.iter().map(|&v| Load { vertices: v, parts: fewest, pieces: 0.0, extra: 0 }).collect();
    check_division(&loads, budget::TOOL_FUEL, too_much)?;
    let bytes = sizes.iter().zip(&props).map(|(&v, &b)| output_bytes(v, fewest, b)).fold(0, usize::saturating_add);
    too_big(feats.len(), feats.len().saturating_mul(fewest), bytes)?;
    // A feature that cannot be divided still took reading and preparing: the part of
    // `divide_fuel` that grows with its vertices alone.
    let mut failed = 0u64;
    let prepared: Vec<Res<(Parcel, usize)>> = feats
        .iter()
        .zip(&sizes)
        .map(|(f, &v)| {
            #[cfg(feature = "diagnostics")]
            PREPARED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let prep = || {
                let g = f.get("geometry").filter(|g| !g.is_null()).ok_or_else(|| "no geometry".to_string())?;
                let g = geojson::from_geojson(g)?;
                Ok((Parcel::new(&g, mode)?, g.num_coords()))
            };
            let r = prep();
            if r.is_err() {
                failed += budget::edit_fuel(1, v) + divide_fuel(v, 0, 0.0);
            }
            r
        })
        .collect();
    let mut bytes = 0usize;
    let mut parcels: Vec<Load> = prepared
        .iter()
        .zip(&props)
        .filter_map(|(r, &b)| r.as_ref().ok().map(|(p, v)| (p, *v, b)))
        .map(|(p, v, b)| {
            let parts = p.parts_estimate(n, plan);
            bytes = bytes.saturating_add(output_bytes(v, parts, b));
            Load { vertices: v, parts, pieces: p.authalic_pieces(n, parts), extra: 0 }
        })
        .collect();
    check_division(&parcels, budget::TOOL_FUEL.saturating_sub(failed), too_much)?;
    too_big(parcels.len(), parcels.iter().map(|l| l.parts).fold(0, usize::saturating_add), bytes)?;
    // where the cuts go: how often they cross the edges is known before cutting
    let mut loads = parcels.iter_mut();
    let planned: Vec<Res<(Parcel, Vec<f64>, Vec<Option<f64>>)>> = prepared
        .into_iter()
        .map(|prep| {
            let (p, _) = prep?;
            let load = loads.next().expect("a load per parcel");
            let (cuts, aims) = p.cuts(n, plan)?;
            load.extra = p.extra_crossings(n, &cuts);
            Ok((p, cuts, aims))
        })
        .collect();
    check_division(&parcels, budget::TOOL_FUEL.saturating_sub(failed), too_much)?;
    let mut out = Vec::new();
    let mut messages = Vec::new();
    let mut unlisted = 0usize;
    let (mut divided, mut max_err) = (0usize, 0.0f64);
    for (i, (f, prep)) in feats.iter().zip(planned).enumerate() {
        let src = f.get("id").filter(|v| !v.is_null()).cloned().unwrap_or(json!(i));
        let label = match &src {
            Value::String(s) => s.clone(),
            v => v.to_string(),
        };
        let parts = match prep.and_then(|(p, cuts, aims)| p.divide_at(n, plan, cuts, aims)) {
            Ok(p) => p,
            Err(e) if messages.len() < MAX_PARCEL_MESSAGES => {
                messages.push(format!("Parcel {label} skipped: {e}."));
                continue;
            }
            Err(_) => {
                unlisted += 1;
                continue;
            }
        };
        let base = f.get("properties").and_then(Value::as_object).map(host_io::inherited_attributes).unwrap_or_default();
        for (k, p) in parts.iter().enumerate() {
            let mut props = base.clone();
            props.insert("src_id".into(), src.clone());
            props.insert("part".into(), json!(k + 1));
            props.insert("area_m2".into(), json!(round4(p.area)));
            out.push(json!({"type": "Feature", "geometry": geojson::to_geojson(&p.geom), "properties": props}));
            max_err = max_err.max(p.error());
        }
        divided += 1;
        if messages.len() < MAX_PARCEL_MESSAGES {
            messages.push(format!("Parcel {label}: {} parts, {} m².", parts.len(), areas_text(&parts)));
        }
    }
    if unlisted > 0 {
        messages.push(format!("… and {unlisted} more parcels skipped."));
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
