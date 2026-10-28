//! GeoJSON geometries <-> geo types, and the checks every geometry passes before it is
//! written back (the host does not validate or close rings itself).

use geo::{
    BoundingRect, Coord, CoordsIter, LineString, MapCoords, MultiLineString, MultiPoint,
    MultiPolygon, Point, Polygon, Rect, Validation,
};
use rustgis_plugin::{Value, json};

/// Tolerance for comparing lon/lat coordinates. The host re-rounds untouched vertices by about
/// one ulp on every write, so coordinates are never compared with `==`.
pub const COORD_EPS_DEG: f64 = 1e-12;

/// Geometries with more vertices than this are not checked with `geo::Validation`: it is
/// quadratic and exceeds a command's fuel at around 1000 vertices.
pub const MAX_VALIDATE_VERTICES: usize = 300;

/// The geometry dimension, which decides what a geometry can be written into.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dim {
    Point,
    Line,
    Polygon,
}

impl Dim {
    /// From `host.layers`' `geometry` ("Point", "Polyline", "Polygon"); `None` for rasters,
    /// tables and anything unknown.
    pub fn from_layer_type(s: &str) -> Option<Dim> {
        let s = s.to_ascii_lowercase();
        if s.contains("polygon") {
            Some(Dim::Polygon)
        } else if s.contains("line") {
            Some(Dim::Line)
        } else if s.contains("point") {
            Some(Dim::Point)
        } else {
            None
        }
    }

    /// For messages: "point", "line", "polygon".
    pub fn noun(self) -> &'static str {
        match self {
            Dim::Point => "point",
            Dim::Line => "line",
            Dim::Polygon => "polygon",
        }
    }
}

/// A feature geometry. Single and multi variants are kept apart because the host does too
/// (a one-part MultiPolygon comes back as a Polygon), and writing back the same kind keeps
/// edits minimal.
#[derive(Clone, Debug, PartialEq)]
pub enum Geom {
    Point(Point<f64>),
    MultiPoint(MultiPoint<f64>),
    LineString(LineString<f64>),
    MultiLineString(MultiLineString<f64>),
    Polygon(Polygon<f64>),
    MultiPolygon(MultiPolygon<f64>),
}

impl Geom {
    pub fn dim(&self) -> Dim {
        match self {
            Geom::Point(_) | Geom::MultiPoint(_) => Dim::Point,
            Geom::LineString(_) | Geom::MultiLineString(_) => Dim::Line,
            Geom::Polygon(_) | Geom::MultiPolygon(_) => Dim::Polygon,
        }
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            Geom::Point(_) => "Point",
            Geom::MultiPoint(_) => "MultiPoint",
            Geom::LineString(_) => "LineString",
            Geom::MultiLineString(_) => "MultiLineString",
            Geom::Polygon(_) => "Polygon",
            Geom::MultiPolygon(_) => "MultiPolygon",
        }
    }

    /// Polygon or MultiPolygon as a MultiPolygon; `None` for other dimensions.
    pub fn to_multi_polygon(&self) -> Option<MultiPolygon<f64>> {
        match self {
            Geom::Polygon(p) => Some(MultiPolygon(vec![p.clone()])),
            Geom::MultiPolygon(m) => Some(m.clone()),
            _ => None,
        }
    }

    /// LineString or MultiLineString as a MultiLineString; `None` for other dimensions.
    pub fn to_multi_line(&self) -> Option<MultiLineString<f64>> {
        match self {
            Geom::LineString(l) => Some(MultiLineString(vec![l.clone()])),
            Geom::MultiLineString(m) => Some(m.clone()),
            _ => None,
        }
    }

    /// Point or MultiPoint as points; `None` for other dimensions.
    pub fn to_points(&self) -> Option<Vec<Point<f64>>> {
        match self {
            Geom::Point(p) => Some(vec![*p]),
            Geom::MultiPoint(m) => Some(m.0.clone()),
            _ => None,
        }
    }

    /// Number of parts (1 for single geometries).
    pub fn num_parts(&self) -> usize {
        match self {
            Geom::Point(_) | Geom::LineString(_) | Geom::Polygon(_) => 1,
            Geom::MultiPoint(m) => m.0.len(),
            Geom::MultiLineString(m) => m.0.len(),
            Geom::MultiPolygon(m) => m.0.len(),
        }
    }

    pub fn num_coords(&self) -> usize {
        match self {
            Geom::Point(g) => g.coords_count(),
            Geom::MultiPoint(g) => g.coords_count(),
            Geom::LineString(g) => g.coords_count(),
            Geom::MultiLineString(g) => g.coords_count(),
            Geom::Polygon(g) => g.coords_count(),
            Geom::MultiPolygon(g) => g.coords_count(),
        }
    }

    pub fn for_each_coord(&self, mut f: impl FnMut(Coord<f64>)) {
        match self {
            Geom::Point(g) => g.coords_iter().for_each(&mut f),
            Geom::MultiPoint(g) => g.coords_iter().for_each(&mut f),
            Geom::LineString(g) => g.coords_iter().for_each(&mut f),
            Geom::MultiLineString(g) => g.coords_iter().for_each(&mut f),
            Geom::Polygon(g) => g.coords_iter().for_each(&mut f),
            Geom::MultiPolygon(g) => g.coords_iter().for_each(&mut f),
        }
    }

    pub fn map_coords(&self, f: impl Fn(Coord<f64>) -> Coord<f64> + Copy) -> Geom {
        match self {
            Geom::Point(g) => Geom::Point(g.map_coords(f)),
            Geom::MultiPoint(g) => Geom::MultiPoint(g.map_coords(f)),
            Geom::LineString(g) => Geom::LineString(g.map_coords(f)),
            Geom::MultiLineString(g) => Geom::MultiLineString(g.map_coords(f)),
            Geom::Polygon(g) => Geom::Polygon(g.map_coords(f)),
            Geom::MultiPolygon(g) => Geom::MultiPolygon(g.map_coords(f)),
        }
    }

    /// [`Geom::map_coords`] with a stateful function, called once per position in
    /// [`Geom::for_each_coord`] order.
    pub fn map_coords_mut(&self, mut f: impl FnMut(Coord<f64>) -> Coord<f64>) -> Geom {
        fn ls(l: &LineString<f64>, f: &mut impl FnMut(Coord<f64>) -> Coord<f64>) -> LineString<f64> {
            LineString(l.0.iter().map(|c| f(*c)).collect())
        }
        fn poly(p: &Polygon<f64>, f: &mut impl FnMut(Coord<f64>) -> Coord<f64>) -> Polygon<f64> {
            let ext = ls(p.exterior(), f);
            Polygon::new(ext, p.interiors().iter().map(|r| ls(r, f)).collect())
        }
        match self {
            Geom::Point(p) => Geom::Point(Point(f(p.0))),
            Geom::MultiPoint(m) => Geom::MultiPoint(MultiPoint(m.0.iter().map(|p| Point(f(p.0))).collect())),
            Geom::LineString(l) => Geom::LineString(ls(l, &mut f)),
            Geom::MultiLineString(m) => Geom::MultiLineString(MultiLineString(m.0.iter().map(|l| ls(l, &mut f)).collect())),
            Geom::Polygon(p) => Geom::Polygon(poly(p, &mut f)),
            Geom::MultiPolygon(m) => Geom::MultiPolygon(MultiPolygon(m.0.iter().map(|p| poly(p, &mut f)).collect())),
        }
    }

    pub fn bbox(&self) -> Option<Rect<f64>> {
        match self {
            Geom::Point(g) => Some(g.bounding_rect()),
            Geom::MultiPoint(g) => g.bounding_rect(),
            Geom::LineString(g) => g.bounding_rect(),
            Geom::MultiLineString(g) => g.bounding_rect(),
            Geom::Polygon(g) => g.bounding_rect(),
            Geom::MultiPolygon(g) => g.bounding_rect(),
        }
    }

    /// As a `geo::Geometry`, for the crate's generic predicates (`Intersects`, `Contains`…).
    pub fn to_geo(&self) -> geo::Geometry<f64> {
        match self {
            Geom::Point(g) => (*g).into(),
            Geom::MultiPoint(g) => g.clone().into(),
            Geom::LineString(g) => g.clone().into(),
            Geom::MultiLineString(g) => g.clone().into(),
            Geom::Polygon(g) => g.clone().into(),
            Geom::MultiPolygon(g) => g.clone().into(),
        }
    }
}

pub fn bboxes_intersect(a: &Rect<f64>, b: &Rect<f64>) -> bool {
    a.min().x <= b.max().x && b.min().x <= a.max().x && a.min().y <= b.max().y && b.min().y <= a.max().y
}

pub fn same_coord(a: Coord<f64>, b: Coord<f64>, eps: f64) -> bool {
    (a.x - b.x).abs() <= eps && (a.y - b.y).abs() <= eps
}

// ---------- parsing ----------

fn position(v: &Value) -> Result<Coord<f64>, String> {
    let a = v.as_array().ok_or("a position is not an array")?;
    match (a.first().and_then(Value::as_f64), a.get(1).and_then(Value::as_f64)) {
        (Some(x), Some(y)) if x.is_finite() && y.is_finite() => Ok(Coord { x, y }),
        _ => Err("a position is not two finite numbers".into()),
    }
}

fn positions(v: &Value) -> Result<Vec<Coord<f64>>, String> {
    v.as_array().ok_or("coordinates are not an array")?.iter().map(position).collect()
}

fn line(v: &Value) -> Result<LineString<f64>, String> {
    Ok(LineString(positions(v)?))
}

/// Rings are closed on the way in, so every geo polygon in the plugin is closed.
fn polygon(v: &Value) -> Result<Polygon<f64>, String> {
    let rings = v.as_array().ok_or("polygon rings are not an array")?;
    let mut it = rings.iter().map(|r| line(r).map(close_ring));
    let ext = it.next().ok_or("a polygon has no rings")??;
    Ok(Polygon::new(ext, it.collect::<Result<_, _>>()?))
}

fn array<'a>(v: &'a Value, what: &str) -> Result<&'a Vec<Value>, String> {
    v.as_array().ok_or_else(|| format!("{what} coordinates are not an array"))
}

/// A GeoJSON geometry object -> [`Geom`]. GeometryCollections and unknown types are errors.
pub fn from_geojson(v: &Value) -> Result<Geom, String> {
    let t = v.get("type").and_then(Value::as_str).ok_or("a geometry has no type")?;
    let c = v.get("coordinates").ok_or_else(|| format!("{t} has no coordinates"))?;
    Ok(match t {
        "Point" => Geom::Point(Point(position(c)?)),
        "MultiPoint" => Geom::MultiPoint(MultiPoint(positions(c)?.into_iter().map(Point).collect())),
        "LineString" => Geom::LineString(line(c)?),
        "MultiLineString" => Geom::MultiLineString(MultiLineString(
            array(c, t)?.iter().map(line).collect::<Result<_, _>>()?,
        )),
        "Polygon" => Geom::Polygon(polygon(c)?),
        "MultiPolygon" => {
            Geom::MultiPolygon(MultiPolygon(array(c, t)?.iter().map(polygon).collect::<Result<_, _>>()?))
        }
        other => return Err(format!("{other} geometries are not supported")),
    })
}

// ---------- writing ----------

fn pos(c: &Coord<f64>) -> Value {
    json!([c.x, c.y])
}

fn ls_json(l: &LineString<f64>) -> Value {
    Value::Array(l.0.iter().map(pos).collect())
}

fn poly_json(p: &Polygon<f64>) -> Value {
    Value::Array(std::iter::once(p.exterior()).chain(p.interiors()).map(ls_json).collect())
}

/// [`Geom`] -> GeoJSON geometry object, as is (run [`prepare_output`] first for anything
/// that goes back to the host).
pub fn to_geojson(g: &Geom) -> Value {
    let coords = match g {
        Geom::Point(p) => pos(&p.0),
        Geom::MultiPoint(m) => Value::Array(m.0.iter().map(|p| pos(&p.0)).collect()),
        Geom::LineString(l) => ls_json(l),
        Geom::MultiLineString(m) => Value::Array(m.0.iter().map(ls_json).collect()),
        Geom::Polygon(p) => poly_json(p),
        Geom::MultiPolygon(m) => Value::Array(m.0.iter().map(poly_json).collect()),
    };
    json!({"type": g.type_name(), "coordinates": coords})
}

// ---------- repair and checks ----------

/// Closes a ring whose last position differs from its first (exactly: snapping near-equal
/// ends together is the caller's decision).
pub fn close_ring(mut r: LineString<f64>) -> LineString<f64> {
    if let (Some(a), Some(b)) = (r.0.first().copied(), r.0.last().copied())
        && a != b
    {
        r.0.push(a);
    }
    r
}

/// Drops consecutive duplicate positions (within `eps`), which boolean ops and edits leave
/// behind and which make rings degenerate.
pub fn dedup_coords(r: &mut LineString<f64>, eps: f64) {
    r.0.dedup_by(|b, a| same_coord(*a, *b, eps));
}

fn clean_polygon(p: Polygon<f64>, eps: f64) -> Option<Polygon<f64>> {
    let (ext, ints) = p.into_inner();
    let fix = |r: LineString<f64>| {
        let mut r = close_ring(r);
        dedup_coords(&mut r, eps);
        // dedup may have merged the closing position into its predecessor
        let r = close_ring(r);
        (r.0.len() >= 4).then_some(r)
    };
    let ext = fix(ext)?;
    Some(Polygon::new(ext, ints.into_iter().filter_map(fix).collect()))
}

/// Closes rings, drops duplicate consecutive positions and degenerate rings/parts, and
/// collapses a one-part MultiPolygon/MultiLineString/MultiPoint to its single type. Fails
/// when nothing is left. `eps` is in the geometry's own units (use [`COORD_EPS_DEG`] for
/// lon/lat).
pub fn normalize(g: Geom, eps: f64) -> Result<Geom, String> {
    let empty = || Err::<Geom, String>("the result is empty".into());
    Ok(match g {
        Geom::Polygon(p) => Geom::Polygon(clean_polygon(p, eps).ok_or("the result polygon is degenerate")?),
        Geom::MultiPolygon(m) => {
            let mut v: Vec<_> = m.0.into_iter().filter_map(|p| clean_polygon(p, eps)).collect();
            match v.len() {
                0 => return empty(),
                1 => Geom::Polygon(v.pop().unwrap()),
                _ => Geom::MultiPolygon(MultiPolygon(v)),
            }
        }
        Geom::LineString(mut l) => {
            dedup_coords(&mut l, eps);
            if l.0.len() < 2 {
                return Err("the result line is degenerate".into());
            }
            Geom::LineString(l)
        }
        Geom::MultiLineString(m) => {
            let mut v: Vec<_> = m
                .0
                .into_iter()
                .map(|mut l| {
                    dedup_coords(&mut l, eps);
                    l
                })
                .filter(|l| l.0.len() >= 2)
                .collect();
            match v.len() {
                0 => return empty(),
                1 => Geom::LineString(v.pop().unwrap()),
                _ => Geom::MultiLineString(MultiLineString(v)),
            }
        }
        Geom::MultiPoint(mut m) => match m.0.len() {
            0 => return empty(),
            1 => Geom::Point(m.0.pop().unwrap()),
            _ => Geom::MultiPoint(m),
        },
        p @ Geom::Point(_) => p,
    })
}

fn ring_ok(r: &LineString<f64>, what: &str) -> Result<(), String> {
    if r.0.len() < 4 {
        return Err(format!("{what} has fewer than 4 positions"));
    }
    if r.0.first() != r.0.last() {
        return Err(format!("{what} is not closed"));
    }
    Ok(())
}

fn polygon_ok(p: &Polygon<f64>) -> Result<(), String> {
    ring_ok(p.exterior(), "a polygon's outer ring")?;
    p.interiors().iter().try_for_each(|r| ring_ok(r, "a polygon hole"))
}

/// The checks every geometry passes before it goes to the host: non-empty, finite, rings
/// closed with at least 4 positions, lines with at least 2. Cheap (linear); topology is
/// checked separately with [`is_valid_small`].
pub fn sanity_check(g: &Geom) -> Result<(), String> {
    let mut finite = true;
    g.for_each_coord(|c| finite &= c.x.is_finite() && c.y.is_finite());
    if !finite {
        return Err("the result has a non-finite coordinate".into());
    }
    match g {
        Geom::Point(_) => Ok(()),
        Geom::MultiPoint(m) if m.0.is_empty() => Err("the result is empty".into()),
        Geom::MultiPoint(_) => Ok(()),
        Geom::LineString(l) if l.0.len() < 2 => Err("a result line has fewer than 2 positions".into()),
        Geom::LineString(_) => Ok(()),
        Geom::MultiLineString(m) if m.0.is_empty() => Err("the result is empty".into()),
        Geom::MultiLineString(m) => {
            m.0.iter().all(|l| l.0.len() >= 2).then_some(()).ok_or("a result line has fewer than 2 positions".into())
        }
        Geom::Polygon(p) => polygon_ok(p),
        Geom::MultiPolygon(m) if m.0.is_empty() => Err("the result is empty".into()),
        Geom::MultiPolygon(m) => m.0.iter().try_for_each(polygon_ok),
    }
}

/// [`normalize`] then [`sanity_check`]: the one call to make on a lon/lat result before
/// writing it.
pub fn prepare_output(g: Geom) -> Result<Geom, String> {
    let g = normalize(g, COORD_EPS_DEG)?;
    sanity_check(&g)?;
    Ok(g)
}

/// OGC validity, only for small geometries: `None` when the geometry has more than
/// `max_vertices` vertices (use [`MAX_VALIDATE_VERTICES`]), since the check is quadratic.
pub fn is_valid_small(g: &Geom, max_vertices: usize) -> Option<bool> {
    if g.num_coords() > max_vertices {
        return None;
    }
    Some(match g {
        Geom::Point(x) => x.is_valid(),
        Geom::MultiPoint(x) => x.is_valid(),
        Geom::LineString(x) => x.is_valid(),
        Geom::MultiLineString(x) => x.is_valid(),
        Geom::Polygon(x) => x.is_valid(),
        Geom::MultiPolygon(x) => x.is_valid(),
    })
}

/// Wraps polygons as a Polygon or MultiPolygon [`Geom`] (not yet normalized).
pub fn from_polygons(mut v: Vec<Polygon<f64>>) -> Geom {
    if v.len() == 1 { Geom::Polygon(v.pop().unwrap()) } else { Geom::MultiPolygon(MultiPolygon(v)) }
}

/// Wraps lines as a LineString or MultiLineString [`Geom`] (not yet normalized).
pub fn from_lines(mut v: Vec<LineString<f64>>) -> Geom {
    if v.len() == 1 { Geom::LineString(v.pop().unwrap()) } else { Geom::MultiLineString(MultiLineString(v)) }
}

// ---------- i_overlay contours ----------

/// A position as i_overlay takes it.
pub type P2 = [f64; 2];

pub fn ring_points(l: &LineString<f64>) -> Vec<P2> {
    l.0.iter().map(|c| [c.x, c.y]).collect()
}

pub fn line_from_points(pts: &[P2]) -> LineString<f64> {
    LineString(pts.iter().map(|p| Coord { x: p[0], y: p[1] }).collect())
}

/// A polygon as i_overlay contours: outer ring then holes, open (no closing position).
pub fn contours(p: &Polygon<f64>) -> Vec<Vec<P2>> {
    std::iter::once(p.exterior())
        .chain(p.interiors())
        .map(|r| {
            let mut v = ring_points(r);
            if v.len() > 1 && v.first() == v.last() {
                v.pop();
            }
            v
        })
        .collect()
}

/// An i_overlay shape (outer contour + holes, open) as a closed geo polygon.
pub fn shape_to_polygon(shape: &[Vec<P2>]) -> Polygon<f64> {
    let ring = |r: &Vec<P2>| close_ring(line_from_points(r));
    match shape.split_first() {
        Some((ext, holes)) => Polygon::new(ring(ext), holes.iter().map(ring).collect()),
        None => Polygon::new(LineString(vec![]), vec![]),
    }
}

// ---------- JSON text without a Value tree ----------
//
// Geometries go to and come from the host as JSON text. Building a `Value` per position and
// formatting through it costs several times more fuel than projecting the vertex, so they
// are written and read here directly. Numbers keep the shortest text that parses back to the
// same f64, and parsing is correctly rounded.

const fn pow10_table() -> [u128; 21] {
    let mut t = [1u128; 21];
    let mut i = 1;
    while i < 21 {
        t[i] = t[i - 1] * 10;
        i += 1;
    }
    t
}

const POW10: [u128; 21] = pow10_table();
const POW10_F: [f64; 23] = [
    1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12, 1e13, 1e14, 1e15, 1e16, 1e17, 1e18,
    1e19, 1e20, 1e21, 1e22,
];
const fn digit_pairs() -> [u8; 200] {
    let mut t = [0u8; 200];
    let mut i = 0;
    while i < 100 {
        t[2 * i] = b'0' + (i / 10) as u8;
        t[2 * i + 1] = b'0' + (i % 10) as u8;
        i += 1;
    }
    t
}

const DIGIT_PAIRS: [u8; 200] = digit_pairs();
const TWO_53: f64 = 9_007_199_254_740_992.0;
/// The exact integer paths below cover 2^-10 <= |x| < 2^53, where every coordinate lies.
const FAST_MIN: f64 = 1.0 / 1024.0;

/// x = m * 2^e for a positive normal x.
fn decompose(x: f64) -> (u64, i32) {
    let b = x.to_bits();
    ((b & ((1 << 52) - 1)) | (1 << 52), ((b >> 52) & 0x7ff) as i32 - 1075)
}

/// The decimals D / 10^k that parse back to x = m * 2^e: D * 2^(2 - e) strictly inside
/// (lo, hi) does; on a bound it is a tie between x and its neighbour (rounded to even).
fn rounding_range(m: u64, k: usize) -> (u128, u128) {
    let four_m = (m as u128) << 2;
    // below a power of two the next smaller f64 is half as far away
    let below = if m == 1 << 52 { 1 } else { 2 };
    ((four_m - below) * POW10[k], (four_m + 2) * POW10[k])
}

/// The D for which D / 10^k parses back to x = m * 2^e, closest to x; exact for any D.
fn digits_exact(m: u64, e: i32, k: usize) -> Option<u64> {
    let s = (2 - e) as u32;
    let (lo, hi) = rounding_range(m, k);
    let (dmin, dmax) = ((lo >> s) + 1, (hi - 1) >> s);
    if dmin > dmax {
        return None;
    }
    let near = (((m as u128) << 2) * POW10[k] + (1 << (s - 1))) >> s;
    Some(near.clamp(dmin, dmax) as u64)
}

/// The shortest (D, k) with D / 10^k parsing back to x (2^-10 <= x < 2^53), D closest to x.
fn shortest_decimal(x: f64) -> Option<(u64, usize)> {
    if !(FAST_MIN..TWO_53).contains(&x) {
        return None;
    }
    let (m, e) = decompose(x);
    // For D < 2^53 both D and 10^k are exact f64, so D / 10^k rounds like a correct parser
    // would: one division decides. The integer path is for 17-digit D.
    let digits = |k: usize| -> Option<u64> {
        let t = x * POW10_F[k];
        if t >= TWO_53 - 2.0 {
            return digits_exact(m, e, k);
        }
        let d = t.round_ties_even();
        [d, d - 1.0, d + 1.0].into_iter().find(|&c| c / POW10_F[k] == x).map(|c| c as u64)
    };
    // 17 significant digits always identify an f64
    let kmax = if x >= 1.0 {
        17 - (1..17).find(|&d| x < POW10_F[d]).unwrap_or(16)
    } else {
        17 + (0..3).take_while(|&z| x * POW10_F[z + 1] < 1.0).count()
    };
    // Coordinates usually need 16 or 17 digits: step down first, bisect for short ones.
    let (mut k, mut d) = match digits(kmax - 1) {
        Some(d) => (kmax - 1, d),
        // 17 digits rounded to nearest always parse back (|error| <= 5e-17 x < half an ulp),
        // except just above a power of two where the gap below is half as wide. (Below 1 the
        // digit count is estimated, so the checked path is taken.)
        None if m != 1 << 52 && x >= 1.0 && e < 0 => {
            let d = ((m as u128) * POW10[kmax] + (1 << (-e - 1) as u32)) >> (-e) as u32;
            return Some((d as u64, kmax));
        }
        None => {
            return match digits_exact(m, e, kmax) {
                Some(d) => Some((d, kmax)),
                None if kmax < 20 => Some((digits_exact(m, e, kmax + 1)?, kmax + 1)),
                None => None,
            };
        }
    };
    for _ in 0..2 {
        match k.checked_sub(1).and_then(|k1| Some((k1, digits(k1)?))) {
            Some((k1, d1)) => (k, d) = (k1, d1),
            None => return Some((d, k)),
        }
    }
    let mut lo = 0;
    while lo < k {
        let mid = (lo + k) / 2;
        match digits(mid) {
            Some(dm) => (k, d) = (mid, dm),
            None => lo = mid + 1,
        }
    }
    Some((d, k))
}

/// Appends `x` as JSON: the shortest text that parses back to exactly `x` (non-finite values,
/// which [`sanity_check`] keeps out of any output, become `null`).
pub fn write_f64(out: &mut String, x: f64) {
    let Some((d, k)) = shortest_decimal(x.abs()) else {
        out.push_str(&rustgis_plugin::serde_json::to_string(&x).unwrap_or_else(|_| "null".into()));
        return;
    };
    let mut buf = [b'0'; 48];
    let mut i = buf.len();
    let mut v = d;
    let mut left = k;
    // fraction digits two at a time, then the integer part
    while left >= 2 {
        let r = (v % 100) as usize * 2;
        v /= 100;
        buf[i - 2..i].copy_from_slice(&DIGIT_PAIRS[r..r + 2]);
        i -= 2;
        left -= 2;
    }
    if left == 1 {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    if k == 0 {
        i -= 1; // the '0' after the point
    }
    i -= 1;
    buf[i] = b'.';
    loop {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    if x < 0.0 {
        i -= 1;
        buf[i] = b'-';
    }
    // SAFETY: only ASCII digits, '.' and '-' were written.
    out.push_str(unsafe { std::str::from_utf8_unchecked(&buf[i..]) });
}

/// w / 10^k correctly rounded, for w < 10^19; `None` outside the exact integer range.
#[inline(always)]
fn decimal_to_f64(w: u64, k: usize) -> Option<f64> {
    if w == 0 {
        return Some(0.0);
    }
    if w < 1 << 53 && k <= 22 {
        // both operands exact, so the one division rounds correctly
        return Some(w as f64 / POW10_F[k]);
    }
    if k > 20 {
        return None;
    }
    let mut x = w as f64 / POW10_F[k];
    for _ in 0..4 {
        if !(FAST_MIN..TWO_53).contains(&x) {
            return None;
        }
        let (m, e) = decompose(x);
        let a = (w as u128) << (2 - e) as u32;
        let (lo, hi) = rounding_range(m, k);
        let even = m & 1 == 0;
        if a < lo || (a == lo && !even) {
            x = f64::from_bits(x.to_bits() - 1);
            if a == lo {
                return Some(x);
            }
        } else if a > hi || (a == hi && !even) {
            x = f64::from_bits(x.to_bits() + 1);
            if a == hi {
                return Some(x);
            }
        } else {
            return Some(x);
        }
    }
    None
}

fn write_pos(out: &mut String, c: &Coord<f64>) {
    out.push('[');
    write_f64(out, c.x);
    out.push(',');
    write_f64(out, c.y);
    out.push(']');
}

fn write_list<T>(out: &mut String, items: &[T], mut f: impl FnMut(&mut String, &T)) {
    out.push('[');
    for (i, it) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        f(out, it);
    }
    out.push(']');
}

fn write_ls(out: &mut String, l: &LineString<f64>) {
    write_list(out, &l.0, write_pos);
}

fn write_poly(out: &mut String, p: &Polygon<f64>) {
    out.push('[');
    write_ls(out, p.exterior());
    for r in p.interiors() {
        out.push(',');
        write_ls(out, r);
    }
    out.push(']');
}

/// Appends `g` as a GeoJSON geometry object: the text [`to_geojson`] would serialize to.
pub fn write_geojson(out: &mut String, g: &Geom) {
    out.reserve(48 + 40 * g.num_coords());
    out.push_str("{\"type\":\"");
    out.push_str(g.type_name());
    out.push_str("\",\"coordinates\":");
    match g {
        Geom::Point(p) => write_pos(out, &p.0),
        Geom::MultiPoint(m) => write_list(out, &m.0, |o, p| write_pos(o, &p.0)),
        Geom::LineString(l) => write_ls(out, l),
        Geom::MultiLineString(m) => write_list(out, &m.0, write_ls),
        Geom::Polygon(p) => write_poly(out, p),
        Geom::MultiPolygon(m) => write_list(out, &m.0, write_poly),
    }
    out.push('}');
}

/// The value of 8 ASCII digits at `b[i..]` read as one little-endian word (the SWAR method of
/// simdjson); `None` unless all 8 are digits.
#[inline(always)]
fn eight_digits(b: &[u8], i: usize) -> Option<u64> {
    let v = u64::from_le_bytes(b.get(i..i + 8)?.try_into().ok()?);
    let d = v.wrapping_sub(0x3030_3030_3030_3030);
    if v & 0xf0f0_f0f0_f0f0_f0f0 != 0x3030_3030_3030_3030 || d.wrapping_add(0x0606_0606_0606_0606) & 0xf0f0_f0f0_f0f0_f0f0 != 0 {
        return None;
    }
    let d = (d & 0x0f0f_0f0f_0f0f_0f0f).wrapping_mul(2561) >> 8;
    let d = (d & 0x00ff_00ff_00ff_00ff).wrapping_mul(6_553_601) >> 16;
    Some(((d & 0x0000_ffff_0000_ffff).wrapping_mul(42_949_672_960_001) >> 32) as u32 as u64)
}

/// A forward-only reader over JSON text, for the parts of host replies that are too big for a
/// `Value` tree. Anything it does not expect is an error; callers fall back to serde_json.
pub struct JsonReader<'a> {
    b: &'a [u8],
    i: usize,
}

type JRes<T> = Result<T, String>;

impl<'a> JsonReader<'a> {
    pub fn new(b: &'a [u8]) -> JsonReader<'a> {
        JsonReader { b, i: 0 }
    }

    #[inline(always)]
    fn ws(&mut self) {
        while let Some(b' ' | b'\n' | b'\r' | b'\t') = self.b.get(self.i) {
            self.i += 1;
        }
    }

    /// The next significant byte, not consumed.
    #[inline(always)]
    pub fn peek(&mut self) -> Option<u8> {
        self.ws();
        self.b.get(self.i).copied()
    }

    #[inline(always)]
    fn eat(&mut self, c: u8) -> JRes<()> {
        if self.peek() == Some(c) {
            self.i += 1;
            Ok(())
        } else {
            Err(format!("JSON: expected '{}' at byte {}", c as char, self.i))
        }
    }

    /// Consumes `null` if it comes next.
    pub fn null(&mut self) -> bool {
        let n = self.peek() == Some(b'n') && self.b[self.i..].starts_with(b"null");
        if n {
            self.i += 4;
        }
        n
    }

    /// A string's raw bytes between the quotes (escapes left as they are).
    fn str_raw(&mut self) -> JRes<&'a [u8]> {
        self.eat(b'"')?;
        let start = self.i;
        loop {
            match self.b.get(self.i) {
                Some(b'"') => break,
                Some(b'\\') => self.i += 2,
                Some(_) => self.i += 1,
                None => return Err("JSON: unterminated string".into()),
            }
        }
        self.i += 1;
        Ok(&self.b[start..self.i - 1])
    }

    pub fn string(&mut self) -> JRes<String> {
        let start = self.i;
        let raw = self.str_raw()?;
        if raw.contains(&b'\\') {
            let quoted = &self.b[start..self.i];
            let quoted = &quoted[quoted.iter().position(|&c| c == b'"').unwrap_or(0)..];
            return rustgis_plugin::serde_json::from_slice(quoted).map_err(|e| e.to_string());
        }
        String::from_utf8(raw.to_vec()).map_err(|e| e.to_string())
    }

    /// Skips one value and returns its text.
    pub fn skip(&mut self) -> JRes<&'a [u8]> {
        self.ws();
        let start = self.i;
        match self.b.get(self.i) {
            Some(b'"') => {
                self.str_raw()?;
            }
            Some(b'{' | b'[') => {
                let mut depth = 0usize;
                loop {
                    match self.b.get(self.i) {
                        None => return Err("JSON: unexpected end".into()),
                        Some(b'"') => {
                            self.str_raw()?;
                            continue;
                        }
                        Some(b'{' | b'[') => depth += 1,
                        Some(b'}' | b']') => {
                            depth -= 1;
                            if depth == 0 {
                                self.i += 1;
                                break;
                            }
                        }
                        _ => {}
                    }
                    self.i += 1;
                }
            }
            _ => {
                while let Some(c) = self.b.get(self.i) {
                    if matches!(c, b',' | b'}' | b']' | b' ' | b'\n' | b'\r' | b'\t') {
                        break;
                    }
                    self.i += 1;
                }
            }
        }
        if self.i == start {
            return Err(format!("JSON: expected a value at byte {start}"));
        }
        Ok(&self.b[start..self.i])
    }

    /// Calls `f` with every key of an object; `f` must consume the value.
    pub fn object(&mut self, mut f: impl FnMut(&mut Self, &'a [u8]) -> JRes<()>) -> JRes<()> {
        self.eat(b'{')?;
        if self.peek() == Some(b'}') {
            self.i += 1;
            return Ok(());
        }
        loop {
            let k = self.str_raw()?;
            self.eat(b':')?;
            f(self, k)?;
            match self.peek() {
                Some(b',') => self.i += 1,
                Some(b'}') => {
                    self.i += 1;
                    return Ok(());
                }
                _ => return Err(format!("JSON: expected ',' or '}}' at byte {}", self.i)),
            }
        }
    }

    /// Calls `f` for every element of an array; `f` must consume it.
    pub fn array(&mut self, mut f: impl FnMut(&mut Self) -> JRes<()>) -> JRes<()> {
        self.eat(b'[')?;
        if self.peek() == Some(b']') {
            self.i += 1;
            return Ok(());
        }
        loop {
            f(self)?;
            match self.peek() {
                Some(b',') => self.i += 1,
                Some(b']') => {
                    self.i += 1;
                    return Ok(());
                }
                _ => return Err(format!("JSON: expected ',' or ']' at byte {}", self.i)),
            }
        }
    }

    #[inline(always)]
    fn digit_run(&mut self, w: &mut u64) {
        while let Some(v) = eight_digits(self.b, self.i) {
            *w = w.wrapping_mul(100_000_000).wrapping_add(v);
            self.i += 8;
        }
        while let Some(&c) = self.b.get(self.i) {
            let d = c.wrapping_sub(b'0');
            if d > 9 {
                break;
            }
            *w = w.wrapping_mul(10).wrapping_add(d as u64);
            self.i += 1;
        }
    }

    pub fn number(&mut self) -> JRes<f64> {
        self.ws();
        let start = self.i;
        let neg = self.b.get(self.i) == Some(&b'-');
        self.i += neg as usize;
        let int_start = self.i;
        let mut w = 0u64;
        self.digit_run(&mut w);
        let mut ndigits = self.i - int_start;
        let mut k = 0;
        if self.b.get(self.i) == Some(&b'.') {
            self.i += 1;
            let frac_start = self.i;
            self.digit_run(&mut w);
            k = self.i - frac_start;
            ndigits += k;
        }
        let mut exact = ndigits <= 19;
        if let Some(b'e' | b'E') = self.b.get(self.i) {
            exact = false;
            self.i += 1;
            while let Some(b'0'..=b'9' | b'+' | b'-') = self.b.get(self.i) {
                self.i += 1;
            }
        }
        if ndigits == 0 {
            return Err(format!("JSON: expected a number at byte {start}"));
        }
        let v = match exact.then(|| decimal_to_f64(w, k)).flatten() {
            Some(v) => v,
            None => std::str::from_utf8(&self.b[int_start..self.i])
                .ok()
                .and_then(|s| s.parse::<f64>().ok())
                .ok_or_else(|| format!("JSON: bad number at byte {start}"))?,
        };
        Ok(if neg { -v } else { v })
    }

    /// Nested coordinate arrays of any depth, flattened: positions go to `n.pts`, and each
    /// array of height h >= 1 records its end in `n.ends[h - 1]` (the count of elements one
    /// level down). Returns the height (0 for a position). Empty arrays are an error.
    fn nested(&mut self, n: &mut Nested) -> JRes<usize> {
        self.eat(b'[')?;
        if let Some(b'0'..=b'9' | b'-') = self.peek() {
            let x = self.number()?;
            self.eat(b',')?;
            let y = self.number()?;
            while self.peek() == Some(b',') {
                self.i += 1;
                self.number()?;
            }
            self.eat(b']')?;
            if !(x.is_finite() && y.is_finite()) {
                return Err("a position is not two finite numbers".into());
            }
            n.pts.push(Coord { x, y });
            return Ok(0);
        }
        let mut h = None;
        loop {
            let hc = self.nested(n)?;
            if h.is_some_and(|h| h != hc) || hc > 2 {
                return Err("JSON: irregular coordinates".into());
            }
            h = Some(hc);
            match self.peek() {
                Some(b',') => self.i += 1,
                Some(b']') => {
                    self.i += 1;
                    break;
                }
                _ => return Err(format!("JSON: expected ',' or ']' at byte {}", self.i)),
            }
        }
        let h = h.unwrap_or(0) + 1;
        let end = if h == 1 { n.pts.len() } else { n.ends[h - 2].len() };
        n.ends[h - 1].push(end);
        Ok(h)
    }

    /// A GeoJSON geometry object, as [`from_geojson`] reads it; `None` for a
    /// GeometryCollection. The coordinates are read before the type is known (the host sorts
    /// keys), so they come flattened and are cut into parts afterwards.
    pub fn geometry(&mut self) -> JRes<Option<Geom>> {
        let (mut t, mut n, mut h) = (None, Nested::default(), None);
        self.object(|r, k| {
            match k {
                b"type" => t = Some(r.str_raw()?),
                b"coordinates" => h = Some(r.nested(&mut n)?),
                _ => {
                    r.skip()?;
                }
            }
            Ok(())
        })?;
        let t = t.ok_or("a geometry has no type")?;
        if t == b"GeometryCollection" {
            return Ok(None);
        }
        let want = match t {
            b"Point" => 0,
            b"MultiPoint" | b"LineString" => 1,
            b"MultiLineString" | b"Polygon" => 2,
            b"MultiPolygon" => 3,
            _ => return Err(format!("{} geometries are not supported", String::from_utf8_lossy(t))),
        };
        if h != Some(want) {
            return Err("JSON: coordinates do not fit the geometry type".into());
        }
        let Nested { pts, ends } = n;
        let lines = |from: usize, to: usize| -> Vec<LineString<f64>> {
            let mut a = if from == 0 { 0 } else { ends[0][from - 1] };
            ends[0][from..to]
                .iter()
                .map(|&b| {
                    let l = LineString(pts[a..b].to_vec());
                    a = b;
                    l
                })
                .collect()
        };
        let polygon = |from: usize, to: usize| -> JRes<Polygon<f64>> {
            let mut rings = lines(from, to).into_iter().map(close_ring);
            let ext = rings.next().ok_or("a polygon has no rings")?;
            Ok(Polygon::new(ext, rings.collect()))
        };
        Ok(Some(match t {
            b"Point" => Geom::Point(Point(pts[0])),
            b"MultiPoint" => Geom::MultiPoint(MultiPoint(pts.into_iter().map(Point).collect())),
            b"LineString" => Geom::LineString(LineString(pts)),
            b"MultiLineString" => Geom::MultiLineString(MultiLineString(lines(0, ends[0].len()))),
            b"Polygon" => Geom::Polygon(polygon(0, ends[0].len())?),
            _ => {
                let mut a = 0;
                let mut v = Vec::with_capacity(ends[1].len());
                for &b in &ends[1] {
                    v.push(polygon(a, b)?);
                    a = b;
                }
                Geom::MultiPolygon(MultiPolygon(v))
            }
        }))
    }
}

/// Flattened nested coordinates, see [`JsonReader::nested`].
#[derive(Default)]
struct Nested {
    pts: Vec<Coord<f64>>,
    ends: [Vec<usize>; 3],
}
