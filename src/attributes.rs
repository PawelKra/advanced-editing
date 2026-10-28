//! Attribute-aware edits and cleanup: merge with attributes, exchange attributes,
//! copy-move, simplify, densify, smooth.
//!
//! Simplify, densify and smooth work on each selected feature on its own, so a boundary shared
//! with a neighbour can move on one side only. Vertices a command keeps come back bit-exact
//! (see [`VertexSnap`]), so features it leaves alone stay untouched.

use geo::{
    Area, ChaikinSmoothing, Coord, Densify, Euclidean, Length, LineString, MultiLineString, MultiPoint,
    MultiPolygon, Polygon, Simplify, unary_union,
};
use rustgis_plugin::{Map, Value, json};

use crate::Ctx;
use crate::geojson::{self, COORD_EPS_DEG, Dim, Geom, same_coord};
use crate::host_io::{self, Res, budget};
use crate::proj::{SNAP_TOL_M, VertexSnap};
use crate::selection::{EditSpec, active_features};

const ALL_DIMS: &[Dim] = &[Dim::Point, Dim::Line, Dim::Polygon];
const SHAPE_DIMS: &[Dim] = &[Dim::Line, Dim::Polygon];

const SHARED_BOUNDARY_NOTE: &str =
    "Each feature was edited on its own: boundaries shared with neighbours can now leave gaps or overlaps.";

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// The metric size that decides which feature's attributes a merge keeps.
fn metric_size(g: &Geom) -> f64 {
    match g {
        Geom::Polygon(p) => p.unsigned_area(),
        Geom::MultiPolygon(m) => m.unsigned_area(),
        Geom::LineString(l) => Euclidean.length(l),
        Geom::MultiLineString(m) => m.0.iter().map(|l| Euclidean.length(l)).sum(),
        Geom::Point(_) | Geom::MultiPoint(_) => 0.0,
    }
}

/// Metres within which overlapping lines count as the same line and ends as meeting.
const DISSOLVE_TOL_M: f64 = 1e-4;

/// The segments of `lines` as (line, index of the first vertex, sorted x range) by min x, for
/// sweeping pairs whose x ranges come within `DISSOLVE_TOL_M` of each other.
fn segments_by_x(lines: &[Vec<Coord<f64>>]) -> Vec<(usize, usize, f64, f64)> {
    let mut segs: Vec<(usize, usize, f64, f64)> = lines
        .iter()
        .enumerate()
        .flat_map(|(l, v)| (1..v.len()).map(move |i| (l, i - 1, v[i - 1].x.min(v[i].x), v[i - 1].x.max(v[i].x))))
        .collect();
    segs.sort_by(|a, b| a.2.total_cmp(&b.2));
    segs
}

/// Calls `f(a, b)` for every pair of segments whose bounding boxes come within the tolerance.
fn near_pairs(lines: &[Vec<Coord<f64>>], mut f: impl FnMut((usize, usize), (usize, usize))) {
    let segs = segments_by_x(lines);
    let ys = |l: usize, i: usize| {
        let (a, b) = (lines[l][i].y, lines[l][i + 1].y);
        (a.min(b), a.max(b))
    };
    for (k, &(la, ia, _, ax1)) in segs.iter().enumerate() {
        let (ay0, ay1) = ys(la, ia);
        for &(lb, ib, bx0, _) in &segs[k + 1..] {
            if bx0 > ax1 + DISSOLVE_TOL_M {
                break;
            }
            let (by0, by1) = ys(lb, ib);
            if by0 <= ay1 + DISSOLVE_TOL_M && ay0 <= by1 + DISSOLVE_TOL_M {
                f((la, ia), (lb, ib));
            }
        }
    }
}

/// Where `p` falls along `a`-`b`: (distance along it, distance off its line).
fn along(a: Coord<f64>, b: Coord<f64>, p: Coord<f64>) -> (f64, f64) {
    let (dx, dy) = (b.x - a.x, b.y - a.y);
    let len = dx.hypot(dy);
    let (px, py) = (p.x - a.x, p.y - a.y);
    ((px * dx + py * dy) / len, (dx * py - dy * px).abs() / len)
}

/// Dissolves the stretches where metric `lines` run over each other, so each is kept once:
/// each segment is split at the ends of the segments it overlaps (with their original
/// vertices), and the repeated pieces are dropped. Lines that only touch or cross are left
/// as they are.
fn dissolve_overlaps(mut lines: Vec<Vec<Coord<f64>>>) -> Vec<Vec<Coord<f64>>> {
    let tol = DISSOLVE_TOL_M;
    // splits[l][i]: (distance along segment i of line l, vertex) to insert
    let mut splits: Vec<Vec<Vec<(f64, Coord<f64>)>>> =
        lines.iter().map(|v| vec![Vec::new(); v.len().saturating_sub(1)]).collect();
    near_pairs(&lines, |(la, ia), (lb, ib)| {
        let (a, b) = (lines[la][ia], lines[la][ia + 1]);
        let (c, d) = (lines[lb][ib], lines[lb][ib + 1]);
        let (la_len, lb_len) = ((b.x - a.x).hypot(b.y - a.y), (d.x - c.x).hypot(d.y - c.y));
        if la_len <= tol || lb_len <= tol {
            return;
        }
        // collinear when the shorter one lies on the longer one's line
        let ((p, q), (r, t), long_len) = if la_len >= lb_len { ((a, b), (c, d), la_len) } else { ((c, d), (a, b), lb_len) };
        let ((s0, o0), (s1, o1)) = (along(p, q, r), along(p, q, t));
        if o0 > tol || o1 > tol || s0.max(s1).min(long_len) - s0.min(s1).max(0.0) <= tol {
            return;
        }
        for (l, i, len, ends, (x, y)) in [(la, ia, la_len, (c, d), (a, b)), (lb, ib, lb_len, (a, b), (c, d))] {
            for e in [ends.0, ends.1] {
                let (s, _) = along(x, y, e);
                if s > tol && s < len - tol {
                    splits[l][i].push((s, e));
                }
            }
        }
    });
    for (v, sp) in lines.iter_mut().zip(&mut splits) {
        if sp.iter().all(Vec::is_empty) {
            continue;
        }
        let mut out = Vec::with_capacity(v.len() + sp.iter().map(Vec::len).sum::<usize>());
        for (i, ins) in sp.iter_mut().enumerate() {
            out.push(v[i]);
            ins.sort_by(|a, b| a.0.total_cmp(&b.0));
            ins.dedup_by(|a, b| a.0 - b.0 <= tol);
            out.extend(ins.iter().map(|&(_, c)| c));
        }
        out.push(v[v.len() - 1]);
        *v = out;
    }
    // drop a segment repeated by an earlier one (lowest line, then vertex)
    let mut dropped: Vec<Vec<bool>> = lines.iter().map(|v| vec![false; v.len().saturating_sub(1)]).collect();
    let same = |a: Coord<f64>, b: Coord<f64>| same_coord(a, b, tol);
    near_pairs(&lines, |x, y| {
        let (first, second) = if x <= y { (x, y) } else { (y, x) };
        let (a, b) = (lines[first.0][first.1], lines[first.0][first.1 + 1]);
        let (c, d) = (lines[second.0][second.1], lines[second.0][second.1 + 1]);
        if (same(a, c) && same(b, d)) || (same(a, d) && same(b, c)) {
            dropped[second.0][second.1] = true;
        }
    });
    let mut out = Vec::new();
    for (v, drop) in lines.into_iter().zip(&dropped) {
        if !drop.contains(&true) {
            out.push(v);
            continue;
        }
        // the stretches between dropped segments
        let mut run: Vec<Coord<f64>> = Vec::new();
        for (i, &gone) in drop.iter().enumerate() {
            if gone {
                if !run.is_empty() {
                    out.push(std::mem::take(&mut run));
                }
            } else {
                if run.is_empty() {
                    run.push(v[i]);
                }
                run.push(v[i + 1]);
            }
        }
        if !run.is_empty() {
            out.push(run);
        }
    }
    out
}

/// Joins lines end to end where exactly two ends meet within `eps` (as GEOS's line merge
/// does), keeping the original coordinates.
fn merge_lines(mut parts: Vec<Vec<Coord<f64>>>, eps: f64) -> Vec<Vec<Coord<f64>>> {
    let ends = |p: &Vec<Coord<f64>>| (p[0], p[p.len() - 1]);
    let degree = |parts: &[Vec<Coord<f64>>], c: Coord<f64>| {
        parts
            .iter()
            .map(|p| {
                let (a, b) = ends(p);
                same_coord(a, c, eps) as usize + same_coord(b, c, eps) as usize
            })
            .sum::<usize>()
    };
    'outer: loop {
        for i in 0..parts.len() {
            for j in 0..parts.len() {
                if i == j {
                    continue;
                }
                let ((si, ei), (sj, ej)) = (ends(&parts[i]), ends(&parts[j]));
                let joined = if same_coord(ei, sj, eps) && degree(&parts, ei) == 2 {
                    Some(parts[i].iter().chain(&parts[j][1..]).copied().collect::<Vec<_>>())
                } else if same_coord(ei, ej, eps) && degree(&parts, ei) == 2 {
                    Some(parts[i].iter().chain(parts[j].iter().rev().skip(1)).copied().collect())
                } else if same_coord(si, sj, eps) && degree(&parts, si) == 2 {
                    Some(parts[j].iter().rev().chain(&parts[i][1..]).copied().collect())
                } else {
                    None
                };
                if let Some(v) = joined {
                    let (hi, lo) = (i.max(j), i.min(j));
                    parts.remove(hi);
                    parts[lo] = v;
                    continue 'outer;
                }
            }
        }
        return parts;
    }
}

/// Fields that identify a feature: never summed.
const ID_FIELDS: [&str; 3] = ["id", "fid", "ogc_fid"];

fn is_numeric_kind(kind: &str) -> bool {
    let k = kind.to_ascii_lowercase();
    ["int", "real", "double", "float", "decimal", "numeric"].iter().any(|n| k.contains(n))
}

/// The sum of a numeric field's values, `None` when all are null (nulls count as 0 next to a
/// value). An integer field's sum stays an integer unless it overflows.
fn sum_values<'a>(values: impl Iterator<Item = &'a Value>, integer: bool) -> Option<Value> {
    let (mut int, mut float, mut all_int, mut any) = (Some(0i64), 0.0f64, integer, false);
    for v in values {
        let Some(x) = v.as_f64() else { continue };
        any = true;
        float += x;
        match v.as_i64() {
            Some(i) if all_int => int = int.and_then(|s| s.checked_add(i)),
            _ => all_int = false,
        }
    }
    any.then(|| match int.filter(|_| all_int) {
        Some(i) => json!(i),
        None => json!(float),
    })
}

/// Merges the selected features of the active layer into one. There is no dialog, so the
/// attributes are those of the largest feature (area for polygons, length for lines; ties
/// and points: the lowest id). That feature keeps its id and gets the merged geometry; the
/// others are deleted. With "Sum numbers when merging" every numeric field (but an id) gets
/// the sum over the merged features instead.
pub fn merge_with_attributes(ctx: &Ctx) -> Res<Value> {
    let sum = ctx.settings.sum_numbers;
    let spec = EditSpec {
        min: 2,
        properties: sum,
        ..EditSpec::new("Select two or more features of the active layer to merge", ALL_DIMS)
    };
    let mut ed = active_features(&spec)?;
    let dim = ed.layer.dim.ok_or("the active layer has no geometry type")?;
    if let Some(f) = ed.features.iter().find(|f| f.geometry.as_ref().is_some_and(|g| g.dim() != dim)) {
        return Err(format!(
            "{}. (feature {} is not a {} like the layer '{}')",
            spec.usage,
            f.id,
            dim.noun(),
            ed.layer.name
        ));
    }
    ed.features.sort_by_key(|f| f.id);
    let frame = ed.frame()?;
    let mut snap = VertexSnap::new(frame, SNAP_TOL_M);
    let metric: Vec<Geom> = ed.features.iter().map(|f| f.geom().map(|g| snap.project(g))).collect::<Res<_>>()?;

    // Sizes within 1 ppm count as equal: the same parcel drawn at two places differs by
    // more than rounding in a projected frame.
    let mut keep = 0;
    for (i, g) in metric.iter().enumerate() {
        if metric_size(g) > metric_size(&metric[keep]) * (1.0 + 1e-6) {
            keep = i;
        }
    }

    let merged = match dim {
        Dim::Polygon => {
            let polys: Vec<Polygon<f64>> =
                metric.iter().filter_map(Geom::to_multi_polygon).flat_map(|m| m.0).collect();
            let u = unary_union(polys.iter());
            if u.0.is_empty() {
                return Err("the merged polygon is empty".into());
            }
            snap.unproject(&Geom::MultiPolygon(u))
        }
        Dim::Line => {
            let parts: Vec<Vec<Coord<f64>>> = metric
                .iter()
                .filter_map(Geom::to_multi_line)
                .flat_map(|m| m.0)
                .filter(|l| !l.0.is_empty())
                .map(|l| l.0)
                .collect();
            let lines = merge_lines(dissolve_overlaps(parts), DISSOLVE_TOL_M);
            snap.unproject(&Geom::MultiLineString(MultiLineString(lines.into_iter().map(LineString).collect())))
        }
        Dim::Point => Geom::MultiPoint(MultiPoint(
            ed.features.iter().filter_map(|f| f.geometry.as_ref()?.to_points()).flatten().collect(),
        )),
    };
    let merged = geojson::prepare_output(merged)?;

    let kept = &ed.features[keep];
    let layer = ed.layer.id;
    host_io::set_geometry(layer, kept.row, &merged)?;
    let mut summed = 0;
    if sum {
        let numeric = ed.layer.fields.iter().filter(|f| {
            is_numeric_kind(&f.kind) && !ID_FIELDS.contains(&f.name.to_ascii_lowercase().as_str())
        });
        for field in numeric {
            let total = sum_values(
                ed.features.iter().filter_map(|f| f.properties.get(&field.name)),
                field.kind.to_ascii_lowercase().contains("int"),
            );
            if let Some(total) = total
                && kept.properties.get(&field.name) != Some(&total)
            {
                host_io::set_value(layer, kept.row, &field.name, total)?;
            }
            summed += 1;
        }
    }
    let others: Vec<usize> = ed.features.iter().filter(|f| f.id != kept.id).map(|f| f.row).collect();
    host_io::delete_rows(layer, &others)?;
    let row = host_io::rows_after_delete(&[kept.row], &others)[0].ok_or("the merged feature was deleted")?;
    host_io::set_selection(&[(layer, vec![row])])?;

    let rule = match dim {
        Dim::Polygon => "the largest area",
        Dim::Line => "the longest",
        Dim::Point => "the lowest id",
    };
    let mut msg = format!(
        "Merged {} into feature {}, which keeps its attributes ({rule}).",
        plural(ed.features.len(), "feature", "features"),
        kept.id
    );
    if sum {
        msg += &format!(" Summed {}.", plural(summed, "numeric field", "numeric fields"));
    }
    ctx.finish(
        &msg,
        json!({"merged": ed.features.len(), "deleted": others.len(), "kept": kept.id, "summed": summed}),
    )
}

/// Swaps all field values of the two selected features (not their ids or geometries).
pub fn exchange_attributes(ctx: &Ctx) -> Res<Value> {
    let spec = EditSpec {
        min: 2,
        max: Some(2),
        properties: true,
        ..EditSpec::new("Select exactly two features of the active layer to swap their attributes", ALL_DIMS)
    };
    let ed = active_features(&spec)?;
    if ed.layer.fields.is_empty() {
        return Err(format!("{}. (the layer '{}' has no fields)", spec.usage, ed.layer.name));
    }
    let (a, b) = (&ed.features[0], &ed.features[1]);
    let mut swapped = 0;
    for field in &ed.layer.fields {
        let va = a.properties.get(&field.name).cloned().unwrap_or(Value::Null);
        let vb = b.properties.get(&field.name).cloned().unwrap_or(Value::Null);
        if va == vb {
            continue;
        }
        host_io::set_value(ed.layer.id, a.row, &field.name, vb)?;
        host_io::set_value(ed.layer.id, b.row, &field.name, va)?;
        swapped += 1;
    }
    host_io::set_selection(&[(ed.layer.id, vec![a.row, b.row])])?;
    let msg = format!(
        "Exchanged the attributes of features {} and {} ({} differed).",
        a.id,
        b.id,
        plural(swapped, "field", "fields")
    );
    ctx.finish(&msg, json!({"fields": swapped}))
}

/// Copies the selected features by the panel distance towards the panel bearing (clockwise
/// from north, in the metric frame), or moves them with "Move instead of copy".
pub fn copy_move(ctx: &Ctx) -> Res<Value> {
    let s = ctx.settings;
    let spec = EditSpec {
        properties: !s.move_original,
        ..EditSpec::new("Select the features to copy or move in the active layer", ALL_DIMS)
    };
    let ed = active_features(&spec)?;
    let frame = ed.frame()?;
    let b = s.bearing_deg.to_radians();
    let (dx, dy) = (s.distance_m * b.sin(), s.distance_m * b.cos());
    let shift = |c: Coord<f64>| {
        let m = frame.fwd_coord(c);
        frame.inv_coord(Coord { x: m.x + dx, y: m.y + dy })
    };
    let layer = &ed.layer;
    let mut rows = Vec::new();
    for f in &ed.features {
        let g = geojson::prepare_output(f.geom()?.map_coords(shift))?;
        if s.move_original {
            host_io::set_geometry(layer.id, f.row, &g)?;
            rows.push(f.row);
        } else {
            let attrs: Map<String, Value> =
                f.properties.iter().filter(|(k, _)| layer.has_field(k)).map(|(k, v)| (k.clone(), v.clone())).collect();
            rows.push(host_io::add_feature(layer.id, &g, &attrs)?);
        }
    }
    host_io::set_selection(&[(layer.id, rows.to_vec())])?;
    let verb = if s.move_original { "Moved" } else { "Copied" };
    let msg = format!(
        "{verb} {} by {} m towards {}°.",
        plural(rows.len(), "feature", "features"),
        s.distance_m,
        s.bearing_deg
    );
    let key = if s.move_original { "moved" } else { "copied" };
    ctx.finish(&msg, json!({ key: rows.len() }))
}

/// Applies `op` to every ring (`true`) and line part (`false`) of a metric geometry. `op`
/// returning `None` drops that ring or part: a hole is dropped, an outer ring drops its
/// polygon. `None` when nothing is left.
fn rework(g: &Geom, op: &dyn Fn(&LineString<f64>, bool) -> Option<LineString<f64>>) -> Option<Geom> {
    let poly = |p: &Polygon<f64>| {
        let ext = op(p.exterior(), true)?;
        Some(Polygon::new(ext, p.interiors().iter().filter_map(|r| op(r, true)).collect()))
    };
    let lines = |v: &[LineString<f64>]| {
        let v: Vec<_> = v.iter().filter_map(|l| op(l, false)).collect();
        (!v.is_empty()).then_some(v)
    };
    Some(match g {
        Geom::Polygon(p) => Geom::Polygon(poly(p)?),
        Geom::MultiPolygon(m) => {
            let v: Vec<_> = m.0.iter().filter_map(poly).collect();
            if v.is_empty() {
                return None;
            }
            Geom::MultiPolygon(MultiPolygon(v))
        }
        Geom::LineString(l) => Geom::LineString(lines(std::slice::from_ref(l))?.remove(0)),
        Geom::MultiLineString(m) => Geom::MultiLineString(MultiLineString(lines(&m.0)?)),
        other => other.clone(),
    })
}

fn same_geom(a: &Geom, b: &Geom) -> bool {
    if a.type_name() != b.type_name() || a.num_parts() != b.num_parts() || a.num_coords() != b.num_coords() {
        return false;
    }
    let mut ca = Vec::with_capacity(a.num_coords());
    a.for_each_coord(|c| ca.push(c));
    let mut i = 0;
    let mut same = true;
    b.for_each_coord(|c| {
        same &= same_coord(ca[i], c, COORD_EPS_DEG);
        i += 1;
    });
    same
}

/// Whether a line still has two different positions.
fn has_extent(l: &LineString<f64>) -> bool {
    l.0.first().is_some_and(|a| l.0.iter().any(|c| !same_coord(*a, *c, 0.0)))
}

/// What happened to one feature in [`generalize`].
#[derive(Default)]
struct Tally {
    changed: Vec<usize>,
    unchanged: usize,
    collapsed: Vec<i64>,
    invalid: Vec<i64>,
    before: usize,
    after: usize,
}

/// Runs `op` (see [`rework`]) on every selected line or polygon of the active layer, in
/// metres, and writes back the features it changed. A feature that would collapse or turn
/// invalid is left as it is and reported.
fn generalize(
    usage: &'static str,
    ctx: &Ctx,
    vertex_factor: Option<&dyn Fn(&Geom) -> usize>,
    check_topology: bool,
    op: &dyn Fn(&LineString<f64>, bool) -> Option<LineString<f64>>,
) -> Res<(Tally, Dim)> {
    let spec = EditSpec { whole_layer: ctx.settings.whole_layer, ..EditSpec::new(usage, SHAPE_DIMS) };
    let ed = active_features(&spec)?;
    let dim = ed.layer.dim.ok_or("the active layer has no geometry type")?;
    let frame = ed.frame()?;
    let mut snap = VertexSnap::new(frame, SNAP_TOL_M);
    let metric: Vec<Geom> = ed.features.iter().map(|f| f.geom().map(|g| snap.project(g))).collect::<Res<_>>()?;
    if let Some(estimate) = vertex_factor {
        budget::check_write(metric.len(), metric.iter().map(estimate).sum())?;
    }
    let mut t = Tally::default();
    for (f, m) in ed.features.iter().zip(&metric) {
        let orig = f.geom()?;
        t.before += orig.num_coords();
        t.after += orig.num_coords();
        let out_m = rework(m, op);
        let out = out_m.as_ref().and_then(|g| geojson::prepare_output(snap.unproject(g)).ok());
        let (Some(out_m), Some(out)) = (out_m, out) else {
            t.collapsed.push(f.id);
            continue;
        };
        if same_geom(&out, orig) {
            t.unchanged += 1;
            continue;
        }
        if check_topology && out_m.dim() == Dim::Polygon && !rings_simple(&out_m) && rings_simple(m) {
            t.invalid.push(f.id);
            continue;
        }
        t.after = t.after - orig.num_coords() + out.num_coords();
        host_io::set_geometry(ed.layer.id, f.row, &out)?;
        t.changed.push(f.row);
    }
    if !t.changed.is_empty() {
        host_io::set_selection(&[(ed.layer.id, t.changed.to_vec())])?;
    }
    Ok((t, dim))
}

fn orient(a: Coord<f64>, b: Coord<f64>, c: Coord<f64>) -> f64 {
    (b.x - a.x) * (c.y - a.y) - (b.y - a.y) * (c.x - a.x)
}

/// Segments ab and cd cross, or overlap collinearly over a positive length.
fn segments_cross(a: Coord<f64>, b: Coord<f64>, c: Coord<f64>, d: Coord<f64>) -> bool {
    let (o1, o2, o3, o4) = (orient(a, b, c), orient(a, b, d), orient(c, d, a), orient(c, d, b));
    if o1 * o2 < 0.0 && o3 * o4 < 0.0 {
        return true;
    }
    if o1 == 0.0 && o2 == 0.0 {
        let along_x = (b.x - a.x).abs() >= (b.y - a.y).abs();
        let u = |p: Coord<f64>| if along_x { p.x } else { p.y };
        let (lo1, hi1) = (u(a).min(u(b)), u(a).max(u(b)));
        let (lo2, hi2) = (u(c).min(u(d)), u(c).max(u(d)));
        return hi1.min(hi2) > lo1.max(lo2);
    }
    false
}

fn point_in_ring(p: Coord<f64>, r: &LineString<f64>) -> bool {
    let mut inside = false;
    for w in r.0.windows(2) {
        let (a, b) = (w[0], w[1]);
        if (a.y > p.y) != (b.y > p.y) && p.x < a.x + (p.y - a.y) / (b.y - a.y) * (b.x - a.x) {
            inside = !inside;
        }
    }
    inside
}

/// A cheap stand-in for OGC validity, linear-ish where `geo::Validation` is quadratic and
/// too slow for a selection: no two segments of any rings cross (touching at a vertex is
/// allowed), and every hole starts inside its outer ring. This catches what simplifying or
/// smoothing rings independently can break.
fn rings_simple(g: &Geom) -> bool {
    let Some(mp) = g.to_multi_polygon() else { return true };
    // (min x, max x, a, b, ring, index, segments in ring)
    let mut segs = Vec::new();
    let mut ring_no = 0usize;
    for p in &mp.0 {
        if p.interiors().iter().any(|h| h.0.first().is_some_and(|c| !point_in_ring(*c, p.exterior()))) {
            return false;
        }
        for r in std::iter::once(p.exterior()).chain(p.interiors()) {
            let n = r.0.len().saturating_sub(1);
            for (i, w) in r.0.windows(2).enumerate() {
                segs.push((w[0].x.min(w[1].x), w[0].x.max(w[1].x), w[0], w[1], ring_no, i, n));
            }
            ring_no += 1;
        }
    }
    segs.sort_by(|a, b| a.0.total_cmp(&b.0));
    for (i, s) in segs.iter().enumerate() {
        for t in &segs[i + 1..] {
            if t.0 > s.1 {
                break;
            }
            if s.4 == t.4 {
                let d = s.5.abs_diff(t.5);
                if d == 1 || d + 1 == s.6 {
                    continue;
                }
            }
            if segments_cross(s.2, s.3, t.2, t.3) {
                return false;
            }
        }
    }
    true
}

fn ids(v: &[i64]) -> String {
    v.iter().map(i64::to_string).collect::<Vec<_>>().join(", ")
}

fn report(ctx: &Ctx, verb: &str, setting: &str, t: &Tally, dim: Dim, note_shared: bool) -> Res<Value> {
    let mut msg = format!(
        "{verb} {} ({setting}; {} → {} vertices).",
        plural(t.changed.len(), "feature", "features"),
        t.before,
        t.after
    );
    if t.unchanged > 0 {
        msg += &format!(" {} needed no change.", plural(t.unchanged, "feature", "features"));
    }
    if !t.collapsed.is_empty() {
        msg += &format!(" Left unchanged, would collapse: {}.", ids(&t.collapsed));
    }
    if !t.invalid.is_empty() {
        msg += &format!(" Left unchanged, would become invalid: {}.", ids(&t.invalid));
    }
    if note_shared && dim == Dim::Polygon && !t.changed.is_empty() {
        msg += " ";
        msg += SHARED_BOUNDARY_NOTE;
    }
    ctx.finish(
        &msg,
        json!({
            "changed": t.changed.len(),
            "unchanged": t.unchanged,
            "collapsed": t.collapsed.len(),
            "invalid": t.invalid.len(),
            "verticesBefore": t.before,
            "verticesAfter": t.after,
        }),
    )
}

/// Simplifies the selected features with the panel tolerance (Douglas-Peucker, in metres).
/// Holes and parts that collapse are dropped; a feature that would collapse entirely is left.
pub fn simplify(ctx: &Ctx) -> Res<Value> {
    let tol = ctx.settings.tolerance_m;
    let op = |l: &LineString<f64>, ring: bool| {
        let s = l.simplify(tol);
        if ring { (s.0.len() >= 4).then_some(s) } else { has_extent(&s).then_some(s) }
    };
    let (t, dim) = generalize("Select the lines or polygons to simplify in the active layer", ctx, None, true, &op)?;
    report(ctx, "Simplified", &format!("tolerance {tol} m"), &t, dim, true)
}

/// Adds vertices so that no segment is longer than the panel distance.
pub fn densify(ctx: &Ctx) -> Res<Value> {
    let d = ctx.settings.distance_m;
    let estimate = |g: &Geom| g.num_coords() + (metric_length(g) / d).ceil() as usize;
    let op = |l: &LineString<f64>, _ring: bool| Some(Euclidean.densify(l, d));
    let (t, dim) = generalize(
        "Select the lines or polygons to densify in the active layer",
        ctx,
        Some(&estimate),
        false,
        &op,
    )?;
    report(ctx, "Densified", &format!("at most {d} m between vertices"), &t, dim, false)
}

/// Smooths the selected features by Chaikin corner cutting, the panel's iterations times.
/// Polygon rings stay closed and line ends stay in place.
pub fn smooth(ctx: &Ctx) -> Res<Value> {
    let it = ctx.settings.iterations as usize;
    let estimate = |g: &Geom| g.num_coords() << it;
    let op = |l: &LineString<f64>, _ring: bool| Some(l.chaikin_smoothing(it));
    let (t, dim) = generalize("Select the lines or polygons to smooth in the active layer", ctx, Some(&estimate), true, &op)?;
    report(ctx, "Smoothed", &plural(it, "iteration", "iterations"), &t, dim, true)
}

/// Total length of all rings and lines, in the geometry's units.
fn metric_length(g: &Geom) -> f64 {
    match g {
        Geom::LineString(l) => Euclidean.length(l),
        Geom::MultiLineString(m) => m.0.iter().map(|l| Euclidean.length(l)).sum(),
        Geom::Polygon(p) => std::iter::once(p.exterior()).chain(p.interiors()).map(|r| Euclidean.length(r)).sum(),
        Geom::MultiPolygon(m) => m
            .0
            .iter()
            .flat_map(|p| std::iter::once(p.exterior()).chain(p.interiors()))
            .map(|r| Euclidean.length(r))
            .sum(),
        _ => 0.0,
    }
}
