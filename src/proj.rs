//! Metric frames for geometry work.
//!
//! The host hands features over in lon/lat and does not tell the plugin the layer's CRS, so
//! every metric operation projects to a local transverse Mercator frame, works in metres and
//! projects back: EPSG:2180 (PUWG 1992) when all input lies in Poland, otherwise the WGS84 UTM
//! zone of the input's centre.
//!
//! The projection is the Krüger series to n^6 (Karney 2011), the method of PROJ's default
//! `tmerc`; it agrees with PROJ to well below 1 mm within ~4000 km of the central meridian.
//! Like PROJ, it goes between geodetic and conformal latitude by series too, and sums every
//! series with Clenshaw's method: projecting is most of a command's geometry work, and this
//! needs a third of the trigonometric calls of evaluating each term.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

use geo::{Coord, MapCoords};

use crate::geojson::Geom;

pub const GRS80_A: f64 = 6_378_137.0;
pub const GRS80_F: f64 = 1.0 / 298.257_222_101;
pub const WGS84_F: f64 = 1.0 / 298.257_223_563;

/// Poland's extent (lon_min, lat_min, lon_max, lat_max), where EPSG:2180 is used.
pub const POLAND: [f64; 4] = [14.0, 49.0, 24.2, 54.9];

/// Inputs spanning more longitude than this are refused: TM distortion grows quickly away
/// from the central meridian and such edits are not meaningful in metres anyway.
pub const MAX_LON_SPAN_DEG: f64 = 20.0;

/// UTM is defined up to 84°N / 80°S; beyond that the frame would be badly distorted.
pub const MAX_ABS_LAT_DEG: f64 = 84.0;

/// Transverse Mercator on an ellipsoid, lat_0 = 0.
#[derive(Clone, Copy, Debug)]
pub struct Tm {
    lon0: f64,
    k0: f64,
    fe: f64,
    fn_: f64,
    a_rect: f64,
    /// Geodetic -> conformal latitude and back: χ = φ + Σ cbg_j sin 2jφ, φ = χ + Σ cgb_j sin 2jχ.
    cbg: [f64; 6],
    cgb: [f64; 6],
    alp: [f64; 6],
    bet: [f64; 6],
}

impl Tm {
    pub fn new(a: f64, f: f64, lon0_deg: f64, k0: f64, fe: f64, fn_: f64) -> Tm {
        let n = f / (2.0 - f);
        let (n2, n3) = (n * n, n * n * n);
        let (n4, n5, n6) = (n2 * n2, n2 * n3, n3 * n3);
        let a_rect = a / (1.0 + n) * (1.0 + n2 / 4.0 + n4 / 64.0 + n6 / 256.0);
        let alp = [
            n / 2.0 - 2.0 * n2 / 3.0 + 5.0 * n3 / 16.0 + 41.0 * n4 / 180.0 - 127.0 * n5 / 288.0
                + 7891.0 * n6 / 37800.0,
            13.0 * n2 / 48.0 - 3.0 * n3 / 5.0 + 557.0 * n4 / 1440.0 + 281.0 * n5 / 630.0
                - 1983433.0 * n6 / 1935360.0,
            61.0 * n3 / 240.0 - 103.0 * n4 / 140.0 + 15061.0 * n5 / 26880.0 + 167603.0 * n6 / 181440.0,
            49561.0 * n4 / 161280.0 - 179.0 * n5 / 168.0 + 6601661.0 * n6 / 7257600.0,
            34729.0 * n5 / 80640.0 - 3418889.0 * n6 / 1995840.0,
            212378941.0 * n6 / 319334400.0,
        ];
        let bet = [
            n / 2.0 - 2.0 * n2 / 3.0 + 37.0 * n3 / 96.0 - n4 / 360.0 - 81.0 * n5 / 512.0
                + 96199.0 * n6 / 604800.0,
            n2 / 48.0 + n3 / 15.0 - 437.0 * n4 / 1440.0 + 46.0 * n5 / 105.0 - 1118711.0 * n6 / 3870720.0,
            17.0 * n3 / 480.0 - 37.0 * n4 / 840.0 - 209.0 * n5 / 4480.0 + 5569.0 * n6 / 90720.0,
            4397.0 * n4 / 161280.0 - 11.0 * n5 / 504.0 - 830251.0 * n6 / 7257600.0,
            4583.0 * n5 / 161280.0 - 108847.0 * n6 / 3991680.0,
            20648693.0 * n6 / 638668800.0,
        ];
        // PROJ's tmerc coefficients (Engsager & Poder), checked against the exact formula in
        // the tests
        let cbg = [
            n * (-2.0 + n * (2.0 / 3.0 + n * (4.0 / 3.0 + n * (-82.0 / 45.0 + n * (32.0 / 45.0 + n * 4642.0 / 4725.0))))),
            n2 * (5.0 / 3.0 + n * (-16.0 / 15.0 + n * (-13.0 / 9.0 + n * (904.0 / 315.0 + n * -1522.0 / 945.0)))),
            n3 * (-26.0 / 15.0 + n * (34.0 / 21.0 + n * (8.0 / 5.0 + n * -12686.0 / 2835.0))),
            n4 * (1237.0 / 630.0 + n * (-12.0 / 5.0 + n * -24832.0 / 14175.0)),
            n5 * (-734.0 / 315.0 + n * 109598.0 / 31185.0),
            n6 * 444337.0 / 155925.0,
        ];
        let cgb = [
            n * (2.0 + n * (-2.0 / 3.0 + n * (-2.0 + n * (116.0 / 45.0 + n * (26.0 / 45.0 + n * -2854.0 / 675.0))))),
            n2 * (7.0 / 3.0 + n * (-8.0 / 5.0 + n * (-227.0 / 45.0 + n * (2704.0 / 315.0 + n * 2323.0 / 945.0)))),
            n3 * (56.0 / 15.0 + n * (-136.0 / 35.0 + n * (-1262.0 / 105.0 + n * 73814.0 / 2835.0))),
            n4 * (4279.0 / 630.0 + n * (-332.0 / 35.0 + n * -399572.0 / 14175.0)),
            n5 * (4174.0 / 315.0 + n * -144838.0 / 6237.0),
            n6 * 601676.0 / 22275.0,
        ];
        Tm { lon0: lon0_deg.to_radians(), k0, fe, fn_, a_rect, cbg, cgb, alp, bet }
    }

    /// EPSG:2180 (ETRS89 / Poland CS92).
    pub fn epsg2180() -> Tm {
        Tm::new(GRS80_A, GRS80_F, 19.0, 0.9993, 500_000.0, -5_300_000.0)
    }

    /// WGS84 / UTM `zone` (1..=60); the southern hemisphere adds the 10 000 km false northing.
    pub fn utm(zone: u32, south: bool) -> Tm {
        let fn_ = if south { 10_000_000.0 } else { 0.0 };
        Tm::new(GRS80_A, WGS84_F, zone as f64 * 6.0 - 183.0, 0.9996, 500_000.0, fn_)
    }

    /// (lon, lat) in degrees -> (easting, northing) in metres. A longitude on the other side
    /// of the antimeridian from the central meridian counts from it the short way round.
    pub fn forward(&self, lon_deg: f64, lat_deg: f64) -> (f64, f64) {
        let mut lam = lon_deg.to_radians() - self.lon0;
        // ±360 is exact for longitudes near ±180°
        if lam > std::f64::consts::PI {
            lam = (lon_deg - 360.0).to_radians() - self.lon0;
        } else if lam < -std::f64::consts::PI {
            lam = (lon_deg + 360.0).to_radians() - self.lon0;
        }
        let phi = lat_deg.to_radians();
        let (sphi, cphi) = sin_cos(phi);
        let (slam, clam) = sin_cos(lam);
        // tan of the conformal latitude φ + δ, by the addition theorem
        let delta = clenshaw_real(&self.cbg, 2.0 * sphi * cphi, cphi * cphi - sphi * sphi);
        let t = tan_small(delta);
        let taup = (sphi + cphi * t) / (cphi - sphi * t);
        let r = (taup * taup + clam * clam).sqrt();
        let xip = taup.atan2(clam);
        let q = slam / r;
        let etap = q.asinh();
        // sin/cos of 2ξ' and sinh/cosh of 2η' from the half angles, without more libm calls
        let (s1, c1, ch1) = (taup / r, clam / r, (1.0 + q * q).sqrt());
        let (s2, c2, sh2, ch2) = (2.0 * s1 * c1, c1 * c1 - s1 * s1, 2.0 * q * ch1, 1.0 + 2.0 * q * q);
        let (dxi, deta) = clenshaw(&self.alp, s2, c2, sh2, ch2);
        let ka = self.k0 * self.a_rect;
        (self.fe + ka * (etap + deta), self.fn_ + ka * (xip + dxi))
    }

    /// (easting, northing) in metres -> (lon, lat) in degrees.
    pub fn inverse(&self, x: f64, y: f64) -> (f64, f64) {
        let ka = self.k0 * self.a_rect;
        let (xi, eta) = ((y - self.fn_) / ka, (x - self.fe) / ka);
        let (s1, c1) = sin_cos(xi);
        let (sh1, ch1) = sinh_cosh(eta);
        let (s2, c2, sh2, ch2) = (2.0 * s1 * c1, c1 * c1 - s1 * s1, 2.0 * sh1 * ch1, ch1 * ch1 + sh1 * sh1);
        let (dxi, deta) = clenshaw(&self.bet, s2, c2, sh2, ch2);
        let (xip, etap) = (xi - dxi, eta - deta);
        let (sxip, cxip) = sin_cos(xip);
        let (shetap, _) = sinh_cosh(etap);
        let taup = sxip / (shetap * shetap + cxip * cxip).sqrt();
        let lam = shetap.atan2(cxip);
        // conformal -> geodetic latitude, sin/cos 2χ from tan χ
        let h2 = 1.0 + taup * taup;
        let delta = clenshaw_real(&self.cgb, 2.0 * taup / h2, (1.0 - taup * taup) / h2);
        let lon = (lam + self.lon0).to_degrees();
        let lon = if lon > 180.0 {
            lon - 360.0
        } else if lon < -180.0 {
            lon + 360.0
        } else {
            lon
        };
        (lon, (taup.atan() + delta).to_degrees())
    }
}

/// sin and cos with one libm call up to 1 rad (57°, beyond Poland): 1 - s² is formed as
/// (1 - s)(1 + s), accurate to an ulp. Closer to the pole cos gets small and its relative
/// error would show in the result, so libm computes it.
fn sin_cos(x: f64) -> (f64, f64) {
    let s = x.sin();
    if x.abs() < 1.0 { (s, ((1.0 - s) * (1.0 + s)).sqrt()) } else { (s, x.cos()) }
}

/// tan δ for |δ| < 0.01 (the conformal latitude shift): the Taylor series to δ^7 is exact to
/// f64 precision there.
fn tan_small(d: f64) -> f64 {
    let d2 = d * d;
    d * (1.0 + d2 * (1.0 / 3.0 + d2 * (2.0 / 15.0 + d2 * (17.0 / 315.0))))
}

/// Σ c_j sin(2jθ) by Clenshaw summation from sin 2θ and cos 2θ.
fn clenshaw_real(c: &[f64; 6], s2: f64, c2: f64) -> f64 {
    let (mut y0, mut y1) = (0.0, 0.0);
    for &cj in c.iter().rev() {
        (y1, y0) = (y0, 2.0 * c2 * y0 - y1 + cj);
    }
    s2 * y0
}

/// sinh and cosh with one `exp_m1`, accurate near zero.
fn sinh_cosh(x: f64) -> (f64, f64) {
    let u = x.exp_m1();
    let ex = u + 1.0;
    (0.5 * (u + u / ex), 0.5 * (ex + 1.0 / ex))
}

/// Σ c_j sin(2jζ) for ζ = ξ + iη (as (real, imaginary)), by Clenshaw summation from
/// sin/cos(2ξ) and sinh/cosh(2η): one set of trigonometric values instead of one per term.
fn clenshaw(c: &[f64; 6], s2: f64, c2: f64, sh2: f64, ch2: f64) -> (f64, f64) {
    // a = 2 cos(2ζ)
    let (ar, ai) = (2.0 * c2 * ch2, -2.0 * s2 * sh2);
    let (mut y0r, mut y0i, mut y1r, mut y1i) = (0.0, 0.0, 0.0, 0.0);
    for &cj in c.iter().rev() {
        let (tr, ti) = (ar * y0r - ai * y0i - y1r + cj, ar * y0i + ai * y0r - y1i);
        (y1r, y1i, y0r, y0i) = (y0r, y0i, tr, ti);
    }
    // sin(2ζ) * y
    let (sr, si) = (s2 * ch2, c2 * sh2);
    (sr * y0r - si * y0i, sr * y0i + si * y0r)
}

/// Which projection a [`Frame`] uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameKind {
    Epsg2180,
    Utm { zone: u32, south: bool },
}

/// A metric working frame: projects lon/lat geometries to metres and back.
#[derive(Clone, Copy, Debug)]
pub struct Frame {
    pub kind: FrameKind,
    tm: Tm,
}

pub fn in_poland(lon: f64, lat: f64) -> bool {
    (POLAND[0]..=POLAND[2]).contains(&lon) && (POLAND[1]..=POLAND[3]).contains(&lat)
}

impl Frame {
    pub fn epsg2180() -> Frame {
        Frame { kind: FrameKind::Epsg2180, tm: Tm::epsg2180() }
    }

    /// The UTM zone containing (lon, lat). Norway/Svalbard zone exceptions are ignored: any
    /// nearby zone is an equally good working frame.
    pub fn utm_at(lon: f64, lat: f64) -> Frame {
        let zone = (((lon + 180.0) / 6.0).floor() as i64).rem_euclid(60) as u32 + 1;
        let south = lat < 0.0;
        Frame { kind: FrameKind::Utm { zone, south }, tm: Tm::utm(zone, south) }
    }

    /// The frame for a set of lon/lat positions: EPSG:2180 when every position lies in
    /// Poland, else UTM at the centre of their bounding box. Fails on no positions, non-finite
    /// values, polar latitudes or an extent too wide for a single TM frame.
    ///
    /// Longitudes are taken on the shorter way round the globe: positions on both sides of
    /// the antimeridian (179.9° and -179.9°) span 0.2°, centred on 180°.
    pub fn for_positions(points: impl IntoIterator<Item = [f64; 2]>) -> Result<Frame, String> {
        let mut bb = [f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY];
        // the longitude range with negative longitudes moved by 360°: [0, 360) instead of
        // [-180, 180) has no break at the antimeridian
        let (mut e0, mut e1) = (f64::INFINITY, f64::NEG_INFINITY);
        let mut all_pl = true;
        let mut n = 0usize;
        for [lon, lat] in points {
            if !lon.is_finite() || !lat.is_finite() {
                return Err("a coordinate is not a finite number".into());
            }
            all_pl &= in_poland(lon, lat);
            bb = [bb[0].min(lon), bb[1].min(lat), bb[2].max(lon), bb[3].max(lat)];
            let east = if lon < 0.0 { lon + 360.0 } else { lon };
            (e0, e1) = (e0.min(east), e1.max(east));
            n += 1;
        }
        if n == 0 {
            return Err("no coordinates to work with".into());
        }
        if bb[1] < -MAX_ABS_LAT_DEG || bb[3] > MAX_ABS_LAT_DEG {
            return Err(format!("features beyond ±{MAX_ABS_LAT_DEG}° latitude are not supported"));
        }
        // a range within 20° cannot hold both 0° and 180°, so one of the two is unbroken
        let (lon0, lon1) = if e1 - e0 < bb[2] - bb[0] { (e0, e1) } else { (bb[0], bb[2]) };
        if lon1 - lon0 > MAX_LON_SPAN_DEG {
            return Err(format!(
                "the features span {:.1}° of longitude; metric editing needs them within {MAX_LON_SPAN_DEG}°",
                lon1 - lon0
            ));
        }
        if all_pl {
            Ok(Frame::epsg2180())
        } else {
            let lon = 0.5 * (lon0 + lon1);
            Ok(Frame::utm_at(if lon >= 180.0 { lon - 360.0 } else { lon }, 0.5 * (bb[1] + bb[3])))
        }
    }

    /// The frame for lon/lat geometries (see [`Frame::for_positions`]).
    pub fn for_geoms<'a>(geoms: impl IntoIterator<Item = &'a Geom>) -> Result<Frame, String> {
        let mut pts = Vec::new();
        for g in geoms {
            g.for_each_coord(|c| pts.push([c.x, c.y]));
        }
        Frame::for_positions(pts)
    }

    /// A short name for messages: "EPSG:2180" or "UTM 34N".
    pub fn name(&self) -> String {
        match self.kind {
            FrameKind::Epsg2180 => "EPSG:2180".into(),
            FrameKind::Utm { zone, south } => format!("UTM {zone}{}", if south { 'S' } else { 'N' }),
        }
    }

    /// (lon, lat) -> (x, y) metres.
    pub fn fwd(&self, lon: f64, lat: f64) -> (f64, f64) {
        self.tm.forward(lon, lat)
    }

    /// (x, y) metres -> (lon, lat).
    pub fn inv(&self, x: f64, y: f64) -> (f64, f64) {
        self.tm.inverse(x, y)
    }

    pub fn fwd_coord(&self, c: Coord<f64>) -> Coord<f64> {
        let (x, y) = self.fwd(c.x, c.y);
        Coord { x, y }
    }

    pub fn inv_coord(&self, c: Coord<f64>) -> Coord<f64> {
        let (x, y) = self.inv(c.x, c.y);
        Coord { x, y }
    }

    /// Any geo geometry type, lon/lat -> metres.
    pub fn project_geo<G: MapCoords<f64, f64, Output = G>>(&self, g: &G) -> G {
        g.map_coords(|c| self.fwd_coord(c))
    }

    /// Any geo geometry type, metres -> lon/lat.
    pub fn unproject_geo<G: MapCoords<f64, f64, Output = G>>(&self, g: &G) -> G {
        g.map_coords(|c| self.inv_coord(c))
    }

    /// A whole geometry, lon/lat -> metres.
    pub fn project(&self, g: &Geom) -> Geom {
        g.map_coords(|c| self.fwd_coord(c))
    }

    /// A whole geometry, metres -> lon/lat.
    pub fn unproject(&self, g: &Geom) -> Geom {
        g.map_coords(|c| self.inv_coord(c))
    }
}

/// Default snapping distance for [`VertexSnap`]: well above the precision boolean ops keep
/// (i_overlay works on an integer grid), far below any meaningful vertex spacing.
pub const SNAP_TOL_M: f64 = 1e-5;

/// Multiplicative hashing (as rustc's FxHash) for the snapping grid: SipHash costs more than
/// the distance checks it serves.
#[derive(Clone, Copy, Default)]
struct GridHasher(u64);

impl Hasher for GridHasher {
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.write_u64(b as u64);
        }
    }

    fn write_u64(&mut self, x: u64) {
        self.0 = (self.0.rotate_left(5) ^ x).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

/// Grid cells are this many snapping distances wide: an unsnapped vertex then rarely needs
/// to look into the neighbouring cells.
const CELL_PER_TOL: f64 = 8.0;
const NONE: u32 = u32::MAX;

/// Puts original vertices back bit-exactly when unprojecting results.
///
/// A projection round trip and the boolean ops' integer grid move untouched vertices by
/// nanometres. That would break shared boundaries between neighbouring features (the
/// neighbour keeps the original coordinates), so every output vertex within `tol` metres of
/// an input vertex is replaced by that input's original lon/lat (the nearest one; among
/// equally near ones the first registered).
pub struct VertexSnap {
    frame: Frame,
    tol: f64,
    cell: f64,
    /// Grid cell key -> the last vertex registered in it.
    heads: HashMap<u64, u32, BuildHasherDefault<GridHasher>>,
    /// (metric, original lon/lat, previous vertex in the same cell or [`NONE`]).
    verts: Vec<(Coord<f64>, Coord<f64>, u32)>,
}

impl VertexSnap {
    pub fn new(frame: Frame, tol_m: f64) -> VertexSnap {
        let tol = tol_m.max(1e-12);
        VertexSnap { frame, tol, cell: tol * CELL_PER_TOL, heads: HashMap::default(), verts: Vec::new() }
    }

    /// Cells that differ map to different keys unless their indices differ by a multiple of
    /// 2^64, which cannot happen within one TM frame. Equal keys only add candidates anyway.
    fn key(cx: f64, cy: f64) -> u64 {
        (cx as i64 as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ (cy as i64 as u64)
    }

    fn insert(&mut self, m: Coord<f64>, orig: Coord<f64>) {
        let k = VertexSnap::key((m.x / self.cell).floor(), (m.y / self.cell).floor());
        let i = self.verts.len() as u32;
        let prev = self.heads.insert(k, i).unwrap_or(NONE);
        self.verts.push((m, orig, prev));
    }

    /// Registers every vertex of a lon/lat geometry as a snapping target.
    pub fn add(&mut self, lonlat: &Geom) {
        self.reserve(lonlat);
        let frame = self.frame;
        lonlat.for_each_coord(|c| self.insert(frame.fwd_coord(c), c));
    }

    fn reserve(&mut self, g: &Geom) {
        let n = g.num_coords();
        self.verts.reserve(n);
        self.heads.reserve(n);
    }

    /// Projects a lon/lat geometry and registers its vertices: the usual way in.
    pub fn project(&mut self, lonlat: &Geom) -> Geom {
        self.reserve(lonlat);
        let frame = self.frame;
        lonlat.map_coords_mut(|c| {
            let m = frame.fwd_coord(c);
            self.insert(m, c);
            m
        })
    }

    /// The nearest registered vertex within `tol` of `m` as (squared distance, index).
    fn nearest_in(&self, key: u64, m: Coord<f64>, best: &mut Option<(f64, u32)>) {
        let mut i = self.heads.get(&key).copied().unwrap_or(NONE);
        while i != NONE {
            let (pm, _, prev) = self.verts[i as usize];
            let (dx, dy) = (pm.x - m.x, pm.y - m.y);
            let d2 = dx * dx + dy * dy;
            let better = match *best {
                None => d2 <= self.tol * self.tol,
                Some((bd, bi)) => d2 < bd || (d2 == bd && i < bi),
            };
            if better {
                *best = Some((d2, i));
            }
            i = prev;
        }
    }

    fn snap_coord(&self, m: Coord<f64>) -> Coord<f64> {
        self.snapped(m).unwrap_or_else(|| self.frame.inv_coord(m))
    }

    /// The original lon/lat of the registered vertex nearest to the metric position `m`, if
    /// one lies within the snapping distance.
    pub fn snapped(&self, m: Coord<f64>) -> Option<Coord<f64>> {
        let (fx, fy) = (m.x / self.cell, m.y / self.cell);
        let (cx, cy) = (fx.floor(), fy.floor());
        let mut best = None;
        self.nearest_in(VertexSnap::key(cx, cy), m, &mut best);
        // a neighbouring cell can only hold a nearer vertex if it is nearer than the best one
        let (rx, ry) = ((fx - cx) * self.cell, (fy - cy) * self.cell);
        let gap = |d: i8, r: f64| match d {
            -1 => r,
            1 => self.cell - r,
            _ => 0.0,
        };
        for dx in -1i8..=1 {
            for dy in -1i8..=1 {
                if dx == 0 && dy == 0 {
                    continue;
                }
                let (gx, gy) = (gap(dx, rx), gap(dy, ry));
                let limit = best.map_or(self.tol * self.tol, |(d2, _)| d2);
                if gx * gx + gy * gy <= limit {
                    self.nearest_in(VertexSnap::key(cx + dx as f64, cy + dy as f64), m, &mut best);
                }
            }
        }
        best.map(|(_, i)| self.verts[i as usize].1)
    }

    /// Metres -> lon/lat, snapping to registered original vertices.
    pub fn unproject(&self, metric: &Geom) -> Geom {
        metric.map_coords(|c| self.snap_coord(c))
    }

    pub fn frame(&self) -> Frame {
        self.frame
    }
}
