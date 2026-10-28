//! Attribute-aware edits and cleanup: merge with attributes, exchange attributes,
//! copy-move, simplify, densify, smooth.
//!
//! Simplify, densify and smooth work on each selected feature on its own, so a boundary shared
//! with a neighbour can move on one side only. Vertices a command keeps come back bit-exact
//! (see [`VertexSnap`]), so features it leaves alone stay untouched.

use std::collections::HashMap;
use std::hash::BuildHasherDefault;

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
use crate::topo::KeyHasher;

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

/// The segments of `lines` for sweeping the pairs whose ranges along the sweep axis come
/// within `DISSOLVE_TOL_M` of each other: (line, index of the first vertex), end points, and
/// sorted ranges along the axis (by its minimum) and across it, in separate arrays so that
/// the sweep reads only what it compares.
struct Segs {
    /// How many pairs [`Segs::near_pairs`] looks at.
    visits: u64,
    ids: Vec<(usize, usize)>,
    ends: Vec<[Coord<f64>; 2]>,
    x0: Vec<f64>,
    x1: Vec<f64>,
    y0: Vec<f64>,
    y1: Vec<f64>,
}

impl Segs {
    /// Sweeps along x or y, whichever looks at fewer pairs: along x, the segments of a line
    /// running north would all be looked at against each other.
    fn new(lines: &[Vec<Coord<f64>>]) -> Segs {
        let (x, y) = (Segs::along(lines, false), Segs::along(lines, true));
        if y.visits < x.visits { y } else { x }
    }

    fn along(lines: &[Vec<Coord<f64>>], swap: bool) -> Segs {
        let f = |c: Coord<f64>| if swap { Coord { x: c.y, y: c.x } } else { c };
        let mut v: Vec<(f64, usize, usize)> = lines
            .iter()
            .enumerate()
            .flat_map(|(l, v)| (1..v.len()).map(move |i| (f(v[i - 1]).x.min(f(v[i]).x), l, i - 1)))
            .collect();
        v.sort_by(|a, b| a.0.total_cmp(&b.0));
        let n = v.len();
        let mut s = Segs {
            visits: 0,
            ids: Vec::with_capacity(n),
            ends: Vec::with_capacity(n),
            x0: Vec::with_capacity(n),
            x1: Vec::with_capacity(n),
            y0: Vec::with_capacity(n),
            y1: Vec::with_capacity(n),
        };
        for (x0, l, i) in v {
            let (a, b) = (lines[l][i], lines[l][i + 1]);
            s.ids.push((l, i));
            s.ends.push([a, b]);
            let (a, b) = (f(a), f(b));
            s.x0.push(x0);
            s.x1.push(a.x.max(b.x));
            s.y0.push(a.y.min(b.y));
            s.y1.push(a.y.max(b.y));
        }
        s.visits = s.visit_count();
        s
    }

    fn len(&self) -> usize {
        self.ids.len()
    }

    /// How many pairs [`Segs::near_pairs`] looks at (those whose ranges along the axis come
    /// within the tolerance), counted without looking at them.
    fn visit_count(&self) -> u64 {
        (0..self.len()).map(|k| (self.x0.partition_point(|&x| x <= self.x1[k] + DISSOLVE_TOL_M) - k - 1) as u64).sum()
    }

    /// The pairs of segments (by index here, `k < j`) whose bounding boxes come within the
    /// tolerance, `None` when there are more than `max`. Collected first and then looked at:
    /// with the caller's work inlined, the loop cost several times more per pair.
    fn near_pairs(&self, max: usize) -> Option<Vec<(u32, u32)>> {
        let tol = DISSOLVE_TOL_M;
        let mut out = Vec::new();
        for k in 0..self.len() {
            let (ax1, ay0, ay1) = (self.x1[k] + tol, self.y0[k] - tol, self.y1[k] + tol);
            let n = self.x0[k + 1..].iter().take_while(|&&x| x <= ax1).count();
            let (y0, y1) = (&self.y0[k + 1..k + 1 + n], &self.y1[k + 1..k + 1 + n]);
            for (j, (&b0, &b1)) in y0.iter().zip(y1).enumerate() {
                if b0 <= ay1 && ay0 <= b1 {
                    if out.len() == max {
                        return None;
                    }
                    out.push((k as u32, (k + 1 + j) as u32));
                }
            }
        }
        Some(out)
    }
}

/// Fuel of a line merge per pair of segments [`Segs::near_pairs`] looks at, per pair it finds
/// close together and per segment, in each of its two sweeps, and per line: upper bounds (by
/// a third at least) fitted to `diag-bench` runs on up to 2000 lines radiating from one point,
/// crossing in a grid, lying side by side, running north, chained end to end and overlapping
/// along one line.
const MERGE_FUEL_PER_VISIT: u64 = 150;
const MERGE_FUEL_PER_PAIR: u64 = 650;
const MERGE_FUEL_PER_SEGMENT: u64 = 8_000;
const MERGE_FUEL_PER_LINE: u64 = 16_000;

/// What a line merge may still spend; refuses, before anything is written, work that would
/// not fit in one command.
pub(crate) struct MergeFuel {
    pub left: u64,
    lines: usize,
}

impl MergeFuel {
    pub fn new(left: u64) -> MergeFuel {
        MergeFuel { left, lines: 0 }
    }

    /// The pairs of `segs` close together, when finding and looking at them fits.
    fn pairs(&mut self, segs: &Segs) -> Res<Vec<(u32, u32)>> {
        let sweep = segs.visits.saturating_mul(MERGE_FUEL_PER_VISIT) + segs.len() as u64 * MERGE_FUEL_PER_SEGMENT;
        if sweep > self.left {
            return Err(self.too_much(&format!("{} pairs of segments", budget::count(segs.visits as usize))));
        }
        self.left -= sweep;
        let max = (self.left / MERGE_FUEL_PER_PAIR) as usize;
        let pairs = segs.near_pairs(max).ok_or_else(|| {
            self.too_much(&format!("more than {} pairs of segments that cross or lie close together", budget::count(max)))
        })?;
        self.left -= pairs.len() as u64 * MERGE_FUEL_PER_PAIR;
        Ok(pairs)
    }

    fn too_much(&self, pairs: &str) -> String {
        let pairs = if pairs.is_empty() { String::new() } else { format!(" means comparing {pairs}, and") };
        format!(
            "Merging these {} lines{pairs} is more than one command can do. Select fewer lines and merge them in steps.",
            budget::count(self.lines)
        )
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
fn dissolve_overlaps(mut lines: Vec<Vec<Coord<f64>>>, fuel: &mut MergeFuel) -> Res<Vec<Vec<Coord<f64>>>> {
    let tol = DISSOLVE_TOL_M;
    // splits[l][i]: (distance along segment i of line l, vertex) to insert
    let mut splits: Vec<Vec<Vec<(f64, Coord<f64>)>>> =
        lines.iter().map(|v| vec![Vec::new(); v.len().saturating_sub(1)]).collect();
    let segs = Segs::new(&lines);
    // whether any two segments might repeat each other; without, there is nothing to drop
    let mut repeats = false;
    for (k, j) in fuel.pairs(&segs)? {
        let (k, j) = (k as usize, j as usize);
        let (((la, ia), [a, b]), ((lb, ib), [c, d])) = ((segs.ids[k], segs.ends[k]), (segs.ids[j], segs.ends[j]));
        // collinear when the shorter one lies on the longer one's line; most pairs that only
        // cross or touch are told apart by squares, without the costlier lengths (√2 for
        // ends that are the same within the tolerance on each axis)
        let sq = |x: f64| x * x;
        let (la2, lb2) = (sq(b.x - a.x) + sq(b.y - a.y), sq(d.x - c.x) + sq(d.y - c.y));
        let ((p, q), (r, t), long2) = if la2 >= lb2 { ((a, b), (c, d), la2) } else { ((c, d), (a, b), lb2) };
        let off2 = |e: Coord<f64>| sq((q.x - p.x) * (e.y - p.y) - (q.y - p.y) * (e.x - p.x));
        let max2 = 2.0 * tol * tol * long2 * (1.0 + 1e-9);
        if off2(r) > max2 || off2(t) > max2 {
            continue;
        }
        repeats = true;
        let (la_len, lb_len) = ((b.x - a.x).hypot(b.y - a.y), (d.x - c.x).hypot(d.y - c.y));
        if la_len <= tol || lb_len <= tol {
            continue;
        }
        let long_len = if la2 >= lb2 { la_len } else { lb_len };
        let ((s0, o0), (s1, o1)) = (along(p, q, r), along(p, q, t));
        if o0 > tol || o1 > tol || s0.max(s1).min(long_len) - s0.min(s1).max(0.0) <= tol {
            continue;
        }
        for (l, i, len, ends, (x, y)) in [(la, ia, la_len, (c, d), (a, b)), (lb, ib, lb_len, (a, b), (c, d))] {
            for e in [ends.0, ends.1] {
                let (s, _) = along(x, y, e);
                if s > tol && s < len - tol {
                    splits[l][i].push((s, e));
                }
            }
        }
    }
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
    if !repeats {
        return Ok(lines);
    }
    // drop a segment repeated by an earlier one (lowest line, then vertex)
    let mut dropped: Vec<Vec<bool>> = lines.iter().map(|v| vec![false; v.len().saturating_sub(1)]).collect();
    let same = |a: Coord<f64>, b: Coord<f64>| same_coord(a, b, tol);
    let segs = Segs::new(&lines);
    for (k, j) in fuel.pairs(&segs)? {
        let (k, j) = (k as usize, j as usize);
        let (first, second) = if segs.ids[k] <= segs.ids[j] { (k, j) } else { (j, k) };
        let ([a, b], [c, d]) = (segs.ends[first], segs.ends[second]);
        if (same(a, c) && same(b, d)) || (same(a, d) && same(b, c)) {
            let (l, i) = segs.ids[second];
            dropped[l][i] = true;
        }
    }
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
    Ok(out)
}

/// Dissolves metric lines where they run over each other and joins them where their ends
/// meet (see [`dissolve_overlaps`] and [`merge_lines`]), within `fuel`.
pub(crate) fn merge_metric_lines(parts: Vec<Vec<Coord<f64>>>, fuel: &mut MergeFuel) -> Res<Vec<Vec<Coord<f64>>>> {
    fuel.lines = parts.len();
    fuel.left = fuel.left.checked_sub(parts.len() as u64 * MERGE_FUEL_PER_LINE).ok_or_else(|| fuel.too_much(""))?;
    Ok(merge_lines(dissolve_overlaps(parts, fuel)?, DISSOLVE_TOL_M))
}

/// Joins lines end to end through the points where exactly two ends meet within `eps` (as
/// GEOS's line merge does), keeping the original coordinates: ends are gathered into nodes
/// once, then each chain is followed from its lowest line in both directions.
fn merge_lines(mut parts: Vec<Vec<Coord<f64>>>, eps: f64) -> Vec<Vec<Coord<f64>>> {
    // end k is the first vertex of line k / 2 for even k, the last for odd k
    let end = |parts: &[Vec<Coord<f64>>], k: usize| {
        let p = &parts[k / 2];
        if k % 2 == 0 { p[0] } else { p[p.len() - 1] }
    };
    // nodes in a grid of cells 8 eps wide: a node within eps lies in the end's own cell or,
    // near its border, in a neighbouring one
    let cell = 8.0 * eps;
    let mut heads: HashMap<(i64, i64), usize, BuildHasherDefault<KeyHasher>> = HashMap::default();
    // per node: its first end's position, the previous node in its cell, how many ends meet
    // there and the first two of them
    let mut nodes: Vec<(Coord<f64>, usize, usize, [usize; 2])> = Vec::new();
    let mut node_of = vec![0; 2 * parts.len()];
    for k in 0..2 * parts.len() {
        let c = end(&parts, k);
        let (fx, fy) = (c.x / cell, c.y / cell);
        let (cx, cy) = (fx.floor(), fy.floor());
        let near = |f: f64, i: f64| -> &[i64] {
            let r = (f - i) * cell;
            if r <= eps {
                &[0, -1]
            } else if r >= cell - eps {
                &[0, 1]
            } else {
                &[0]
            }
        };
        let mut found = usize::MAX;
        for &dx in near(fx, cx) {
            for &dy in near(fy, cy) {
                let mut n = heads.get(&(cx as i64 + dx, cy as i64 + dy)).copied().unwrap_or(usize::MAX);
                while n != usize::MAX {
                    if n < found && same_coord(nodes[n].0, c, eps) {
                        found = n;
                    }
                    n = nodes[n].1;
                }
            }
        }
        if found == usize::MAX {
            found = nodes.len();
            let prev = heads.insert((cx as i64, cy as i64), found).unwrap_or(usize::MAX);
            nodes.push((c, prev, 0, [0; 2]));
        }
        let node = &mut nodes[found];
        if node.2 < 2 {
            node.3[node.2] = k;
        }
        node.2 += 1;
        node_of[k] = found;
    }
    // the other end at the node of end `k` when exactly two meet there
    let next = |k: usize| {
        let (_, _, count, [a, b]) = nodes[node_of[k]];
        (count == 2).then_some(if a == k { b } else { a })
    };
    let mut used = vec![false; parts.len()];
    let mut out = Vec::new();
    for first in 0..parts.len() {
        if used[first] {
            continue;
        }
        used[first] = true;
        let mut line = std::mem::take(&mut parts[first]);
        // forwards from its last vertex: each next line is appended without its joining vertex
        let mut k = 2 * first + 1;
        while let Some(o) = next(k).filter(|o| !used[o / 2]) {
            used[o / 2] = true;
            let p = std::mem::take(&mut parts[o / 2]);
            if o % 2 == 0 {
                line.extend_from_slice(&p[1..]);
            } else {
                line.extend(p.iter().rev().skip(1));
            }
            k = o ^ 1;
        }
        // backwards from its first vertex: each previous line keeps its joining vertex;
        // `before` runs from the joint outwards
        let (mut before, mut skip) = (Vec::new(), 0);
        let mut k = 2 * first;
        while let Some(o) = next(k).filter(|o| !used[o / 2]) {
            used[o / 2] = true;
            let p = std::mem::take(&mut parts[o / 2]);
            if before.pop().is_none() {
                skip = 1;
            }
            if o % 2 == 0 {
                before.extend_from_slice(&p);
            } else {
                before.extend(p.iter().rev());
            }
            k = o ^ 1;
        }
        before.reverse();
        before.extend_from_slice(&line[skip..]);
        out.push(before);
    }
    out
}

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
            // everything is computed before the first write, so a refusal changes nothing
            let read = budget::edit_fuel(ed.features.len(), budget::vertices(&ed.features));
            let mut fuel = MergeFuel::new((budget::COMMAND_FUEL * 9 / 10).saturating_sub(read));
            let lines = merge_metric_lines(parts, &mut fuel)?;
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
            is_numeric_kind(&f.kind) && !host_io::is_id_field(&f.name)
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
                host_io::inherited_attributes(&f.properties).into_iter().filter(|(k, _)| layer.has_field(k)).collect();
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

/// Caps vertex estimates (also NaN, which fails the comparison) well inside a 32-bit usize.
const MAX_ESTIMATE: f64 = 1e9;

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
    vertex_factor: Option<&dyn Fn(&Geom) -> f64>,
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
        // Summed in f64: a tiny script distance gives counts far beyond usize, and a
        // saturated or wrapped integer would slip past the budget.
        let total: f64 = metric.iter().map(estimate).sum();
        let total = if total < MAX_ESTIMATE { total.ceil() as usize } else { MAX_ESTIMATE as usize };
        budget::check_write(metric.len(), total)?;
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

/// Whether `p` lies inside ring `r`; `None` when it lies on the ring.
fn ring_side(p: Coord<f64>, r: &LineString<f64>) -> Option<bool> {
    let mut inside = false;
    for w in r.0.windows(2) {
        let (a, b) = (w[0], w[1]);
        if orient(a, b, p) == 0.0
            && p.x >= a.x.min(b.x)
            && p.x <= a.x.max(b.x)
            && p.y >= a.y.min(b.y)
            && p.y <= a.y.max(b.y)
        {
            return None;
        }
        if (a.y > p.y) != (b.y > p.y) && p.x < a.x + (p.y - a.y) / (b.y - a.y) * (b.x - a.x) {
            inside = !inside;
        }
    }
    Some(inside)
}

/// Whether ring `a` lies inside ring `b`, for rings that do not cross: decided by the first
/// vertex or segment midpoint of `a` that is not on `b`, since vertices may touch.
fn ring_inside(a: &LineString<f64>, b: &LineString<f64>) -> bool {
    a.0.windows(2)
        .find_map(|w| {
            let mid = Coord { x: (w[0].x + w[1].x) / 2.0, y: (w[0].y + w[1].y) / 2.0 };
            ring_side(w[0], b).or_else(|| ring_side(mid, b))
        })
        .unwrap_or(false)
}

/// (min x, min y, max x, max y) of a ring.
fn ring_box(r: &LineString<f64>) -> [f64; 4] {
    r.0.iter().fold([f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY], |b, c| {
        [b[0].min(c.x), b[1].min(c.y), b[2].max(c.x), b[3].max(c.y)]
    })
}

fn box_within(a: &[f64; 4], b: &[f64; 4]) -> bool {
    a[0] >= b[0] && a[1] >= b[1] && a[2] <= b[2] && a[3] <= b[3]
}

/// A cheap stand-in for OGC validity, linear-ish where `geo::Validation` is quadratic and
/// too slow for a selection: no two segments of any rings cross (touching at a vertex is
/// allowed), every hole lies inside its own outer ring and outside its other holes, and an
/// outer ring lies inside another part only within one of its holes. This catches what
/// simplifying or smoothing rings independently can break. Containment is only tested
/// between rings whose boxes nest, found by a sweep like the segments'.
fn rings_simple(g: &Geom) -> bool {
    let Some(mp) = g.to_multi_polygon() else { return true };
    // (min x, max x, a, b, ring, index, segments in ring)
    let mut segs = Vec::new();
    // per polygon, the boxes of its outer ring and holes
    let mut boxes: Vec<Vec<[f64; 4]>> = Vec::with_capacity(mp.0.len());
    let mut ring_no = 0usize;
    for p in &mp.0 {
        let mut pb = Vec::with_capacity(p.interiors().len() + 1);
        for r in std::iter::once(p.exterior()).chain(p.interiors()) {
            let n = r.0.len().saturating_sub(1);
            for (i, w) in r.0.windows(2).enumerate() {
                segs.push((w[0].x.min(w[1].x), w[0].x.max(w[1].x), w[0], w[1], ring_no, i, n));
            }
            pb.push(ring_box(r));
            ring_no += 1;
        }
        boxes.push(pb);
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
    // ring k of polygon p: 0 is the outer ring, k > 0 hole k - 1
    let ring = |p: usize, k: usize| if k == 0 { mp.0[p].exterior() } else { &mp.0[p].interiors()[k - 1] };
    for (p, pb) in boxes.iter().enumerate() {
        for k in 1..pb.len() {
            if !box_within(&pb[k], &pb[0]) || !ring_inside(ring(p, k), ring(p, 0)) {
                return false;
            }
        }
    }
    // `a` lying inside `b` breaks validity when both are outer rings (unless `a` sits in one of
    // `b`'s holes) or both are holes of one polygon; other nestings follow from these.
    let bad_nesting = |(ap, ak): (usize, usize), (bp, bk): (usize, usize)| {
        let ab = &boxes[ap][ak];
        if !box_within(ab, &boxes[bp][bk]) {
            return false;
        }
        let ra = ring(ap, ak);
        match (ak, bk) {
            (0, 0) => {
                ring_inside(ra, ring(bp, 0))
                    && !(1..boxes[bp].len()).any(|h| box_within(ab, &boxes[bp][h]) && ring_inside(ra, ring(bp, h)))
            }
            (0, _) | (_, 0) => false,
            _ => ap == bp && ring_inside(ra, ring(bp, bk)),
        }
    };
    let mut order: Vec<(usize, usize)> =
        boxes.iter().enumerate().flat_map(|(p, pb)| (0..pb.len()).map(move |k| (p, k))).collect();
    order.sort_by(|a, b| boxes[a.0][a.1][0].total_cmp(&boxes[b.0][b.1][0]));
    for (i, &a) in order.iter().enumerate() {
        for &b in &order[i + 1..] {
            if boxes[b.0][b.1][0] > boxes[a.0][a.1][2] {
                break;
            }
            if bad_nesting(a, b) || bad_nesting(b, a) {
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
    let estimate = |g: &Geom| g.num_coords() as f64 + (metric_length(g) / d).ceil();
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
    let estimate = |g: &Geom| g.num_coords() as f64 * 2f64.powi(it as i32);
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
