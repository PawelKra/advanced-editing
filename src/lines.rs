//! Line-driven editing: split with a line, reshape, offset curve, trim/extend.
//!
//! Geometry work happens in metres (see [`crate::proj`]); results go back through a
//! [`VertexSnap`], so the vertices a command does not move keep their exact coordinates.

use geo::algorithm::buffer::{BufferStyle, LineJoin as GeoJoin};
use geo::{Area, Buffer, Euclidean, Length, LineString, MultiPolygon, Polygon};
use i_overlay::core::fill_rule::FillRule;
use i_overlay::core::solver::Solver;
use i_overlay::float::overlay::OverlayOptions;
use i_overlay::float::slice::FloatSlice;
use rustgis_plugin::{Map, Value, json};

use crate::Ctx;
use crate::geojson::{
    self, Dim, Geom, P2, contours, from_lines, from_polygons, line_from_points, ring_points, shape_to_polygon,
};
use crate::host_io::{self, Feature, LAYER_READ_CAP, Res, budget};
use crate::proj::{SNAP_TOL_M, VertexSnap};
use crate::selection::{self, EditSpec, ToolSpec};
use crate::topo;

const SPLIT_USAGE: &str =
    "Select the splitting line(s) in the active layer and the polygons or lines to split in another layer";
const RESHAPE_USAGE: &str =
    "Select one reshaping line in the active layer and the polygons or lines to reshape in another layer";
const OFFSET_USAGE: &str = "Select the lines or polygons to offset in the active layer";
const TRIM_USAGE: &str =
    "Select one boundary line (or polygon) in the active layer and the lines to trim or extend in another layer";

/// Positions closer than this meet: a line snapped to a vertex or edge in lon/lat lands within
/// nanometres of it after projection.
const TOUCH_M: f64 = 1e-4;
/// Consecutive positions closer than this are one.
const DEDUP_M: f64 = 1e-7;
/// Split pieces smaller than this are precision noise (a cutter running along an edge).
const MIN_PIECE_M2: f64 = 1e-6;

fn usage_err(usage: &str, why: impl std::fmt::Display) -> String {
    format!("{usage}. ({why})")
}

/// Splits the target polygons or lines with the tool line(s).
///
/// Every target the lines cross completely is cut into pieces; the largest piece (area, or
/// length for lines) stays in the original feature together with the parts of a multi-part
/// feature no line splits, and every other piece becomes a new feature with the original's
/// attributes. Closed target lines are not cut at their start/end position. The lines are
/// first snapped to the targets (see [`topo`]): a line drawn along an edge splits nothing
/// there, and one ending on the boundary splits.
pub fn split_with_line(ctx: &Ctx) -> Res<Value> {
    let mut spec = ToolSpec::new(SPLIT_USAGE, &[Dim::Line], &[Dim::Polygon, Dim::Line]);
    spec.target_properties = true;
    spec.whole_layer = ctx.settings.whole_layer;
    let sel = selection::gather(&spec)?;
    let frame = sel.frame()?;
    let mut pl = topo::Placer::new(frame);
    // targets first: their vertices and edges win over the tools'
    let mut targets = Vec::new();
    for set in &sel.targets {
        for f in &set.features {
            targets.push((set.layer.id, f, pl.add_target(f.geom()?)));
        }
    }
    let mut cutters = Vec::new();
    for t in &sel.tools {
        cutters.extend(paths_of(&pl.add_tool(t.geom()?)));
    }
    let metric: Vec<&Geom> = targets.iter().map(|t| &t.2).collect();
    pl.snap_paths(&metric, &mut cutters);
    let mut new_vertices = Vec::new();
    // the split features (layer, feature, number of pieces) and their pieces in lon/lat, the
    // one that stays in the feature first
    let (mut done, mut results) = (Vec::new(), Vec::new());
    for (owner, (layer, f, g)) in targets.iter().enumerate() {
        let cut = pl.cutters_for(owner, &cutters, TOUCH_M);
        let pieces = match g.dim() {
            Dim::Polygon => split_polygons(g, &cut)?,
            Dim::Line => split_lines(g, &cut),
            Dim::Point => None,
        };
        let Some(pieces) = pieces else { continue };
        done.push((*layer, *f, pieces.len()));
        for p in &pieces {
            results.push((owner, geojson::prepare_output(pl.unproject(p, &mut new_vertices))?));
        }
    }
    topo::share_within(frame, results.iter_mut().map(|(o, g)| (*o, g)), &new_vertices);
    let mut results = results.into_iter().map(|(_, g)| g);
    let (mut writes, mut adds) = (Vec::new(), Vec::new());
    for &(layer, f, n) in &done {
        writes.extend(results.next().map(|g| (layer, f, g)));
        adds.extend(results.by_ref().take(n - 1).map(|g| (layer, f, g)));
    }
    let (split, created) = (writes.len(), adds.len());
    if split == 0 {
        return Err(usage_err(
            SPLIT_USAGE,
            format!("the line does not cross any of the {} target feature(s) from side to side", sel.target_count()),
        ));
    }
    let mut exclude: Vec<(i64, i64)> = writes.iter().map(|(l, f, _)| (*l, f.id)).collect();
    exclude.extend(sel.tools.iter().map(|t| (sel.active.id, t.id)));
    let scope = topo::Scope {
        layers: &sel.layers,
        edited: sel.targets.iter().map(|t| t.layer.id).collect(),
        exclude,
        spent: budget::edit_fuel(
            sel.target_count(),
            budget::vertices(&sel.tools) + sel.targets.iter().map(|t| budget::vertices(&t.features)).sum::<usize>(),
        ),
        read_whole: sel.targets_from_active.then_some(sel.active.id),
    };
    let shared = topo::plan(ctx.settings.topological, &scope, &new_vertices)?;
    // the placer has already unprojected; Edits only keeps the rows for the final selection
    let mut ed = Edits::new(VertexSnap::new(frame, SNAP_TOL_M));
    for (layer, f, g) in &writes {
        host_io::set_geometry(*layer, f.row, g)?;
        ed.changed.push((*layer, f.row));
    }
    for (layer, f, g) in &adds {
        ed.changed.push((*layer, host_io::add_feature(*layer, g, &host_io::inherited_attributes(&f.properties))?));
    }
    let note = shared.note();
    let (sv, sf) = (shared.vertices(), shared.features());
    shared.apply()?;
    ed.finish(tools_to_delete(&sel, ctx.settings.keep_tools))?;
    let msg = topo::with_note(
        &ctx.tr(
            "Split {split} feature(s) into {pieces} pieces",
            &[("split", &split.to_string()), ("pieces", &(split + created).to_string())],
        ),
        &note,
    );
    ctx.finish(&msg, json!({"split": split, "created": created, "sharedVertices": sv, "sharedFeatures": sf}))
}

/// Reshapes the target polygon or line along the tool line.
///
/// Polygons: the line must cross exactly one ring of the feature (its outline, a hole, or
/// the outline of one part) at least twice; the ring between the first and last crossing is
/// replaced by the line. Of the two rings this can give, the larger one is kept, as QGIS
/// does: a line outside the polygon adds area, a line inside cuts the smaller side off (for a
/// hole: outside the hole enlarges it, inside it shrinks it). Holes left outside a reshaped
/// outline, or swallowed by a reshaped hole, are dropped. A line that runs inside to the
/// opposite side and meets the ring there (along an edge or at a point) would cut the
/// polygon in two: it is refused, as that is Split With Line's job.
///
/// Lines: the stretch between the first and last crossing is replaced by the line; a line
/// that starts or ends at the target's start or end point extends the target.
pub fn reshape(ctx: &Ctx) -> Res<Value> {
    let mut spec = ToolSpec::new(RESHAPE_USAGE, &[Dim::Line], &[Dim::Polygon, Dim::Line]);
    spec.max_tools = Some(1);
    spec.whole_layer = ctx.settings.whole_layer;
    let sel = selection::gather(&spec)?;
    let mut ed = Edits::new(VertexSnap::new(sel.frame()?, SNAP_TOL_M));
    let tool_paths = paths_of(&ed.snap.project(sel.tools[0].geom()?));
    let [tool] = tool_paths.as_slice() else {
        return Err(usage_err(RESHAPE_USAGE, "the reshaping line must be a single line with at least two positions"));
    };
    let mut reshaped = 0usize;
    let mut reasons: Vec<String> = Vec::new();
    for set in &sel.targets {
        for f in &set.features {
            let g = ed.snap.project(f.geom()?);
            let r = match &g {
                Geom::Polygon(_) | Geom::MultiPolygon(_) => reshape_polygons(&g, tool),
                Geom::LineString(_) | Geom::MultiLineString(_) => reshape_lines(&g, tool),
                _ => Err("not a line or polygon".into()),
            };
            match r {
                Ok(new) => {
                    ed.write(set.layer.id, f.row, &new)?;
                    reshaped += 1;
                }
                Err(why) => reasons.push(format!("feature {}: {why}", f.id)),
            }
        }
    }
    if reshaped == 0 {
        reasons.truncate(3);
        return Err(usage_err(RESHAPE_USAGE, reasons.join("; ")));
    }
    ed.finish(tools_to_delete(&sel, ctx.settings.keep_tools))?;
    let msg = ctx.tr("Reshaped {n} feature(s)", &[("n", &reshaped.to_string())]);
    ctx.finish(&msg, json!({"reshaped": reshaped, "skipped": reasons.len()}))
}

/// Adds an offset copy of the selected lines (or polygons) at the panel distance, or moves
/// the originals there with "Move instead of copy".
///
/// Lines get a one-sided offset curve, to the right of their direction or to the left with
/// "Offset to the left"; polygons are buffered outwards, or inwards with "Offset to the
/// left". The copies keep the original's attributes.
pub fn offset(ctx: &Ctx) -> Res<Value> {
    let in_place = ctx.settings.move_original;
    let mut spec = EditSpec::new(OFFSET_USAGE, &[Dim::Line, Dim::Polygon]);
    spec.properties = !in_place;
    let ed_in = selection::active_features(&spec)?;
    let d = ctx.settings.distance_m;
    let left = ctx.settings.offset_left;
    let frame = ed_in.frame()?;
    // everything is computed before the first write, so running out of budget changes nothing
    let read = budget::edit_fuel(ed_in.features.len(), budget::vertices(&ed_in.features));
    let mut fuel = OffsetFuel { left: (budget::COMMAND_FUEL * 9 / 10).saturating_sub(read) };
    let mut outs = Vec::with_capacity(ed_in.features.len());
    for f in &ed_in.features {
        let out = offset_geom(&frame.project(f.geom()?), d, left, &mut fuel).map_err(|e| usage_err(OFFSET_USAGE, e))?;
        outs.push(out);
    }
    let (n_out, v_out) = outs.iter().flatten().fold((0, 0), |(n, v), g| (n + 1, v + g.num_coords()));
    if budget::write_fuel(n_out, v_out) > fuel.left {
        return Err(usage_err(
            OFFSET_USAGE,
            format!(
                "the offsets have {} vertices, more than one command can write after computing them; \
                 select fewer features and repeat the command for the rest",
                budget::count(v_out)
            ),
        ));
    }
    let mut ed = Edits::new(VertexSnap::new(frame, SNAP_TOL_M));
    let mut skipped = Vec::new();
    for (f, out) in ed_in.features.iter().zip(outs) {
        match out {
            Some(o) if in_place => ed.write(ed_in.layer.id, f.row, &o)?,
            Some(o) => ed.add(ed_in.layer.id, &o, &host_io::inherited_attributes(&f.properties))?,
            None => skipped.push(f.id),
        }
    }
    let created = ed.changed.len();
    if created == 0 {
        return Err(usage_err(
            OFFSET_USAGE,
            format!("nothing is left at {d} m: the distance is larger than the selected features allow"),
        ));
    }
    ed.finish(None)?;
    let side = if left { "left" } else { "right" };
    let text = if in_place { "Offset {n} feature(s) by {d} m ({side})" } else { "Added {n} offset copies at {d} m ({side})" };
    let msg = ctx.tr(text, &[("n", &created.to_string()), ("d", &d.to_string()), ("side", side)]);
    let key = if in_place { "moved" } else { "created" };
    ctx.finish(&msg, json!({ key: created, "skipped": skipped.len() }))
}

/// What Offset Curve may still spend in one command, in fuel: lines spend it as they are
/// offset (see [`offset_curve`]), polygons by an estimate before they are buffered.
pub struct OffsetFuel {
    pub left: u64,
}

impl OffsetFuel {
    fn spend(&mut self, fuel: u64) -> bool {
        let ok = fuel <= self.left;
        self.left = self.left.saturating_sub(fuel);
        ok
    }
}

/// Fuel of [`offset_curve`] per raw offset segment, per candidate segment looked at for a
/// crossing, per candidate looked at for a distance, per piece of the raw offset kept or
/// dropped, per candidate looked at while trimming an end and per vertex looked at while
/// choosing the chords of [`offset_chords`]: upper bounds (by a quarter at least) fitted to
/// `diag-bench` runs on wavy, zigzag, spiral, circular, hairpin, randomly turning and jittery
/// lines and rings of 300 to 3000 vertices, offset by 1 to 40 m.
const OFFSET_FUEL_PER_RAW_SEGMENT: u64 = 11_000;
const OFFSET_FUEL_PER_CROSSING_STEP: u64 = 300;
const OFFSET_FUEL_PER_STEP: u64 = 800;
const OFFSET_FUEL_PER_PIECE: u64 = 12_000;
const OFFSET_FUEL_PER_TRIM_STEP: u64 = 200;
const OFFSET_FUEL_PER_CHECK: u64 = 500;

/// Fuel to buffer a polygon by `d`: an upper bound fitted to `diag-bench` runs on smooth,
/// rectangular and comb-shaped rings of 250 to 2000 vertices, inwards and outwards by 1 to
/// 20 m. It grows faster than the vertices, the more so the more of them lie within `d` of
/// each other (inwards along a curve every shifted edge overlaps that many others).
fn buffer_fuel(p: &Polygon<f64>, d: f64) -> u64 {
    let rings = std::iter::once(p.exterior()).chain(p.interiors());
    let (v, len) = rings.fold((0usize, 0.0), |(v, len), r| (v + r.0.len(), len + Euclidean.length(r)));
    let near = if len > 0.0 { (2.0 * d.abs() * v as f64 / len).min(1e6) } else { 0.0 };
    let v = v as f64;
    (v * (80_000.0 + 200.0 * v) + 200.0 * v * near * near).min(u64::MAX as f64 / 4.0) as u64
}

/// The offset of a metric line (one-sided, to the left with `left`) or polygon (inwards
/// with `left`) by `d`; `None` when nothing is left. Refuses when `fuel` runs out.
pub fn offset_geom(g: &Geom, d: f64, left: bool, fuel: &mut OffsetFuel) -> Res<Option<Geom>> {
    let too_much = |what: &str| {
        format!(
            "offsetting these {what} by {d} m is more than one command can do; select fewer features, \
             simplify them or use a smaller distance"
        )
    };
    Ok(match g {
        Geom::LineString(_) | Geom::MultiLineString(_) => {
            let signed = if left { d } else { -d };
            let mut parts = Vec::new();
            for part in g.to_multi_line().map(|m| m.0).unwrap_or_default() {
                let curve = offset_curve(&ring_points(&part), signed, fuel).ok_or_else(|| too_much("lines"))?;
                parts.extend(curve.iter().map(|r| line_from_points(r)));
            }
            (!parts.is_empty()).then(|| from_lines(parts))
        }
        Geom::Polygon(_) | Geom::MultiPolygon(_) => {
            let mp = g.to_multi_polygon().unwrap_or(MultiPolygon(vec![]));
            if !mp.0.iter().all(|p| fuel.spend(buffer_fuel(p, d))) {
                return Err(too_much("polygons"));
            }
            let b = buffer_polygons(&mp, if left { -d } else { d });
            let parts: Vec<Polygon<f64>> = b.0.into_iter().filter(|p| p.unsigned_area() > MIN_PIECE_M2).collect();
            (!parts.is_empty()).then(|| from_polygons(parts))
        }
        _ => None,
    })
}

/// Trims or extends the end of the selected line nearest to the tool line, to meet it.
///
/// For each target, the end nearest to the boundary is moved: when the target crosses the
/// boundary, that end is trimmed back to the crossing nearest to it; otherwise it is extended
/// along its last segment until it meets the boundary (the other end is tried when that ray
/// misses), however far that is, as in QGIS. Lines already touching the boundary are left
/// alone. With nothing selected in other layers the targets are the active layer's lines
/// that cross the boundary, or point at it with their nearest end within the panel Distance
/// of it: there the Distance is a search radius, not a limit on the extension.
pub fn trim_extend(ctx: &Ctx) -> Res<Value> {
    let mut spec = ToolSpec::new(TRIM_USAGE, &[Dim::Line, Dim::Polygon], &[Dim::Line]);
    spec.max_tools = Some(1);
    spec.need_targets = false;
    spec.whole_layer = ctx.settings.whole_layer;
    let sel = selection::gather(&spec)?;
    let mut targets: Vec<(i64, Feature)> =
        sel.targets.iter().flat_map(|t| t.features.iter().map(|f| (t.layer.id, f.clone()))).collect();
    let frame_geoms: Vec<Geom>;
    if sel.targets_from_active {
        if sel.active.dim != Some(Dim::Line) {
            return Err(usage_err(TRIM_USAGE, "no lines are selected in another layer"));
        }
        // Lines to extend do not touch the boundary yet, so the selection model's
        // "features touching the tool" is not enough here: the targets are the lines that
        // cross it and those pointing at it from an end within the search radius.
        let radius = ctx.settings.distance_m;
        let tool_lonlat = sel.tools[0].geom()?.clone();
        let reach = host_io::all_features_capped(&sel.active, LAYER_READ_CAP, false)?;
        let frame = crate::proj::Frame::for_geoms(std::iter::once(&tool_lonlat))?;
        let tool_m = paths_of(&frame.project(&tool_lonlat));
        targets.clear();
        for f in reach {
            if f.id == sel.tools[0].id {
                continue;
            }
            let Some(g) = f.geometry.as_ref() else { continue };
            let lines = paths_of(&frame.project(g));
            let lines: Vec<&Vec<P2>> = lines.iter().filter(|l| l.len() >= 2).collect();
            let crosses = lines.iter().any(|l| !hits_any(l, &tool_m).is_empty());
            let end_dist = lines
                .iter()
                .flat_map(|l| [l[0], l[l.len() - 1]])
                .map(|q| dist_to_paths(q, &tool_m))
                .fold(f64::INFINITY, f64::min);
            let points_at = || {
                lines.iter().any(|l| {
                    let n = l.len();
                    ray_hit(l[1], l[0], &tool_m).is_some() || ray_hit(l[n - 2], l[n - 1], &tool_m).is_some()
                })
            };
            if crosses || (end_dist <= radius && points_at()) {
                targets.push((sel.active.id, f));
            }
        }
        if targets.is_empty() {
            return Err(format!(
                "No line of the active layer crosses the boundary or ends within {radius} m of it. \
                 Select the lines to trim or extend in another layer, or raise Distance."
            ));
        }
        frame_geoms = targets.iter().filter_map(|(_, f)| f.geometry.clone()).chain([tool_lonlat]).collect();
    } else {
        frame_geoms = sel.geoms().cloned().collect();
    }
    if targets.is_empty() {
        return Err(usage_err(
            TRIM_USAGE,
            format!("no line of '{}' crosses the boundary or points at it", sel.active.name),
        ));
    }
    let mut ed = Edits::new(VertexSnap::new(crate::proj::Frame::for_geoms(frame_geoms.iter())?, SNAP_TOL_M));
    let tool = paths_of(&ed.snap.project(sel.tools[0].geom()?));
    let (mut trimmed, mut extended) = (0usize, 0usize);
    let mut reasons = Vec::new();
    for (layer, f) in &targets {
        let g = ed.snap.project(f.geom()?);
        match trim_extend_geom(&g, &tool) {
            Ok((new, was_trim)) => {
                ed.write(*layer, f.row, &new)?;
                if was_trim {
                    trimmed += 1;
                } else {
                    extended += 1;
                }
            }
            Err(why) => reasons.push(format!("feature {}: {why}", f.id)),
        }
    }
    if trimmed + extended == 0 {
        reasons.truncate(3);
        return Err(usage_err(TRIM_USAGE, reasons.join("; ")));
    }
    ed.finish(None)?;
    let msg = ctx.tr(
        "Trimmed {t} and extended {e} line(s)",
        &[("t", &trimmed.to_string()), ("e", &extended.to_string())],
    );
    ctx.finish(&msg, json!({"trimmed": trimmed, "extended": extended, "skipped": reasons.len()}))
}

// ---------- writing ----------

/// The edits of one command, and the rows they touched (for the final selection).
struct Edits {
    snap: VertexSnap,
    changed: Vec<(i64, usize)>,
}

impl Edits {
    fn new(snap: VertexSnap) -> Edits {
        Edits { snap, changed: Vec::new() }
    }

    fn lonlat(&self, metric: &Geom) -> Res<Geom> {
        geojson::prepare_output(self.snap.unproject(metric))
    }

    fn write(&mut self, layer: i64, row: usize, metric: &Geom) -> Res<()> {
        host_io::set_geometry(layer, row, &self.lonlat(metric)?)?;
        self.changed.push((layer, row));
        Ok(())
    }

    fn add(&mut self, layer: i64, metric: &Geom, attributes: &Map<String, Value>) -> Res<()> {
        let row = host_io::add_feature(layer, &self.lonlat(metric)?, attributes)?;
        self.changed.push((layer, row));
        Ok(())
    }

    /// Deletes the tool features (if asked) and selects the changed and new features.
    fn finish(self, delete: Option<(i64, Vec<usize>)>) -> Res<()> {
        let mut changed = self.changed;
        if let Some((layer, rows)) = delete {
            host_io::delete_rows(layer, &rows)?;
            let (mine, rest): (Vec<_>, Vec<_>) = changed.into_iter().partition(|(l, _)| *l == layer);
            let mine_rows: Vec<usize> = mine.iter().map(|(_, r)| *r).collect();
            changed = rest;
            changed.extend(host_io::rows_after_delete(&mine_rows, &rows).into_iter().flatten().map(|r| (layer, r)));
        }
        let mut by_layer: Vec<(i64, Vec<usize>)> = Vec::new();
        for (l, r) in changed {
            match by_layer.iter_mut().find(|(x, _)| *x == l) {
                Some((_, v)) => v.push(r),
                None => by_layer.push((l, vec![r])),
            }
        }
        host_io::set_selection(&by_layer)
    }
}

fn tools_to_delete(sel: &selection::Selection, keep: bool) -> Option<(i64, Vec<usize>)> {
    (!keep).then(|| (sel.active.id, sel.tools.iter().map(|t| t.row).collect()))
}

// ---------- small geometry helpers (metres) ----------

fn dist(a: P2, b: P2) -> f64 {
    (b[0] - a[0]).hypot(b[1] - a[1])
}

fn lerp(a: P2, b: P2, t: f64) -> P2 {
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t]
}

fn dedup(v: &mut Vec<P2>, eps: f64) {
    v.dedup_by(|b, a| dist(*a, *b) <= eps);
}

fn path_len(v: &[P2]) -> f64 {
    v.windows(2).map(|w| dist(w[0], w[1])).sum()
}

/// Shoelace area, positive for counter-clockwise; works for closed and open rings.
fn signed_area(r: &[P2]) -> f64 {
    let n = r.len();
    if n < 3 {
        return 0.0;
    }
    0.5 * (0..n).map(|i| {
        let (a, b) = (r[i], r[(i + 1) % n]);
        a[0] * b[1] - b[0] * a[1]
    }).sum::<f64>()
}

fn point_in_ring(q: P2, r: &[P2]) -> bool {
    let mut inside = false;
    let n = r.len();
    let mut j = n - 1;
    for i in 0..n {
        let (a, b) = (r[i], r[j]);
        if (a[1] > q[1]) != (b[1] > q[1]) && q[0] < (b[0] - a[0]) * (q[1] - a[1]) / (b[1] - a[1]) + a[0] {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// Distance from `q` to segment ab, and the parameter of the nearest point.
fn seg_dist(q: P2, a: P2, b: P2) -> (f64, f64) {
    let d = [b[0] - a[0], b[1] - a[1]];
    let l2 = d[0] * d[0] + d[1] * d[1];
    let t = if l2 > 0.0 { (((q[0] - a[0]) * d[0] + (q[1] - a[1]) * d[1]) / l2).clamp(0.0, 1.0) } else { 0.0 };
    (dist(q, lerp(a, b, t)), t)
}

fn dist_to_paths(q: P2, paths: &[Vec<P2>]) -> f64 {
    paths.iter().flat_map(|p| p.windows(2)).map(|w| seg_dist(q, w[0], w[1]).0).fold(f64::INFINITY, f64::min)
}

/// The parts of a line geometry, or the rings of a polygon, as deduplicated paths of at
/// least two positions.
fn paths_of(g: &Geom) -> Vec<Vec<P2>> {
    let raw: Vec<Vec<P2>> = match g {
        Geom::LineString(l) => vec![ring_points(l)],
        Geom::MultiLineString(m) => m.0.iter().map(ring_points).collect(),
        Geom::Polygon(p) => std::iter::once(p.exterior()).chain(p.interiors()).map(ring_points).collect(),
        Geom::MultiPolygon(m) => {
            m.0.iter().flat_map(|p| std::iter::once(p.exterior()).chain(p.interiors())).map(ring_points).collect()
        }
        _ => vec![],
    };
    raw.into_iter()
        .map(|mut v| {
            dedup(&mut v, DEDUP_M);
            v
        })
        .filter(|v| v.len() >= 2)
        .collect()
}

/// Intersection of segments ab and cd as (t on ab, u on cd); `None` when parallel.
fn seg_inter(a: P2, b: P2, c: P2, d: P2) -> Option<(f64, f64)> {
    let r = [b[0] - a[0], b[1] - a[1]];
    let s = [d[0] - c[0], d[1] - c[1]];
    let den = r[0] * s[1] - r[1] * s[0];
    if den.abs() <= 1e-12 * (r[0].hypot(r[1]) * s[0].hypot(s[1])) {
        return None;
    }
    let q = [c[0] - a[0], c[1] - a[1]];
    let t = (q[0] * s[1] - q[1] * s[0]) / den;
    let u = (q[0] * r[1] - q[1] * r[0]) / den;
    Some((t, u))
}

/// A crossing of path `a` (segment `i`, parameter `t`) with path `b` (segment `j`, `u`).
#[derive(Clone, Copy, Debug)]
struct Hit {
    i: usize,
    t: f64,
    j: usize,
    u: f64,
    /// The crossing on `b` (exactly on its segment).
    pb: P2,
}

impl Hit {
    fn pos_a(&self) -> f64 {
        self.i as f64 + self.t
    }
    fn pos_b(&self) -> f64 {
        self.j as f64 + self.u
    }
}

/// Parameter range of segment `k` of an `n`-segment open path: the ends reach `TOUCH_M`
/// further, so lines snapped onto an edge or vertex still meet it.
fn seg_range(k: usize, n: usize, len: f64) -> (f64, f64) {
    let e = if len > 0.0 { TOUCH_M / len } else { 0.0 };
    (if k == 0 { -e } else { 0.0 }, if k + 1 == n { 1.0 + e } else { 1.0 })
}

/// Crossings and touches of `a` with `b`, sorted along `a`, one per position.
fn hits(a: &[P2], b: &[P2]) -> Vec<Hit> {
    let (na, nb) = (a.len() - 1, b.len() - 1);
    let b_closed = nb >= 3 && dist(b[0], b[nb]) <= DEDUP_M;
    let mut out = Vec::new();
    for i in 0..na {
        let (a0, a1) = (a[i], a[i + 1]);
        let (lo_a, hi_a) = seg_range(i, na, dist(a0, a1));
        let (ax0, ax1) = (a0[0].min(a1[0]) - TOUCH_M, a0[0].max(a1[0]) + TOUCH_M);
        let (ay0, ay1) = (a0[1].min(a1[1]) - TOUCH_M, a0[1].max(a1[1]) + TOUCH_M);
        for j in 0..nb {
            let (b0, b1) = (b[j], b[j + 1]);
            if b0[0].max(b1[0]) < ax0 || b0[0].min(b1[0]) > ax1 || b0[1].max(b1[1]) < ay0 || b0[1].min(b1[1]) > ay1 {
                continue;
            }
            let (lo_b, hi_b) = if b_closed { (0.0, 1.0) } else { seg_range(j, nb, dist(b0, b1)) };
            if let Some((t, u)) = seg_inter(a0, a1, b0, b1)
                && t >= lo_a
                && (t < hi_a || (i + 1 == na && t <= hi_a))
                && u >= lo_b
                && u <= hi_b
            {
                out.push(Hit { i, t, j, u: u.clamp(0.0, 1.0), pb: lerp(b0, b1, u.clamp(0.0, 1.0)) });
            }
        }
    }
    out.sort_by(|x, y| x.pos_a().total_cmp(&y.pos_a()));
    out.dedup_by(|y, x| dist(x.pb, y.pb) <= DEDUP_M);
    out
}

fn hits_any(a: &[P2], others: &[Vec<P2>]) -> Vec<Hit> {
    let mut v: Vec<Hit> = others.iter().flat_map(|b| hits(a, b)).collect();
    v.sort_by(|x, y| x.pos_a().total_cmp(&y.pos_a()));
    v
}

// ---------- split ----------

/// Split pieces below this are dropped as precision noise, as Cut and Clip drop theirs.
const SPLIT_SLIVER_M2: f64 = crate::polygons::AREA_EPS_M2;

/// Splits one polygon by the cutter paths with i_overlay's `slice`. Collinear vertices are
/// kept, so the vertices shared with neighbours survive.
pub fn split_polygon(p: &Polygon<f64>, cutters: &[Vec<P2>]) -> Vec<Polygon<f64>> {
    use geo::BoundingRect;
    let Some(bb) = p.bounding_rect() else { return vec![] };
    let near: Vec<Vec<P2>> = cutters
        .iter()
        .filter(|c| {
            let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
            for q in c.iter() {
                (x0, y0, x1, y1) = (x0.min(q[0]), y0.min(q[1]), x1.max(q[0]), y1.max(q[1]));
            }
            x1 >= bb.min().x && x0 <= bb.max().x && y1 >= bb.min().y && y0 <= bb.max().y
        })
        .cloned()
        .collect();
    if near.is_empty() {
        return vec![p.clone()];
    }
    let opts = OverlayOptions { preserve_input_collinear: true, preserve_output_collinear: true, ..Default::default() };
    let shapes = contours(p).slice_custom_by(&near, FillRule::EvenOdd, opts, Solver::default());
    shapes.iter().map(|s| shape_to_polygon(s)).filter(|q| q.unsigned_area() >= SPLIT_SLIVER_M2).collect()
}

/// The pieces of a (multi)polygon cut by `cutters`, the one that stays in the feature first;
/// `None` when no part is cut.
fn split_polygons(g: &Geom, cutters: &[Vec<P2>]) -> Res<Option<Vec<Geom>>> {
    let parts = g.to_multi_polygon().map(|m| m.0).unwrap_or_default();
    let mut kept = Vec::new();
    let mut pieces = Vec::new();
    for p in parts {
        let s = split_polygon(&p, cutters);
        if s.len() >= 2 {
            let (a0, a1) = (p.unsigned_area(), s.iter().map(|q| q.unsigned_area()).sum::<f64>());
            if (a0 - a1).abs() > 1e-6 * a0 + 1e-4 {
                return Err(format!("splitting changed the area from {a0:.6} m² to {a1:.6} m²; nothing was split"));
            }
            pieces.extend(s);
        } else {
            kept.push(p);
        }
    }
    if pieces.is_empty() {
        return Ok(None);
    }
    pieces.sort_by(|a, b| b.unsigned_area().total_cmp(&a.unsigned_area()));
    let mut it = pieces.into_iter();
    let mut first = vec![it.next().unwrap()];
    first.extend(kept);
    Ok(Some(std::iter::once(from_polygons(first)).chain(it.map(Geom::Polygon)).collect()))
}

/// Splits a line at its crossings with the cutters. A closed line is not cut at its start
/// position: the first and last piece are joined.
pub fn split_line(line: &[P2], cutters: &[Vec<P2>]) -> Vec<Vec<P2>> {
    let hits = hits_any(line, cutters);
    let mut out = Vec::new();
    let mut cur = vec![line[0]];
    let mut h = 0;
    for i in 0..line.len() - 1 {
        while h < hits.len() && hits[h].i == i {
            let p = lerp(line[i], line[i + 1], hits[h].t.clamp(0.0, 1.0));
            cur.push(p);
            out.push(std::mem::replace(&mut cur, vec![p]));
            h += 1;
        }
        cur.push(line[i + 1]);
    }
    out.push(cur);
    for l in out.iter_mut() {
        dedup(l, DEDUP_M);
    }
    out.retain(|l| l.len() >= 2 && path_len(l) > TOUCH_M);
    let closed = line.len() >= 4 && dist(line[0], line[line.len() - 1]) <= DEDUP_M;
    let cut_at_start = hits.iter().any(|h| dist(lerp(line[h.i], line[h.i + 1], h.t), line[0]) <= TOUCH_M);
    if closed && !cut_at_start && out.len() >= 2 {
        let last = out.pop().unwrap();
        let mut joined = last;
        joined.extend_from_slice(&out[0][1..]);
        out[0] = joined;
    }
    out
}

fn split_lines(g: &Geom, cutters: &[Vec<P2>]) -> Option<Vec<Geom>> {
    let parts = g.to_multi_line()?.0;
    let mut kept = Vec::new();
    let mut pieces: Vec<Vec<P2>> = Vec::new();
    for l in parts {
        let pts = ring_points(&l);
        let s = if pts.len() >= 2 { split_line(&pts, cutters) } else { vec![] };
        if s.len() >= 2 {
            pieces.extend(s);
        } else {
            kept.push(l);
        }
    }
    if pieces.is_empty() {
        return None;
    }
    pieces.sort_by(|a, b| path_len(b).total_cmp(&path_len(a)));
    let mut it = pieces.into_iter();
    let mut first = vec![line_from_points(&it.next().unwrap())];
    first.extend(kept);
    Some(std::iter::once(from_lines(first)).chain(it.map(|p| Geom::LineString(line_from_points(&p)))).collect())
}

// ---------- reshape ----------

/// Ring positions strictly after parameter `from` up to `to`, walking forward around a
/// closed ring of `n` segments (`ring[n] == ring[0]`).
fn ring_arc(ring: &[P2], from: f64, to: f64) -> Vec<P2> {
    let n = ring.len() - 1;
    let mut to = to;
    if to <= from {
        to += n as f64;
    }
    let mut v = Vec::new();
    let mut k = from.floor() as usize + 1;
    while (k as f64) < to {
        v.push(ring[k % n]);
        k += 1;
    }
    v
}

/// Forward distance from `from` to `x` around a ring of `n` segments.
fn ahead(from: f64, x: f64, n: usize) -> f64 {
    (x - from).rem_euclid(n as f64)
}

fn segments_cross(a: &[P2]) -> bool {
    let n = a.len().saturating_sub(1);
    for i in 0..n {
        for j in i + 2..n {
            if let Some((t, u)) = seg_inter(a[i], a[i + 1], a[j], a[j + 1])
                && (0.0..=1.0).contains(&t)
                && (0.0..=1.0).contains(&u)
            {
                return true;
            }
        }
    }
    false
}

/// Whether `tool`, between its first and last crossing `hs` of a closed ring, meets the ring
/// again only by touching it from inside on one of the two arcs between those crossings
/// (`on_ba` tells the arc of a ring position): along an edge, at a point, or at a vertex of the
/// line within `TOUCH_M` of the ring, which `hs` may miss. Keeping that arc would pinch the
/// ring into two pieces.
fn cuts_in_two(ring: &[P2], tool: &[P2], hs: &[Hit], on_ba: impl Fn(f64) -> bool) -> bool {
    let (h0, h1) = (hs[0], hs[hs.len() - 1]);
    let nearest = |q: P2| {
        let mut best = (f64::INFINITY, 0.0);
        for (j, e) in ring.windows(2).enumerate() {
            let (d, t) = seg_dist(q, e[0], e[1]);
            if d < best.0 {
                best = (d, j as f64 + t);
            }
        }
        best
    };
    // (position along the tool, position along the ring)
    let mut meets: Vec<(f64, f64)> = hs[1..hs.len() - 1].iter().map(|h| (h.pos_a(), h.pos_b())).collect();
    for k in h0.i + 1..=h1.i {
        let q = tool[k];
        if dist(q, h0.pb) <= TOUCH_M || dist(q, h1.pb) <= TOUCH_M || hs.iter().any(|h| dist(q, h.pb) <= TOUCH_M) {
            continue;
        }
        let (d, pos) = nearest(q);
        if d <= TOUCH_M {
            meets.push((k as f64, pos));
        }
    }
    if meets.is_empty() {
        return false;
    }
    meets.sort_by(|x, y| x.0.total_cmp(&y.0));
    let at = |pos: f64| {
        let i = (pos.floor() as usize).min(tool.len() - 2);
        lerp(tool[i], tool[i + 1], pos - i as f64)
    };
    let along: Vec<f64> =
        std::iter::once(h0.pos_a()).chain(meets.iter().map(|m| m.0)).chain(std::iter::once(h1.pos_a())).collect();
    // -1 outside, 0 along the ring, 1 inside
    let stretch: Vec<i8> = along
        .windows(2)
        .map(|w| {
            let q = at(0.5 * (w[0] + w[1]));
            if nearest(q).0 <= TOUCH_M {
                0
            } else if point_in_ring(q, ring) {
                1
            } else {
                -1
            }
        })
        .collect();
    let touch: Vec<bool> = stretch.windows(2).map(|w| w[0] >= 0 && w[1] >= 0 && w[0].max(w[1]) > 0).collect();
    [true, false].into_iter().any(|arc| {
        let mut on = meets.iter().zip(&touch).filter(|(m, _)| on_ba(m.1) == arc).peekable();
        on.peek().is_some() && on.all(|(_, &t)| t)
    })
}

/// Replaces the stretch of a closed ring between the first and last crossing of `tool`.
/// Refuses a line that would cut the ring in two (`hole`: the ring is a hole).
fn reshape_ring(ring: &[P2], tool: &[P2], hs: &[Hit], hole: bool) -> Res<Vec<P2>> {
    let n = ring.len() - 1;
    let (h0, h1) = (hs[0], hs[hs.len() - 1]);
    let (sa, sb) = (h0.pos_b(), h1.pos_b());
    if dist(h0.pb, h1.pb) <= TOUCH_M {
        return Err("the line crosses the boundary twice at the same place".into());
    }
    let mut mid = vec![h0.pb];
    mid.extend_from_slice(&tool[h0.i + 1..=h1.i.max(h0.i)]);
    mid.push(h1.pb);
    dedup(&mut mid, DEDUP_M);
    if segments_cross(&mid) {
        return Err("the reshaping line crosses itself".into());
    }
    // A candidate keeps one arc of the ring; it is only simple if the line does not cross that
    // arc between its first and last crossing.
    let inner = &hs[1..hs.len() - 1];
    let on_ba_at = |pos: f64| {
        let x = ahead(sb, pos, n);
        x > 0.0 && x < ahead(sb, sa, n)
    };
    let on_ba = |h: &Hit| on_ba_at(h.pos_b());
    // keeping the other side instead is not what the line asks for
    if cuts_in_two(ring, tool, hs, on_ba_at) {
        return Err(if hole {
            "This line would cut the hole in two; reshape one side of it at a time".into()
        } else {
            "This line would cut the polygon in two; use Split With Line instead".into()
        });
    }
    let mut cands = Vec::new();
    if !inner.iter().any(on_ba) {
        let mut c = mid.clone();
        c.extend(ring_arc(ring, sb, sa));
        c.push(h0.pb);
        cands.push(c);
    }
    if !inner.iter().any(|h| !on_ba(h)) {
        let mut c: Vec<P2> = mid.iter().rev().copied().collect();
        c.extend(ring_arc(ring, sa, sb));
        c.push(h1.pb);
        cands.push(c);
    }
    for c in cands.iter_mut() {
        dedup(c, DEDUP_M);
    }
    let orig_sign = signed_area(ring).signum();
    let mut best = cands
        .into_iter()
        .filter(|c| c.len() >= 4)
        .max_by(|a, b| signed_area(a).abs().total_cmp(&signed_area(b).abs()))
        .ok_or("the line crosses the boundary back and forth, which would make the outline cross itself")?;
    if signed_area(&best).signum() != orig_sign {
        best.reverse();
    }
    Ok(best)
}

fn reshape_polygons(g: &Geom, tool: &[P2]) -> Res<Geom> {
    let mut parts: Vec<Vec<Vec<P2>>> = g
        .to_multi_polygon()
        .unwrap_or(MultiPolygon(vec![]))
        .0
        .iter()
        .map(|p| std::iter::once(p.exterior()).chain(p.interiors()).map(ring_points).collect())
        .collect();
    let mut crossed = Vec::new();
    for (pi, rings) in parts.iter().enumerate() {
        for (ri, r) in rings.iter().enumerate() {
            if r.len() < 4 {
                continue;
            }
            let hs = hits(tool, r);
            if !hs.is_empty() {
                crossed.push((pi, ri, hs));
            }
        }
    }
    let (pi, ri, hs) = match crossed.len() {
        0 => return Err("the line does not cross the boundary".into()),
        1 => crossed.pop().unwrap(),
        _ => return Err("the line crosses more than one ring (outline or hole); reshape one ring at a time".into()),
    };
    if hs.len() < 2 {
        return Err("the line must cross the boundary at least twice".into());
    }
    let new = reshape_ring(&parts[pi][ri], tool, &hs, ri > 0)?;
    if ri == 0 {
        for (pj, other) in parts.iter().enumerate() {
            if pj != pi && point_in_ring(other[0][0], &new) {
                return Err("the reshaped outline would cover another part of the feature".into());
            }
        }
        let holes: Vec<Vec<P2>> = parts[pi].drain(1..).filter(|h| point_in_ring(h[0], &new)).collect();
        parts[pi] = std::iter::once(new).chain(holes).collect();
    } else {
        let rings = std::mem::take(&mut parts[pi]);
        let mut out = Vec::new();
        for (k, r) in rings.into_iter().enumerate() {
            if k == ri {
                out.push(new.clone());
            } else if k == 0 || !point_in_ring(r[0], &new) {
                out.push(r);
            }
        }
        parts[pi] = out;
    }
    let polys = parts
        .into_iter()
        .map(|rings| {
            let mut it = rings.into_iter().map(|r| line_from_points(&r));
            let ext = it.next().unwrap_or(LineString(vec![]));
            Polygon::new(ext, it.collect())
        })
        .collect();
    Ok(from_polygons(polys))
}

/// Reshapes one line part; `None` when the tool neither crosses it twice nor continues one of
/// its ends.
fn reshape_line(line: &[P2], tool: &[P2]) -> Option<Vec<P2>> {
    let hs = hits(tool, line);
    let n = line.len();
    let (first, last) = (line[0], line[n - 1]);
    let (t0, t1) = (tool[0], tool[tool.len() - 1]);
    let mut out: Vec<P2>;
    if hs.len() >= 2 {
        let (h0, h1) = (hs[0], hs[hs.len() - 1]);
        let mut mid = vec![lerp(line[h0.j], line[h0.j + 1], h0.u)];
        mid.extend_from_slice(&tool[h0.i + 1..=h1.i.max(h0.i)]);
        mid.push(lerp(line[h1.j], line[h1.j + 1], h1.u));
        let (lo, hi) = if h0.pos_b() <= h1.pos_b() { (h0, h1) } else { (h1, h0) };
        if h0.pos_b() > h1.pos_b() {
            mid.reverse();
        }
        out = line[..=lo.j].to_vec();
        out.extend(mid);
        out.extend_from_slice(&line[hi.j + 1..]);
    } else if dist(t0, last) <= TOUCH_M {
        out = line.to_vec();
        out.extend_from_slice(&tool[1..]);
    } else if dist(t1, last) <= TOUCH_M {
        out = line.to_vec();
        out.extend(tool[..tool.len() - 1].iter().rev());
    } else if dist(t0, first) <= TOUCH_M {
        out = tool.iter().rev().copied().collect();
        out.extend_from_slice(&line[1..]);
    } else if dist(t1, first) <= TOUCH_M {
        out = tool.to_vec();
        out.extend_from_slice(&line[1..]);
    } else {
        return None;
    }
    dedup(&mut out, DEDUP_M);
    (out.len() >= 2).then_some(out)
}

fn reshape_lines(g: &Geom, tool: &[P2]) -> Res<Geom> {
    let mut parts: Vec<Vec<P2>> = g.to_multi_line().map(|m| m.0.iter().map(ring_points).collect()).unwrap_or_default();
    let mut done: Vec<(usize, Vec<P2>)> =
        parts.iter().enumerate().filter_map(|(k, p)| Some((k, reshape_line(p, tool)?))).collect();
    match done.len() {
        0 => Err("the line must cross the target twice, or start or end at one of its ends".into()),
        1 => {
            let (k, new) = done.pop().unwrap();
            parts[k] = new;
            Ok(from_lines(parts.iter().map(|p| line_from_points(p)).collect()))
        }
        _ => Err("the line reshapes more than one part; reshape one part at a time".into()),
    }
}

// ---------- offset ----------

/// Distance without `hypot`, which takes thousands of instructions in WebAssembly; offset
/// coordinates are local and far from overflowing.
fn odist(a: P2, b: P2) -> f64 {
    let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
    (dx * dx + dy * dy).sqrt()
}

fn opath_len(v: &[P2]) -> f64 {
    v.windows(2).map(|w| odist(w[0], w[1])).sum()
}

/// [`seg_inter`] without `hypot`.
fn crossing_params(a: P2, b: P2, c: P2, d: P2) -> Option<(f64, f64)> {
    let r = [b[0] - a[0], b[1] - a[1]];
    let s = [d[0] - c[0], d[1] - c[1]];
    let den = r[0] * s[1] - r[1] * s[0];
    if den * den <= 1e-24 * ((r[0] * r[0] + r[1] * r[1]) * (s[0] * s[0] + s[1] * s[1])) {
        return None;
    }
    let q = [c[0] - a[0], c[1] - a[1]];
    Some(((q[0] * s[1] - q[1] * s[0]) / den, (q[0] * r[1] - q[1] * r[0]) / den))
}

/// Angle step of round joins (radians): chords stay within a(1 - cos 0.05) < 0.13 % of the
/// offset distance.
const ARC_STEP: f64 = 0.1;

type BBox = [f64; 4];

fn seg_bbox(s: &[P2; 2]) -> BBox {
    [s[0][0].min(s[1][0]), s[0][1].min(s[1][1]), s[0][0].max(s[1][0]), s[0][1].max(s[1][1])]
}

/// Segments by grid cell, in flat arrays (hashing is slow in WebAssembly). A segment is
/// registered in the cells of sample points at most `cell / 2` apart, so each of its points
/// lies within `cell / 4` of a registered sample.
struct SegGrid {
    cell: f64,
    origin: P2,
    nx: i64,
    ny: i64,
    /// Items of cell `c` are `items[start[c]..start[c + 1]]`.
    start: Vec<u32>,
    items: Vec<u32>,
}

impl SegGrid {
    fn new(segs: &[[P2; 2]], cell: f64) -> SegGrid {
        let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
        for q in segs.iter().flatten() {
            (x0, y0, x1, y1) = (x0.min(q[0]), y0.min(q[1]), x1.max(q[0]), y1.max(q[1]));
        }
        let mut g = SegGrid { cell, origin: [x0, y0], nx: 1, ny: 1, start: Vec::new(), items: Vec::new() };
        // a coarser grid only costs more candidates per query
        let cap = 4.0 * segs.len() as f64 + 4096.0;
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
            mine.extend(g.samples(s).map(|q| {
                let (cx, cy) = g.key(q);
                (cy.clamp(0, g.ny - 1) * g.nx + cx.clamp(0, g.nx - 1)) as u32
            }));
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

    fn samples(&self, s: &[P2; 2]) -> impl Iterator<Item = P2> {
        let steps = ((odist(s[0], s[1]) / (0.5 * self.cell)).ceil() as usize).max(1);
        let (a, b) = (s[0], s[1]);
        (0..=steps).map(move |k| lerp(a, b, k as f64 / steps as f64))
    }

    /// Calls `f` with the segments registered in the cells overlapping the box (some several
    /// times) until it returns false; false when stopped.
    fn in_box(&self, b: BBox, f: &mut impl FnMut(usize) -> bool) -> bool {
        let (x0, y0) = self.key([b[0], b[1]]);
        let (x1, y1) = self.key([b[2], b[3]]);
        for cy in y0.max(0)..=y1.min(self.ny - 1) {
            for cx in x0.max(0)..=x1.min(self.nx - 1) {
                let c = (cy * self.nx + cx) as usize;
                for &k in &self.items[self.start[c] as usize..self.start[c + 1] as usize] {
                    if !f(k as usize) {
                        return false;
                    }
                }
            }
        }
        true
    }

    /// Calls `f` with every segment that may cross segment `s` (some several times).
    fn crossing(&self, s: &[P2; 2], mut f: impl FnMut(usize)) {
        let q = 0.25 * self.cell;
        let mut g = |k| {
            f(k);
            true
        };
        if odist(s[0], s[1]) <= 2.0 * self.cell {
            let b = seg_bbox(s);
            self.in_box([b[0] - q, b[1] - q, b[2] + q, b[3] + q], &mut g);
        } else {
            // a crossing lies within cell/4 of a sample of each segment
            for p in self.samples(s) {
                self.in_box([p[0] - 2.0 * q, p[1] - 2.0 * q, p[0] + 2.0 * q, p[1] + 2.0 * q], &mut g);
            }
        }
    }
}

fn cross(o: P2, a: P2, b: P2) -> f64 {
    (a[0] - o[0]) * (b[1] - o[1]) - (a[1] - o[1]) * (b[0] - o[0])
}

/// What a segment of a raw offset is.
#[derive(Clone, Copy, PartialEq)]
enum Raw {
    Shifted,
    Arc,
    /// A chord across an inner corner: never part of the offset, but it keeps the raw
    /// offset one curve.
    Inner,
}

/// The raw offset of a path at distance `a` on side `s` (+1 left, -1 right): every segment
/// shifted, outer corners joined by arcs, inner corners cut where the shifted segments meet
/// or, when they are too short to meet, joined by chords; the loops this makes are removed
/// afterwards. Returns the positions, and per segment its kind and the path segment
/// it comes from. A closed path gives a closed curve.
fn raw_offset(path: &[P2], units: &[P2], closed: bool, a: f64, s: f64) -> (Vec<P2>, Vec<Raw>, Vec<usize>) {
    let nseg = path.len() - 1;
    let off = |k: usize| [-units[k][1] * s * a, units[k][0] * s * a];
    let add = |p: P2, o: P2| [p[0] + o[0], p[1] + o[1]];
    let mut r = Vec::with_capacity(2 * path.len());
    let mut kind = Vec::with_capacity(2 * path.len());
    let mut src = Vec::with_capacity(2 * path.len());
    r.push(add(path[0], off(0)));
    for k in 0..nseg {
        let o = off(k);
        r.push(add(path[k + 1], o));
        kind.push(Raw::Shifted);
        src.push(k);
        if k + 1 == nseg && !closed {
            break;
        }
        let next = (k + 1) % nseg;
        let v = path[k + 1];
        let (uk, un) = (units[k], units[next]);
        let turn = uk[0] * un[1] - uk[1] * un[0];
        let dot = uk[0] * un[0] + uk[1] * un[1];
        let a_next = add(v, off(next));
        if s * turn < 0.0 || (turn.abs() <= 1e-9 && dot < 0.0) {
            // outer corner: arc around the vertex, clockwise for a left offset
            let angle = turn.abs().atan2(dot);
            let steps = (angle / ARC_STEP).ceil() as usize;
            if steps > 1 {
                let step = -s * angle / steps as f64;
                let (sn, cs) = step.sin_cos();
                let mut w = o;
                for _ in 1..steps {
                    w = [w[0] * cs - w[1] * sn, w[0] * sn + w[1] * cs];
                    r.push(add(v, w));
                    kind.push(Raw::Arc);
                    src.push(k);
                }
            }
            r.push(a_next);
            kind.push(Raw::Arc);
            src.push(k);
        } else if turn.abs() > 1e-12 {
            // inner corner: both shifted segments end where they cross, a tan(angle / 2) back
            let h = a * turn.abs() / (1.0 + dot);
            let n = r.len();
            let avail = odist(r[n - 2], r[n - 1]);
            let next_len = if closed && next == 0 { odist(r[0], r[1]) } else { odist(path[next], path[next + 1]) };
            if h <= avail && h <= next_len {
                let x = [r[n - 1][0] - uk[0] * h, r[n - 1][1] - uk[1] * h];
                r[n - 1] = x;
                if closed && next == 0 {
                    r[0] = x;
                }
            } else {
                r.push(a_next);
                kind.push(Raw::Inner);
                src.push(k);
            }
        }
    }
    if closed {
        // the closing join ends where the curve started
        let first = r[0];
        *r.last_mut().unwrap() = first;
    }
    (r, kind, src)
}

/// The band of points within `a` of a path, for telling offset points from the rest; an open
/// path's band is round at its ends, as the distance to the path is.
struct Band<'p> {
    path: &'p [P2],
    units: Vec<P2>,
    bbs: Vec<BBox>,
    closed: bool,
    a: f64,
    tol: f64,
    want: f64,
    grid: SegGrid,
    stamp: Vec<u32>,
    query: u32,
    cand: Vec<(usize, f64, f64)>,
    /// Candidate segments looked at, for the fuel.
    visits: u64,
}

impl Band<'_> {
    /// Distance from `q` to path segment `j` and where along it.
    fn seg(&self, q: P2, j: usize) -> (f64, f64) {
        let (p0, p1) = (self.path[j], self.path[j + 1]);
        let u = self.units[j];
        let t = (((q[0] - p0[0]) * u[0] + (q[1] - p0[1]) * u[1]) / odist(p0, p1)).clamp(0.0, 1.0);
        (odist(q, lerp(p0, p1, t)), t)
    }

    /// Whether some part of the path is nearer to `q` than `r`.
    fn nearer(&mut self, q: P2, r: f64) -> bool {
        let m = r + 0.25 * self.grid.cell;
        let mut visits = 0;
        let mut near = false;
        self.grid.in_box([q[0] - m, q[1] - m, q[0] + m, q[1] + m], &mut |j| {
            visits += 1;
            near = self.seg(q, j).0 < r;
            !near
        });
        self.visits += visits;
        near
    }

    /// Trims the end of an open curve, cut where the raw offset crosses a chord of a round
    /// join (up to `a (1 - cos(ARC_STEP / 2))` inside the circle), back to where it is `a`
    /// from the path.
    fn trim_end(&mut self, pts: &mut Vec<P2>) {
        let r = self.a * (1.0 - 1e-7) - 2.0 * self.tol;
        if pts.len() < 2 || !self.nearer(pts[pts.len() - 1], r) {
            return;
        }
        let mut spans: Vec<(f64, f64)> = Vec::new();
        while pts.len() >= 2 {
            let (p, q) = (pts[pts.len() - 2], pts[pts.len() - 1]);
            let b = seg_bbox(&[p, q]);
            let m = self.a + 0.25 * self.grid.cell;
            let mut visits = 0;
            spans.clear();
            self.grid.in_box([b[0] - m, b[1] - m, b[2] + m, b[3] + m], &mut |j| {
                visits += 1;
                let e = self.bbs[j];
                if e[0] - b[2] < r && b[0] - e[2] < r && e[1] - b[3] < r && b[1] - e[3] < r {
                    spans.extend(capsule_span(p, q, self.path[j], self.path[j + 1], r));
                }
                true
            });
            self.visits += visits;
            // the stretch nearer than `a` that reaches the end
            spans.sort_by(|x, y| y.1.total_cmp(&x.1));
            let mut from = 1.0;
            for &(t0, t1) in &spans {
                if t1 >= from - 1e-12 {
                    from = from.min(t0);
                }
            }
            if from >= 1.0 {
                return;
            }
            if from > 0.0 {
                *pts.last_mut().unwrap() = lerp(p, q, from);
                return;
            }
            pts.pop();
        }
    }

    /// Whether `q`, an offset of path segment `k`, is a point of the offset curve: no part of
    /// the path nearer than `thr`, and `q` on the wanted side of some nearest part. Near a
    /// shared vertex the side is the outer side of the turn there.
    fn holds(&mut self, q: P2, k: usize, thr: f64) -> bool {
        let nseg = self.path.len() - 1;
        // the neighbours of the source segment are the likeliest to be too close
        for j in k.saturating_sub(2)..(k + 3).min(nseg) {
            if self.seg(q, j).0 < thr {
                return false;
            }
        }
        self.query += 1;
        let (query, reach) = (self.query, self.a + self.tol);
        let mut cand = std::mem::take(&mut self.cand);
        cand.clear();
        let mut best = f64::INFINITY;
        let m = reach + 0.25 * self.grid.cell;
        let mut stamp = std::mem::take(&mut self.stamp);
        let mut visits = 0;
        let all = self.grid.in_box([q[0] - m, q[1] - m, q[0] + m, q[1] + m], &mut |j| {
            visits += 1;
            if stamp[j] == query {
                return true;
            }
            stamp[j] = query;
            let b = self.bbs[j];
            if b[0] - q[0] > reach || q[0] - b[2] > reach || b[1] - q[1] > reach || q[1] - b[3] > reach {
                return true;
            }
            let (d, t) = self.seg(q, j);
            if d < thr {
                return false;
            }
            best = best.min(d);
            cand.push((j, d, t));
            true
        });
        self.stamp = stamp;
        self.visits += visits;
        let ok = all && best <= reach && self.side_ok(q, &cand, best, nseg);
        self.cand = cand;
        ok
    }

    fn side_ok(&self, q: P2, cand: &[(usize, f64, f64)], best: f64, nseg: usize) -> bool {
        let (path, closed, units, want, tol) = (self.path, self.closed, &self.units, self.want, self.tol);
        cand.iter().filter(|c| c.1 <= best + tol).any(|&(j, _, t)| {
            let vertex = if t == 0.0 && (j > 0 || closed) {
                Some((if j > 0 { j - 1 } else { nseg - 1 }, j))
            } else if t == 1.0 && (j + 1 < nseg || closed) {
                Some((j, (j + 1) % nseg))
            } else {
                None
            };
            let side = match vertex {
                Some((i, o)) => {
                    let turn = units[i][0] * units[o][1] - units[i][1] * units[o][0];
                    if turn.abs() <= 1e-9 { cross(path[j], path[j + 1], q) } else { -turn }
                }
                None => cross(path[j], path[j + 1], q),
            };
            side * want > 0.0
        })
    }
}

/// Most vertices one chord of [`offset_chords`] replaces: the search for the longest chord
/// costs the square of it.
const CHORD_SPAN: usize = 32;
/// How much further from the line than the distance the offset of a chord may lie, relative
/// to the distance (0.8 mm at 20 m).
const CHORD_EXCESS: f64 = 4e-5;
/// How far behind a chord the vertices it replaces may lie, relative to the distance.
const CHORD_DEPTH: f64 = 1e-2;

/// The vertices of `path` the offset on side `s` (+1 left, -1 right) at distance `a` is built
/// from, and per chord between them how far behind it the vertices it replaces lie. A vertex
/// is replaced only when it lies behind the chord (away from the offset), within its length,
/// and the offset of the chord stays within `CHORD_EXCESS` of `a` from the line: on its side
/// the offset is then no nearer to the replaced vertices than to the chord, and what comes
/// near them from behind is cut off by [`clip_to_line`]. A densely digitised line (GPS
/// tracks, scanned boundaries) loses its jitter there, which otherwise makes the raw offset
/// loop at every vertex when `a` is far longer than the segments. `None`: out of fuel.
fn offset_chords(path: &[P2], s: f64, a: f64, tol: f64, fuel: &mut OffsetFuel) -> Option<(Vec<usize>, Vec<f64>)> {
    let n = path.len();
    let (excess, deep) = (CHORD_EXCESS * a, CHORD_DEPTH * a);
    // Along the chord the line is no further from the chord's offset than an anchor (a vertex
    // at x, h behind the chord) makes it: h + (x' - x)² / 2a beyond `a` at x'.
    let far = |(x, h): (f64, f64), at: f64| h + (at - x) * (at - x) / (2.0 * a);
    let (mut anchors, mut right): (Vec<(f64, f64)>, Vec<usize>) = (Vec::new(), Vec::new());
    // the depth behind the chord i -> k of the vertices between, if it may replace them
    let mut chord = |i: usize, k: usize| -> Option<f64> {
        let (p0, p1) = (path[i], path[k]);
        let l = odist(p0, p1);
        if l <= DEDUP_M {
            return None;
        }
        let u = [(p1[0] - p0[0]) / l, (p1[1] - p0[1]) / l];
        let front = [-u[1] * s, u[0] * s];
        anchors.clear();
        anchors.push((0.0, 0.0));
        let mut depth = 0.0_f64;
        for q in &path[i + 1..k] {
            let r = [q[0] - p0[0], q[1] - p0[1]];
            let (x, y) = (r[0] * u[0] + r[1] * u[1], r[0] * front[0] + r[1] * front[1]);
            if y > tol || -y > deep || x < anchors[anchors.len() - 1].0 - tol || x > l + tol {
                return None;
            }
            depth = depth.max(-y);
            anchors.push((x, (-y).max(0.0)));
        }
        anchors.push((l, 0.0));
        // Between two neighbouring anchors, the nearest one so far on each side bounds it
        // (of two such parabolas the left one only falls behind further right).
        let m = anchors.len();
        right.clear();
        right.resize(m, m - 1);
        for g in (1..m - 1).rev() {
            let at = anchors[g - 1].0;
            right[g] = if far(anchors[g], at) <= far(anchors[right[g + 1]], at) { g } else { right[g + 1] };
        }
        let mut lb = 0;
        for g in 0..m - 1 {
            let at = anchors[g + 1].0;
            if far(anchors[g], at) <= far(anchors[lb], at) {
                lb = g;
            }
            let (pl, pr) = (anchors[lb], anchors[right[g + 1]]);
            let dx = pr.0 - pl.0;
            let x = if dx > 0.0 { a * (pr.1 - pl.1) / dx + 0.5 * (pl.0 + pr.0) } else { anchors[g].0 };
            let x = x.clamp(anchors[g].0, at.max(anchors[g].0));
            if far(pl, x).min(far(pr, x)) > excess {
                return None;
            }
        }
        Some(depth)
    };
    let (mut keep, mut depth) = (vec![0], Vec::new());
    let mut checks = 0u64;
    let mut i = 0;
    while i + 1 < n {
        let (mut next, mut dep) = (i + 1, 0.0);
        // a jittery line alternates: the chord to the next vertex may fail where the one
        // after succeeds
        let mut misses = 0;
        for k in i + 2..n.min(i + 2 + CHORD_SPAN) {
            checks += (k - i) as u64;
            match chord(i, k) {
                Some(h) => (next, dep, misses) = (k, h, 0),
                None if misses == 0 => misses = 1,
                None => break,
            }
        }
        keep.push(next);
        depth.push(dep);
        i = next;
    }
    if !fuel.spend(checks * OFFSET_FUEL_PER_CHECK) {
        return None;
    }
    // a ring keeps at least a triangle
    if keep.len() < 4 && odist(path[0], path[n - 1]) <= DEDUP_M {
        return Some(((0..n).collect(), vec![0.0; n - 1]));
    }
    Some((keep, depth))
}

/// Parameters along pq of the points nearer than `r` to segment e0e1 (an interval, as the
/// points within `r` of a segment form a convex shape), within 0..1.
fn capsule_span(p: P2, q: P2, e0: P2, e1: P2, r: f64) -> Option<(f64, f64)> {
    let w = [q[0] - p[0], q[1] - p[1]];
    let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
    let ww = w[0] * w[0] + w[1] * w[1];
    for c in [e0, e1] {
        let pc = [p[0] - c[0], p[1] - c[1]];
        let b = w[0] * pc[0] + w[1] * pc[1];
        let cc = pc[0] * pc[0] + pc[1] * pc[1] - r * r;
        if ww <= 0.0 {
            if cc < 0.0 {
                (lo, hi) = (0.0, 1.0);
            }
            continue;
        }
        let disc = b * b - ww * cc;
        if disc > 0.0 {
            let sq = disc.sqrt();
            (lo, hi) = (lo.min((-b - sq) / ww), hi.max((-b + sq) / ww));
        }
    }
    let le = odist(e0, e1);
    if le > 0.0 {
        let u = [(e1[0] - e0[0]) / le, (e1[1] - e0[1]) / le];
        let pe = [p[0] - e0[0], p[1] - e0[1]];
        // t where v0 + t dv lies within lo..hi
        let range = |v0: f64, dv: f64, lo: f64, hi: f64| {
            if dv == 0.0 {
                if (lo..=hi).contains(&v0) { (f64::NEG_INFINITY, f64::INFINITY) } else { (1.0, 0.0) }
            } else {
                let (t0, t1) = ((lo - v0) / dv, (hi - v0) / dv);
                (t0.min(t1), t0.max(t1))
            }
        };
        let along = range(pe[0] * u[0] + pe[1] * u[1], w[0] * u[0] + w[1] * u[1], 0.0, le);
        let across = range(pe[1] * u[0] - pe[0] * u[1], w[1] * u[0] - w[0] * u[1], -r, r);
        let (t0, t1) = (along.0.max(across.0), along.1.min(across.1));
        if t1 > t0 {
            (lo, hi) = (lo.min(t0), hi.max(t1));
        }
    }
    let (lo, hi) = (lo.max(0.0), hi.min(1.0));
    (hi > lo).then_some((lo, hi))
}

/// Squared distance from `q` to segment ab.
fn seg_dist2(q: P2, a: P2, b: P2) -> f64 {
    let d = [b[0] - a[0], b[1] - a[1]];
    let l2 = d[0] * d[0] + d[1] * d[1];
    let t = if l2 > 0.0 { (((q[0] - a[0]) * d[0] + (q[1] - a[1]) * d[1]) / l2).clamp(0.0, 1.0) } else { 0.0 };
    let e = [a[0] + d[0] * t - q[0], a[1] + d[1] * t - q[1]];
    e[0] * e[0] + e[1] * e[1]
}

fn seg_seg_dist(p: P2, q: P2, a: P2, b: P2) -> f64 {
    let (o1, o2, o3, o4) = (cross(p, q, a), cross(p, q, b), cross(a, b, p), cross(a, b, q));
    if (o1 > 0.0) != (o2 > 0.0) && (o3 > 0.0) != (o4 > 0.0) {
        return 0.0;
    }
    seg_dist2(p, a, b).min(seg_dist2(q, a, b)).min(seg_dist2(a, p, q)).min(seg_dist2(b, p, q)).sqrt()
}

/// Cuts out of the offset `curves` of the chords `keep` of `orig` whatever lies nearer than
/// `a` to the vertices the chords replaced. That happens only behind a chord (where the line
/// comes back near itself, or beyond a sharp corner), so it is looked for only near those.
fn clip_to_line(
    curves: Vec<Vec<P2>>,
    orig: &[P2],
    keep: &[usize],
    depth: &[f64],
    a: f64,
    tol: f64,
    fuel: &mut OffsetFuel,
) -> Option<Vec<Vec<P2>>> {
    let deep: Vec<usize> = (0..depth.len()).filter(|&j| depth[j] > tol && keep[j + 1] > keep[j] + 1).collect();
    if deep.is_empty() || curves.is_empty() {
        return Some(curves);
    }
    let segs: Vec<[P2; 2]> = curves.iter().flat_map(|c| c.windows(2).map(|w| [w[0], w[1]])).collect();
    if segs.is_empty() {
        return Some(curves);
    }
    let total: f64 = segs.iter().map(|s| odist(s[0], s[1])).sum();
    let grid = SegGrid::new(&segs, (2.0 * total / segs.len() as f64).max(0.5 * a).max(1e-6));
    let r = a - tol;
    let mut cut: Vec<Vec<(f64, f64)>> = vec![Vec::new(); segs.len()];
    let mut stamp = vec![usize::MAX; segs.len()];
    let mut steps = 0u64;
    for &j in &deep {
        let (p0, p1, h) = (orig[keep[j]], orig[keep[j + 1]], depth[j]);
        let m = a + h + tol;
        let b = seg_bbox(&[p0, p1]);
        grid.in_box([b[0] - m, b[1] - m, b[2] + m, b[3] + m], &mut |k| {
            steps += 1;
            if stamp[k] == j {
                return true;
            }
            stamp[k] = j;
            let [p, q] = segs[k];
            // the replaced vertices lie within `h` of the chord
            if seg_seg_dist(p, q, p0, p1) - h >= r {
                return true;
            }
            for e in orig[keep[j]..=keep[j + 1]].windows(2) {
                steps += 1;
                if let Some(span) = capsule_span(p, q, e[0], e[1], r) {
                    cut[k].push(span);
                }
            }
            true
        });
    }
    if !fuel.spend(steps * OFFSET_FUEL_PER_STEP) {
        return None;
    }
    let mut out = Vec::new();
    let mut k0 = 0;
    for pts in curves {
        let nk = pts.len() - 1;
        let spans = &mut cut[k0..k0 + nk];
        k0 += nk;
        if spans.iter().all(|c| c.is_empty()) {
            out.push(pts);
            continue;
        }
        let ring = pts.len() >= 4 && odist(pts[0], pts[nk]) <= DEDUP_M;
        let mut pieces: Vec<Vec<P2>> = Vec::new();
        let mut cur: Vec<P2> = vec![pts[0]];
        let mut finish = |cur: &mut Vec<P2>| {
            if cur.len() >= 2 {
                pieces.push(std::mem::take(cur));
            }
            cur.clear();
        };
        for (k, c) in spans.iter_mut().enumerate() {
            let (p, q) = (pts[k], pts[k + 1]);
            c.sort_by(|x, y| x.0.total_cmp(&y.0));
            let mut t = 0.0;
            for &(t0, t1) in c.iter() {
                if t0 > t {
                    if cur.is_empty() {
                        cur.push(lerp(p, q, t));
                    }
                    cur.push(lerp(p, q, t0));
                }
                finish(&mut cur);
                t = f64::max(t, t1);
            }
            if t < 1.0 {
                if cur.is_empty() {
                    cur.push(lerp(p, q, t));
                }
                cur.push(q);
            }
        }
        finish(&mut cur);
        // a ring cut open: its last piece continues into its first
        if ring && pieces.len() >= 2 && pieces[0][0] == pts[0] && pieces[pieces.len() - 1].last() == Some(&pts[nk]) {
            let first = pieces.remove(0);
            pieces.last_mut().unwrap().extend_from_slice(&first[1..]);
        }
        out.extend(pieces);
    }
    Some(out)
}

/// One-sided offset curve of a path in metres (`d > 0`: left of its direction, `d < 0`:
/// right), much as GEOS builds it: the raw offset (shifted segments, round outer corners) is
/// cut at its crossings with itself, with the raw offset of the other side and with the
/// round ends of the band, and only the pieces at distance `|d|` from the path on the
/// requested side are kept. That removes the loops and cusps a raw offset makes where `|d|`
/// exceeds the curvature radius, the path comes back near itself or segments are very short.
/// A closed path (a ring) gives a closed curve; inwards, nothing is left once `|d|` reaches
/// the radius of the largest circle inside it. The pieces follow the path's direction; where
/// the path passes within `|d|` of itself the offset is cut apart, and nothing is left when
/// `|d|` is too large. No point of the result is nearer to the path than `|d|`, but for the
/// chords of round joins (up to 0.13 % nearer).
///
/// The raw offset is built from chords of the path ([`offset_chords`]), which leaves out the
/// jitter of densely digitised lines. The work then grows with how often the path comes back
/// within `|d|` of itself, which no vertex count tells in advance: it spends `fuel` as it
/// goes, and `None` means it ran out.
pub fn offset_curve(line: &[P2], d: f64, fuel: &mut OffsetFuel) -> Option<Vec<Vec<P2>>> {
    let a = d.abs();
    if line.len() < 2 || !(a > 0.0) {
        return Some(vec![]);
    }
    // local coordinates keep the arithmetic well away from the projection's large offsets
    let o = line[0];
    let mut orig: Vec<P2> = line.iter().map(|p| [p[0] - o[0], p[1] - o[1]]).collect();
    orig.dedup_by(|q, p| odist(*p, *q) <= DEDUP_M);
    if orig.len() < 2 {
        return Some(vec![]);
    }
    let closed = orig.len() >= 4 && odist(orig[0], orig[orig.len() - 1]) <= DEDUP_M;
    if closed {
        let first = orig[0];
        *orig.last_mut().unwrap() = first;
    }
    let ext = orig.iter().fold(0.0_f64, |m, p| m.max(p[0].abs()).max(p[1].abs()));
    let tol = 1e-9 * (ext + a) + 1e-9;
    let s = d.signum();
    let (keep, depth) = offset_chords(&orig, s, a, tol, fuel)?;
    let path: Vec<P2> = keep.iter().map(|&k| orig[k]).collect();
    let nseg = path.len() - 1;
    let units: Vec<P2> = path
        .windows(2)
        .map(|w| {
            let l = odist(w[0], w[1]);
            [(w[1][0] - w[0][0]) / l, (w[1][1] - w[0][1]) / l]
        })
        .collect();
    let (r, kind, src) = raw_offset(&path, &units, closed, a, s);
    let (other, _, _) = raw_offset(&path, &units, closed, a, -s);
    let segs: Vec<[P2; 2]> = r.windows(2).chain(other.windows(2)).map(|w| [w[0], w[1]]).collect();
    let bbs: Vec<BBox> = segs.iter().map(seg_bbox).collect();
    let nr = r.len() - 1;
    if !fuel.spend(segs.len() as u64 * OFFSET_FUEL_PER_RAW_SEGMENT) {
        return None;
    }

    // cut the raw offset wherever it may enter or leave the distance-|d| band
    let total: f64 = segs.iter().map(|s| odist(s[0], s[1])).sum();
    let grid = SegGrid::new(&segs, (2.0 * total / segs.len() as f64).max(1e-3 * a).max(1e-6));
    let mut cuts: Vec<Vec<f64>> = vec![Vec::new(); nr];
    let mut stamp = vec![usize::MAX; segs.len()];
    for i in 0..nr {
        let [p, q] = segs[i];
        let bi = bbs[i];
        let mut found: Vec<(usize, f64, f64)> = Vec::new();
        let mut visits = 1;
        grid.crossing(&segs[i], |j| {
            visits += 1;
            if stamp[j] == i {
                return;
            }
            stamp[j] = i;
            let adjacent = j < nr && (j <= i + 1 || (closed && i == 0 && j == nr - 1));
            let bj = bbs[j];
            if adjacent || bj[0] > bi[2] || bi[0] > bj[2] || bj[1] > bi[3] || bi[1] > bj[3] {
                return;
            }
            if let Some((t, u)) = crossing_params(p, q, segs[j][0], segs[j][1])
                && (0.0..=1.0).contains(&t)
                && (0.0..=1.0).contains(&u)
            {
                found.push((j, t, u));
            }
        });
        if !fuel.spend(visits * OFFSET_FUEL_PER_CROSSING_STEP) {
            return None;
        }
        for (j, t, u) in found {
            cuts[i].push(t);
            if j < nr {
                cuts[j].push(u);
            }
        }
        // the round ends of the band
        if !closed {
            let w = [q[0] - p[0], q[1] - p[1]];
            let ww = w[0] * w[0] + w[1] * w[1];
            for c in [path[0], path[nseg]] {
                let pc = [p[0] - c[0], p[1] - c[1]];
                let b = w[0] * pc[0] + w[1] * pc[1];
                let disc = b * b - ww * (pc[0] * pc[0] + pc[1] * pc[1] - a * a);
                if ww > 0.0 && disc > 0.0 {
                    let sq = disc.sqrt();
                    cuts[i].extend([(-b - sq) / ww, (-b + sq) / ww].into_iter().filter(|t| (0.0..=1.0).contains(t)));
                }
            }
        }
    }
    let mut pos: Vec<f64> = vec![0.0];
    for (i, c) in cuts.iter().enumerate() {
        pos.extend(c.iter().map(|t| i as f64 + t));
        // an inner chord is a piece of its own: its ends may be offset points, its inside never
        if kind[i] == Raw::Inner {
            pos.extend([i as f64, (i + 1) as f64]);
        }
    }
    pos.push(nr as f64);
    pos.sort_by(f64::total_cmp);
    pos.dedup_by(|b, a| *b - *a <= 1e-12);

    let at = |g: f64| {
        let i = (g.floor() as usize).min(nr - 1);
        lerp(r[i], r[i + 1], g - i as f64)
    };
    let psegs: Vec<[P2; 2]> = path.windows(2).map(|w| [w[0], w[1]]).collect();
    let pcell = (2.0 * opath_len(&path) / nseg as f64).max(a).max(1e-6);
    let mut band = Band {
        path: &path,
        units: units.clone(),
        bbs: psegs.iter().map(seg_bbox).collect(),
        closed,
        a,
        tol,
        want: s,
        grid: SegGrid::new(&psegs, pcell),
        stamp: vec![0; nseg],
        query: 0,
        cand: Vec::new(),
        visits: 0,
    };
    // Inwards of a ring the curve must leave room on its far side: where `a` is the radius of
    // the largest circle inside, the raw offset collapses onto points and lines that are at
    // `a` from the ring without enclosing anything.
    let inward = closed && signed_area(&path) * s > 0.0;
    let room = 4.0 * tol;
    let chord = a * (0.5 * ARC_STEP).cos() - tol;
    let mut runs: Vec<(f64, f64, Vec<P2>)> = Vec::new();
    for w in pos.windows(2) {
        if !fuel.spend(OFFSET_FUEL_PER_PIECE + std::mem::take(&mut band.visits) * OFFSET_FUEL_PER_STEP) {
            return None;
        }
        let (g0, g1) = (w[0], w[1]);
        let first = g0.floor() as usize + 1;
        let last = (g1.ceil() as usize).saturating_sub(1);
        let inner = (first..=last).filter(|&k| (k as f64) > g0 + 1e-9 && (k as f64) < g1 - 1e-9);
        let inner: Vec<usize> = inner.collect();
        let (rep, seg, thr) = match inner.get(inner.len() / 2) {
            Some(&k) => (r[k], k.min(nr - 1), a - tol),
            None => {
                let m = 0.5 * (g0 + g1);
                let i = (m.floor() as usize).min(nr - 1);
                (at(m), i, if kind[i] == Raw::Arc { chord } else { a - tol })
            }
        };
        if kind[seg] == Raw::Inner && inner.is_empty() || !band.holds(rep, src[seg], thr) {
            continue;
        }
        if inward {
            // a step off the curve, away from the ring
            let (p0, p1) = match inner.get(inner.len() / 2) {
                Some(&k) => (r[k - 1], r[k + 1]),
                None => (r[seg], r[seg + 1]),
            };
            let l = odist(p0, p1);
            let n = [-(p1[1] - p0[1]) * s / l, (p1[0] - p0[0]) * s / l];
            if !(l > 0.0) || band.nearer([rep[0] + n[0] * room, rep[1] + n[1] * room], a + 0.1 * room) {
                continue;
            }
        }
        match runs.last_mut() {
            Some((_, end, pts)) if *end == g0 => {
                pts.extend(inner.iter().map(|&k| r[k]));
                pts.push(at(g1));
                *end = g1;
            }
            _ => {
                let mut pts = vec![at(g0)];
                pts.extend(inner.iter().map(|&k| r[k]));
                pts.push(at(g1));
                runs.push((g0, g1, pts));
            }
        }
    }
    // a closed curve's last run continues into its first
    if closed && runs.len() >= 2 && runs[0].0 == 0.0 && runs[runs.len() - 1].1 == nr as f64 {
        let (_, _, first) = runs.remove(0);
        runs.last_mut().unwrap().2.extend_from_slice(&first[1..]);
    }
    let mut out: Vec<Vec<P2>> = Vec::new();
    for (_, _, mut pts) in runs {
        pts.dedup_by(|q, p| odist(*p, *q) <= DEDUP_M);
        // runs separated only by a cut too small to see
        if let Some(prev) = out.last_mut()
            && odist(prev[prev.len() - 1], pts[0]) <= 10.0 * tol
        {
            prev.extend_from_slice(&pts[1..]);
            continue;
        }
        out.push(pts);
    }
    out.retain(|p| p.len() >= 2);
    if !fuel.spend(std::mem::take(&mut band.visits) * OFFSET_FUEL_PER_STEP) {
        return None;
    }
    for p in out.iter_mut() {
        if odist(p[0], p[p.len() - 1]) > DEDUP_M {
            band.trim_end(p);
            p.reverse();
            band.trim_end(p);
            p.reverse();
        }
    }
    out.retain(|p| p.len() >= 2);
    if !fuel.spend(band.visits * OFFSET_FUEL_PER_TRIM_STEP) {
        return None;
    }
    let mut out = clip_to_line(out, &orig, &keep, &depth, a, tol, fuel)?;
    out.retain(|p| p.len() >= 2 && opath_len(p) > 10.0 * tol);
    for p in out.iter_mut() {
        for q in p.iter_mut() {
            *q = [q[0] + o[0], q[1] + o[1]];
        }
    }
    Some(out)
}

/// Polygon buffer (geo), outwards for `d > 0`, inwards for `d < 0`, with round corners.
pub fn buffer_polygons(p: &MultiPolygon<f64>, d: f64) -> MultiPolygon<f64> {
    p.buffer_with_style(BufferStyle::new(d).line_join(GeoJoin::Round(0.1)))
}

// ---------- trim / extend ----------

/// Distance along the ray from `p` (direction `from` -> `p`) to the first crossing with the
/// boundary, and that crossing.
fn ray_hit(from: P2, p: P2, boundary: &[Vec<P2>]) -> Option<(f64, P2)> {
    let len = dist(from, p);
    if len <= 0.0 {
        return None;
    }
    let mut best: Option<(f64, P2)> = None;
    for w in boundary.iter().flat_map(|b| b.windows(2)) {
        if let Some((t, u)) = seg_inter(from, p, w[0], w[1])
            && t > 1.0
            && (0.0..=1.0).contains(&u)
        {
            let s = (t - 1.0) * len;
            if best.is_none_or(|(bs, _)| s < bs) {
                best = Some((s, lerp(w[0], w[1], u)));
            }
        }
    }
    best
}

/// Trims or extends one end of the line geometry to the boundary; `true` when trimmed.
fn trim_extend_geom(g: &Geom, boundary: &[Vec<P2>]) -> Res<(Geom, bool)> {
    let mut parts: Vec<Vec<P2>> = g.to_multi_line().map(|m| m.0.iter().map(ring_points).collect()).unwrap_or_default();
    for p in parts.iter_mut() {
        dedup(p, DEDUP_M);
    }
    // the end nearest to the boundary, over all parts: (distance, part, at the start)
    let mut ends: Vec<(f64, usize, bool)> = Vec::new();
    for (k, p) in parts.iter().enumerate().filter(|(_, p)| p.len() >= 2) {
        ends.push((dist_to_paths(p[0], boundary), k, true));
        ends.push((dist_to_paths(p[p.len() - 1], boundary), k, false));
    }
    ends.sort_by(|a, b| a.0.total_cmp(&b.0));
    let &(d0, k, at_start) = ends.first().ok_or("the line has no segments")?;
    if d0 <= TOUCH_M {
        return Err("the line already meets the boundary".into());
    }
    let line = parts[k].clone();
    let n = line.len();
    let hs = hits_any(&line, boundary);
    let new: Vec<P2>;
    let trimmed;
    if !hs.is_empty() {
        let h = if at_start { hs[0] } else { hs[hs.len() - 1] };
        let p = lerp(line[h.i], line[h.i + 1], h.t.clamp(0.0, 1.0));
        new = if at_start {
            std::iter::once(p).chain(line[h.i + 1..].iter().copied()).collect()
        } else {
            line[..=h.i].iter().copied().chain(std::iter::once(p)).collect()
        };
        trimmed = true;
    } else {
        let try_end = |start: bool| {
            if start { ray_hit(line[1], line[0], boundary) } else { ray_hit(line[n - 2], line[n - 1], boundary) }
        };
        let (start, (_, p)) = match try_end(at_start) {
            Some(r) => (at_start, r),
            None => match try_end(!at_start) {
                Some(r) => (!at_start, r),
                None => return Err("neither end points towards the boundary".into()),
            },
        };
        let mut v = line.clone();
        if start {
            v[0] = p;
        } else {
            v[n - 1] = p;
        }
        new = v;
        trimmed = false;
    }
    let mut new = new;
    dedup(&mut new, DEDUP_M);
    if new.len() < 2 || path_len(&new) <= TOUCH_M {
        return Err("nothing would be left of the line".into());
    }
    parts[k] = new;
    Ok((from_lines(parts.iter().map(|p| line_from_points(p)).collect()), trimmed))
}
