//! Polygon editing: cut/clip with a polygon, rings, parts, gaps.
//!
//! Tool commands follow the model in [`crate::selection`]: the selected polygon(s) of the
//! active layer are the tools, the selected polygons of other layers (or the active layer's
//! polygons touching the tools) are the targets.
//!
//! Ring and part commands edit the selected polygons of the active layer. Selected features
//! in other layers act as locators: only the holes/parts they intersect are affected.
//!
//! Boolean ops run in metres through a [`VertexSnap`], so untouched vertices are written back
//! bit-exact. Ring and part commands that only regroup existing rings never project at all.

use std::collections::HashMap;

use geo::{
    Area, BooleanOps, BoundingRect, Buffer, Intersects, LineString, MultiPolygon, Polygon, Rect, Winding, unary_union,
};
use i_overlay::core::fill_rule::FillRule;
use i_overlay::core::overlay_rule::OverlayRule;
use i_overlay::core::solver::Solver;
use i_overlay::float::overlay::{FloatOverlay, OverlayOptions};
use rustgis_plugin::{Map, Value, json};

use crate::Ctx;
use crate::geojson::{self, Dim, Geom, P2, bboxes_intersect, contours, from_polygons, prepare_output, shape_to_polygon};
use crate::host_io::budget::{self, Meter};
use crate::host_io::{self, Feature, LAYER_READ_CAP, LayerInfo, Res};
use crate::proj::{SNAP_TOL_M, VertexSnap};
use crate::selection::{self, EditSpec, ToolSpec};
use crate::topo;

/// Areas below this (m²) are numerical noise of the boolean ops: such parts are dropped and
/// such area changes count as "unchanged".
pub const AREA_EPS_M2: f64 = 1e-4;

/// Fill Gap ignores enclosed gaps whose area, without the parts narrower than
/// [`MIN_GAP_WIDTH_M`], is below this (m²): slivers left by digitising, and the hair-thin
/// wedges between edges
/// that are straight in the layer's CRS but not in the working frame (a T-junction along a
/// 100 m edge opens a wedge ~0.25 mm wide).
pub const MIN_GAP_M2: f64 = 0.01;
pub const MIN_GAP_WIDTH_M: f64 = 0.01;

const CUT_USAGE: &str =
    "Select the cutting polygon(s) in the active layer and the polygons to cut in another layer";
const CLIP_USAGE: &str =
    "Select the clipping polygon(s) in the active layer and the polygons to clip in another layer";
const ADD_RING_USAGE: &str =
    "Select the polygon(s) to make holes of in the active layer and the polygons around them in another layer";
const ADD_PART_USAGE: &str =
    "Select the polygon(s) to add as parts in the active layer and the one polygon to add them to in another layer";
const DELETE_RING_USAGE: &str = "Select the polygons whose holes to delete in the active layer \
     (optionally points or polygons in another layer to pick the holes)";
const FILL_RING_USAGE: &str = "Select the polygons whose holes to fill in the active layer \
     (optionally points or polygons in another layer to pick the holes)";
const DELETE_PART_USAGE: &str = "Select multi-part polygons in the active layer \
     (optionally points or polygons in another layer to pick the parts to delete)";
const SPLIT_PART_USAGE: &str = "Select multi-part polygons in the active layer \
     (optionally points or polygons in another layer to pick the parts to split off)";
const FILL_GAP_USAGE: &str = "Select the polygons around the gaps in the active layer";

const POLY: &[Dim] = &[Dim::Polygon];

/// What Cut and Clip may spend in all, reading and writing included; the rest is left for the
/// host calls and the message. The estimates are upper bounds: editing by about 15 %, the
/// boolean ops by 10 to 30 %.
pub const CUT_WORK_FUEL: u64 = budget::COMMAND_FUEL * 95 / 100;

/// Fuel per i_overlay boolean op or slice (setting it up and turning the result into
/// polygons), per segment it sweeps, per pair of segments it compares and per crossing it
/// resolves (into a piece of its own, for a slice): upper bounds fitted to `diag-bench` runs
/// on parcels, combs, zigzags and splits into thousands of pieces.
const OVERLAY_FUEL_PER_OP: u64 = 500_000;
const OVERLAY_FUEL_PER_SEGMENT: u64 = 9_000;
/// The tree i_overlay sweeps many segments with when their x ranges overlap a lot.
const OVERLAY_FUEL_PER_TREE_SEGMENT: u64 = 17_000;
/// What a slice (Split With Line) takes per segment on top: it builds its pieces from a graph
/// of the segments.
const SLICE_FUEL_PER_SEGMENT: u64 = 7_000;
const OVERLAY_FUEL_PER_PAIR: u64 = 120;
const OVERLAY_FUEL_PER_CROSSING: u64 = 50_000;

/// Estimated fuel of a boolean op or slice of `segments` segments in all, `pairs` of them
/// side by side (see [`XRanges`]), that cross about `crossings` times.
pub fn overlay_fuel(segments: usize, pairs: u64, crossings: u64) -> u64 {
    let per_segment = if is_list(segments, pairs) { OVERLAY_FUEL_PER_SEGMENT } else { OVERLAY_FUEL_PER_TREE_SEGMENT };
    OVERLAY_FUEL_PER_OP
        + segments as u64 * per_segment
        + pairs * OVERLAY_FUEL_PER_PAIR
        + crossings * OVERLAY_FUEL_PER_CROSSING
}

/// Estimated fuel of a slice of `segments` segments (see [`overlay_fuel`]).
pub fn slice_fuel(segments: usize, pairs: u64, crossings: u64) -> u64 {
    let per_segment = if is_list(segments, pairs) { SLICE_FUEL_PER_SEGMENT } else { 2 * SLICE_FUEL_PER_SEGMENT };
    overlay_fuel(segments, pairs, crossings) + segments as u64 * per_segment
}

/// Up to this many overlapping x ranges per segment i_overlay's list sweep beats its tree,
/// however many segments there are: on parcels the tree, which it picks from 4000 segments
/// on, takes nearly twice the fuel per segment.
const LIST_PAIRS_PER_SEGMENT: u64 = 16;

/// The i_overlay solver for `segments` segments with `pairs` overlapping x ranges.
pub fn solver_for(segments: usize, pairs: u64) -> Solver {
    if pairs <= LIST_PAIRS_PER_SEGMENT * segments as u64 { Solver::LIST } else { Solver::AUTO }
}

/// Whether i_overlay sweeps them with its list (see [`solver_for`]; [`Solver::AUTO`] does up to
/// 4000 segments).
fn is_list(segments: usize, pairs: u64) -> bool {
    pairs <= LIST_PAIRS_PER_SEGMENT * segments as u64 || segments < 4000
}

/// `a` combined with `b` by `rule` as geo's [`BooleanOps`] does it (even-odd, OGC output),
/// with `solver`.
fn boolean(a: &MultiPolygon<f64>, b: &MultiPolygon<f64>, rule: OverlayRule, solver: Solver) -> MultiPolygon<f64> {
    let paths = |m: &MultiPolygon<f64>| m.0.iter().flat_map(contours).collect::<Vec<Vec<P2>>>();
    let shapes = FloatOverlay::with_subj_and_clip_custom(&paths(a), &paths(b), OverlayOptions::ogc(), solver)
        .overlay(rule, FillRule::EvenOdd);
    MultiPolygon(shapes.iter().map(|s| shape_to_polygon(s)).collect())
}

/// The union of `ps` as geo's [`unary_union`] does it (filled by the winding of the first
/// ring that has one, OGC output), with `solver`.
fn union_of(ps: &[Polygon<f64>], solver: Solver) -> MultiPolygon<f64> {
    let rings = || ps.iter().flat_map(|p| std::iter::once(p.exterior()).chain(p.interiors()));
    let fill = match rings().find_map(|r| r.winding_order()) {
        Some(geo::winding_order::WindingOrder::CounterClockwise) => FillRule::Positive,
        _ => FillRule::Negative,
    };
    let paths: Vec<Vec<P2>> = ps.iter().flat_map(contours).collect();
    let shapes = FloatOverlay::with_subj_custom(&paths, OverlayOptions::ogc(), solver).overlay(OverlayRule::Subject, fill);
    MultiPolygon(shapes.iter().map(|s| shape_to_polygon(s)).collect())
}

/// The x ranges of a set of segments. Below some thousands of segments i_overlay sweeps them
/// with a list, comparing every two whose x ranges overlap: few for parcels, all of them for
/// a comb's teeth.
pub struct XRanges {
    ranges: Vec<(f64, f64)>,
    /// The starts and the ends, sorted.
    lo: Vec<f64>,
    hi: Vec<f64>,
    /// Pairs of them that overlap.
    pub pairs: u64,
}

impl XRanges {
    pub fn new<'a>(paths: impl IntoIterator<Item = &'a [geojson::P2]>) -> XRanges {
        // i_overlay merges repeated segments (either way round) before sweeping
        let mut segs: Vec<[u64; 4]> = paths
            .into_iter()
            .flat_map(|p| p.windows(2))
            .map(|w| {
                let [a, b] = if (w[0][0], w[0][1]) <= (w[1][0], w[1][1]) { [w[0], w[1]] } else { [w[1], w[0]] };
                [a[0].to_bits(), a[1].to_bits(), b[0].to_bits(), b[1].to_bits()]
            })
            .collect();
        segs.sort_unstable();
        segs.dedup();
        let ranges: Vec<(f64, f64)> = segs.iter().map(|s| (f64::from_bits(s[0]), f64::from_bits(s[2]))).collect();
        let mut lo: Vec<f64> = ranges.iter().map(|r| r.0).collect();
        let mut hi: Vec<f64> = ranges.iter().map(|r| r.1).collect();
        lo.sort_unstable_by(f64::total_cmp);
        hi.sort_unstable_by(f64::total_cmp);
        let mut x = XRanges { ranges, lo, hi, pairs: 0 };
        // each pair twice, and every range with itself
        x.pairs = (x.with(&x) - x.len() as u64) / 2;
        x
    }

    /// The rings of polygons.
    pub fn of_polygons(ps: &[Polygon<f64>]) -> XRanges {
        let rings: Vec<Vec<geojson::P2>> =
            ps.iter().flat_map(|p| std::iter::once(p.exterior()).chain(p.interiors())).map(geojson::ring_points).collect();
        XRanges::new(rings.iter().map(|r| &r[..]))
    }

    pub fn len(&self) -> usize {
        self.ranges.len()
    }

    /// Estimated fuel to build it: sorting and counting.
    pub fn fuel(&self) -> u64 {
        self.len() as u64 * 1_500
    }

    /// Pairs of a range here and one of `other` that overlap.
    pub fn with(&self, other: &XRanges) -> u64 {
        // those of `other` starting by b, less those ending before a
        let meeting = |a: f64, b: f64| other.lo.partition_point(|x| *x <= b) - other.hi.partition_point(|x| *x < a);
        self.ranges.iter().map(|&(a, b)| meeting(a, b) as u64).sum()
    }
}

/// Removes the tool polygons' area from the target polygons.
pub fn cut_with_polygon(ctx: &Ctx) -> Res<Value> {
    cut_or_clip(ctx, false)
}

/// Keeps only the part of the target polygons inside the tool polygons.
pub fn clip_with_polygon(ctx: &Ctx) -> Res<Value> {
    cut_or_clip(ctx, true)
}

fn cut_or_clip(ctx: &Ctx, clip: bool) -> Res<Value> {
    let usage = if clip { CLIP_USAGE } else { CUT_USAGE };
    let spec = ToolSpec { whole_layer: ctx.settings.whole_layer, ..ToolSpec::new(usage, POLY, POLY) };
    let sel = selection::gather(&spec)?;
    let mut pl = topo::Placer::new(sel.frame()?);
    // every target is registered before the tools are snapped to them
    let targets: Vec<(&LayerInfo, &Feature, Geom)> = sel
        .targets
        .iter()
        .flat_map(|set| set.features.iter().map(move |f| (&set.layer, f)))
        .map(|(l, f)| Ok((l, f, pl.add_target(f.geom()?))))
        .collect::<Res<_>>()?;
    let mut tool_parts = Vec::new();
    for t in &sel.tools {
        tool_parts.extend(polygons_of(&pl.add_tool(t.geom()?)).0);
    }
    let read = budget::edit_fuel(
        sel.target_count(),
        budget::vertices(&sel.tools) + sel.targets.iter().map(|t| budget::vertices(&t.features)).sum::<usize>(),
    );
    let mut fuel = Meter::new(
        CUT_WORK_FUEL.saturating_sub(read),
        format!(
            "{} the targets with these polygons is more than one command can do. Select fewer targets or tool \
             polygons, or simplify them, and repeat the command for the rest.",
            if clip { "Clipping" } else { "Cutting" }
        ),
    );
    let metric: Vec<&Geom> = targets.iter().map(|t| &t.2).collect();
    let results = cut_results(&mut pl, &metric, tool_parts, clip, &mut fuel)?;

    let mut plan = Plan::default();
    let mut new_vertices = Vec::new();
    let mut edited = Vec::new();
    let (mut changed, mut deleted, mut unchanged) = (0, 0, 0);
    let (mut owners, mut kept_vertices) = (Vec::new(), 0);
    for (owner, ((layer, f, g), (a0, result))) in targets.iter().zip(results).enumerate() {
        let a1 = result.unsigned_area();
        let overlap = if clip { a1 } else { a0 - a1 };
        // in the active layer the targets are found by touching; a neighbour that only
        // shares an edge with the tool is not meant
        if sel.targets_from_active && overlap < AREA_EPS_M2 {
            continue;
        }
        if (a1 - a0).abs() < AREA_EPS_M2 {
            unchanged += 1;
        } else if result.0.is_empty() {
            deleted += 1;
            plan.delete.push((layer.id, f.row));
            edited.push((layer.id, f.id));
        } else {
            changed += 1;
            kept_vertices += g.num_coords();
            let g = pl.unproject(&Geom::MultiPolygon(result), &mut new_vertices, &mut fuel)?;
            plan.set.push((layer.id, f.row, prepare_output(g)?));
            owners.push(owner);
            edited.push((layer.id, f.id));
        }
    }
    if changed + deleted == 0 {
        let why = if clip {
            "every target already lies inside the clipping polygons"
        } else {
            "the cutting polygons do not overlap the targets"
        };
        return Err(format!("{usage}. ({why})"));
    }
    let results = owners.into_iter().zip(plan.set.iter_mut().map(|s| &mut s.2));
    topo::share_within(pl.frame(), results, &new_vertices, &mut fuel)?;
    // the vertices the targets had are written as part of editing them
    let vertices: usize = plan.set.iter().map(|s| s.2.num_coords()).sum();
    fuel.spend(budget::write_fuel(0, vertices.saturating_sub(kept_vertices)))?;
    edited.extend(sel.tools.iter().map(|t| (sel.active.id, t.id)));
    let scope = topo::Scope {
        layers: &sel.layers,
        edited: sel.targets.iter().map(|t| t.layer.id).collect(),
        exclude: edited,
        // the results were computed and their writes charged
        spent: read + fuel.spent,
        limit: CUT_WORK_FUEL,
        read_whole: sel.targets_from_active.then_some(sel.active.id),
    };
    let shared = topo::plan(ctx.settings.topological, &scope, &new_vertices)?;
    let tools_deleted = delete_tools(&mut plan, &sel.active, sel.tools.iter(), ctx.settings.keep_tools);
    let (sv, sf, note) = (shared.vertices(), shared.features(), shared.note());
    // before the plan deletes anything: the neighbours' rows are as read
    shared.apply()?;
    plan.apply()?;
    let verb = if clip { "Clipped" } else { "Cut" };
    let mut msg = format!("{verb} {changed} polygon(s)");
    if deleted > 0 {
        msg += &format!(", deleted {deleted} left empty");
    }
    if unchanged > 0 {
        msg += &format!(", {unchanged} unchanged");
    }
    if tools_deleted > 0 {
        msg += &format!(", removed {tools_deleted} tool feature(s)");
    }
    ctx.finish(
        &topo::with_note(&msg, &note),
        json!({"changed": changed, "deleted": deleted, "unchanged": unchanged, "toolsDeleted": tools_deleted,
            "sharedVertices": sv, "sharedFeatures": sf}),
    )
}

/// Each target's (`metric`, registered with `pl` in this order) area and what is left of it
/// cut (or clipped) by the tool polygons (metres, registered with `pl` too), in metres:
/// the target itself where the tools' box misses it. Charges `fuel` for snapping the tools
/// and the boolean ops.
pub fn cut_results(
    pl: &mut topo::Placer,
    metric: &[&Geom],
    tool_parts: Vec<Polygon<f64>>,
    clip: bool,
    fuel: &mut Meter,
) -> Res<Vec<(f64, MultiPolygon<f64>)>> {
    let tool_parts = snap_tool_polygons(pl, metric, tool_parts, fuel)?;
    let parts = XRanges::of_polygons(&tool_parts);
    fuel.spend(parts.fuel() + overlay_fuel(parts.len(), parts.pairs, 0))?;
    let tools = union_of(&tool_parts, solver_for(parts.len(), parts.pairs));
    let tool_bb = tools.bounding_rect();
    let tx = XRanges::of_polygons(&tools.0);
    fuel.spend(tx.fuel())?;
    let mut out = Vec::with_capacity(metric.len());
    for (owner, g) in metric.iter().enumerate() {
        let mp = polygons_of(g);
        let a0 = mp.unsigned_area();
        let overlaps = matches!((tool_bb, mp.bounding_rect()), (Some(a), Some(b)) if bboxes_intersect(&a, &b));
        let result = match (overlaps, clip) {
            (false, false) => mp,
            (false, true) => MultiPolygon(vec![]),
            (true, _) => {
                let gx = XRanges::of_polygons(&mp.0);
                let (segments, pairs) = (gx.len() + tx.len(), gx.pairs + tx.pairs + gx.with(&tx));
                fuel.spend(gx.fuel() + overlay_fuel(segments, pairs, pl.crossings(owner)))?;
                let rule = if clip { OverlayRule::Intersect } else { OverlayRule::Difference };
                drop_slivers(boolean(&mp, &tools, rule, solver_for(segments, pairs)))
            }
        };
        out.push((a0, result));
    }
    Ok(out)
}

/// The tool polygons (metres) snapped to the targets (see [`topo`]), so that a tool edge
/// digitised on a target's edge cuts along it and leaves no sliver.
pub(crate) fn snap_tool_polygons(
    pl: &mut topo::Placer,
    targets: &[&Geom],
    tools: Vec<Polygon<f64>>,
    fuel: &mut Meter,
) -> Res<Vec<Polygon<f64>>> {
    let mut rings: Vec<Vec<geojson::P2>> = Vec::new();
    let mut shape = Vec::new();
    for p in &tools {
        shape.push(1 + p.interiors().len());
        rings.extend(std::iter::once(p.exterior()).chain(p.interiors()).map(geojson::ring_points));
    }
    pl.snap_paths(targets, &mut rings, fuel)?;
    let mut it = rings.into_iter().map(|r| geojson::line_from_points(&r));
    Ok(shape
        .into_iter()
        .filter_map(|n| {
            let ext = it.next()?;
            let holes: Vec<LineString<f64>> = it.by_ref().take(n - 1).collect();
            (ext.0.len() >= 4).then(|| Polygon::new(ext, holes.into_iter().filter(|h| h.0.len() >= 4).collect()))
        })
        .collect())
}

/// Adds the tool polygons as holes in the target polygons.
pub fn add_ring(ctx: &Ctx) -> Res<Value> {
    let spec = ToolSpec { whole_layer: ctx.settings.whole_layer, ..ToolSpec::new(ADD_RING_USAGE, POLY, POLY) };
    let sel = selection::gather(&spec)?;
    let frame = sel.frame()?;
    struct Target<'a> {
        layer: i64,
        feature: &'a Feature,
        lonlat: Vec<Polygon<f64>>,
        metric: Vec<Polygon<f64>>,
        rings: usize,
    }
    let mut targets: Vec<Target> = Vec::new();
    for set in &sel.targets {
        for f in &set.features {
            let lonlat = polygons_of(f.geom()?).0;
            let metric = lonlat.iter().map(|p| frame.project_geo(p)).collect();
            targets.push(Target { layer: set.layer.id, feature: f, lonlat, metric, rings: 0 });
        }
    }
    let (mut added, mut outside, mut crossing) = (0, 0, 0);
    let mut used_tools = Vec::new();
    for t in &sel.tools {
        let mut all_used = true;
        for tool in polygons_of(t.geom()?).0 {
            let m = frame.project_geo(&tool);
            let ta = m.unsigned_area();
            let bb = m.bounding_rect();
            let mut placed = false;
            let mut touched = false;
            'targets: for tg in targets.iter_mut() {
                for (pl, pm) in tg.lonlat.iter_mut().zip(tg.metric.iter_mut()) {
                    if !matches!((bb, pm.bounding_rect()), (Some(a), Some(b)) if bboxes_intersect(&a, &b)) {
                        continue;
                    }
                    let outside_part = m.difference(pm).unsigned_area();
                    if outside_part > ta - AREA_EPS_M2 {
                        continue;
                    }
                    touched = true;
                    if outside_part > AREA_EPS_M2 {
                        continue;
                    }
                    // A valid hole leaves one connected part whose outline is the old one plus the
                    // tool's: it may touch the outline or other holes at points (OGC allows that,
                    // and the boolean op may then return it as a self-touching shell), but a
                    // stretch along them would shorten the outline, and touches that close off
                    // part of the interior would split it.
                    let d = drop_slivers(pm.difference(&m));
                    let want = perimeter(pm) + perimeter(&m);
                    if d.0.len() != 1 || (perimeter(&d.0[0]) - want).abs() > 1e-3 + 1e-9 * want {
                        continue;
                    }
                    pl.interiors_push(as_hole(tool.exterior(), pl.exterior()));
                    pm.interiors_push(m.exterior().clone());
                    tg.rings += 1;
                    placed = true;
                    break 'targets;
                }
            }
            if placed {
                added += 1;
            } else {
                all_used = false;
                if touched { crossing += 1 } else { outside += 1 }
            }
        }
        if all_used {
            used_tools.push(t);
        }
    }
    if added == 0 {
        return Err(format!(
            "{ADD_RING_USAGE}. (no tool polygon lies inside a target: {outside} outside, {crossing} crossing, overlapping \
             or running along a target's outline or holes)"
        ));
    }
    let mut plan = Plan::default();
    let mut rings_in = 0;
    for tg in targets.iter().filter(|t| t.rings > 0) {
        rings_in += 1;
        plan.set.push((tg.layer, tg.feature.row, prepare_output(from_polygons(tg.lonlat.clone()))?));
    }
    let tools_deleted = delete_tools(&mut plan, &sel.active, used_tools.into_iter(), ctx.settings.keep_tools);
    plan.apply()?;
    let mut msg = format!("Added {added} hole(s) to {rings_in} polygon(s)");
    if outside + crossing > 0 {
        msg += &format!(
            "; skipped {} tool polygon(s): {outside} not inside a target, {crossing} crossing, overlapping or \
             running along its outline or holes",
            outside + crossing
        );
    }
    if tools_deleted > 0 {
        msg += &format!(", removed {tools_deleted} tool feature(s)");
    }
    ctx.finish(
        &msg,
        json!({"rings": added, "changed": rings_in, "skipped": outside + crossing, "toolsDeleted": tools_deleted}),
    )
}

/// Removes holes from the selected polygons of the active layer (all holes, or only those
/// intersecting the selected features of other layers).
pub fn delete_ring(ctx: &Ctx) -> Res<Value> {
    let spec = EditSpec { whole_layer: ctx.settings.whole_layer, ..EditSpec::new(DELETE_RING_USAGE, POLY) };
    let ed = selection::active_features(&spec)?;
    let loc = Locators::read(&ed.layer, &ed.layers, "holes")?;
    let mut plan = Plan::default();
    let mut removed = 0;
    for f in &ed.features {
        let mut n = 0;
        let parts: Vec<Polygon<f64>> = polygons_of(f.geom()?)
            .0
            .into_iter()
            .map(|p| {
                let (ext, holes) = p.into_inner();
                let before = holes.len();
                let kept: Vec<LineString<f64>> = holes.into_iter().filter(|h| !loc.picks_ring(h)).collect();
                n += before - kept.len();
                Polygon::new(ext, kept)
            })
            .collect();
        if n > 0 {
            removed += n;
            plan.set.push((ed.layer.id, f.row, prepare_output(from_polygons(parts))?));
        }
    }
    if removed == 0 {
        return Err(format!("{DELETE_RING_USAGE}. ({})", loc.nothing("holes", ed.features.len())));
    }
    let changed = plan.set.len();
    plan.apply()?;
    let msg = format!("Deleted {removed} hole(s) in {changed} polygon(s)");
    ctx.finish(&msg, json!({"rings": removed, "changed": changed}))
}

/// Fills holes of the selected polygons with new features carrying the parent's attributes.
/// Islands lying in a hole (other parts of the feature, other polygons of the layer) stay out
/// of the new polygon, their own holes not: each hole filled by the command is filled once,
/// so an island's hole that gets its own fill stays out of the fill around the island, and
/// one that doesn't belongs to it. When the layer is too large to read, only the selected features are
/// checked for islands and the message says so: the fill may then overlap other polygons.
pub fn fill_ring(ctx: &Ctx) -> Res<Value> {
    let spec = EditSpec {
        whole_layer: ctx.settings.whole_layer,
        properties: true,
        ..EditSpec::new(FILL_RING_USAGE, POLY)
    };
    let ed = selection::active_features(&spec)?;
    let loc = Locators::read(&ed.layer, &ed.layers, "holes")?;
    let parts: Vec<Vec<Polygon<f64>>> = ed.features.iter().map(|f| Ok(polygons_of(f.geom()?).0)).collect::<Res<_>>()?;
    // (feature, part, hole)
    let mut holes = Vec::new();
    for (i, ps) in parts.iter().enumerate() {
        for (k, p) in ps.iter().enumerate() {
            holes.extend(p.interiors().iter().filter(|h| loc.picks_ring(h)).map(|h| (i, k, h)));
        }
    }
    if holes.is_empty() {
        return Err(format!("{FILL_RING_USAGE}. ({})", loc.nothing("holes", ed.features.len())));
    }
    let boxes: Vec<Rect<f64>> = holes.iter().filter_map(|h| h.2.bounding_rect()).collect();
    // A layer too large to read leaves only the selected features to search for islands.
    let others = layer_islands(&ed, &boxes)?;
    let layer_checked = others.is_some();
    let others = others.unwrap_or_else(|| islands_in(&ed.features, &boxes));
    // holes filled by this command, by (feature id, ring): an island's hole among them is
    // part of the island for the fill around it
    let mut filled: HashMap<(i64, [u64; 2]), Vec<&LineString<f64>>> = HashMap::new();
    for &(i, _, h) in &holes {
        filled.entry((ed.features[i].id, ring_key(h))).or_default().push(h);
    }
    let is_filled = |id: i64, r: &LineString<f64>| filled.get(&(id, ring_key(r))).is_some_and(|v| v.contains(&r));
    let frame = ed.frame()?;
    let mut snap = VertexSnap::new(frame, SNAP_TOL_M);
    let mut plan = Plan::default();
    let (mut islands, mut covered) = (0, 0);
    for &(i, k, h) in &holes {
        let ring = ccw(h);
        let Some(hb) = ring.bounding_rect() else { continue };
        let id = ed.features[i].id;
        let own = parts[i].iter().enumerate().filter(|(j, _)| *j != k).map(|(_, p)| (id, p));
        let theirs = others.iter().filter(|o| o.0 != id).map(|o| (o.0, &o.1));
        let hole = Polygon::new(ring, vec![]);
        let hm = frame.project_geo(&hole);
        let mut inside = Vec::new();
        for (oid, p) in own.chain(theirs).filter(|(_, p)| p.exterior().bounding_rect().is_some_and(|b| within(&b, &hb))) {
            let shell = frame.project_geo(&Polygon::new(p.exterior().clone(), vec![]));
            if shell.unsigned_area() > AREA_EPS_M2 && shell.difference(&hm).unsigned_area() <= AREA_EPS_M2 {
                // every ring, filled holes too: their vertices come back bit-exact
                snap.add(&Geom::Polygon(p.clone()));
                let open: Vec<LineString<f64>> = p.interiors().iter().filter(|j| !is_filled(oid, j)).cloned().collect();
                inside.push(if open.is_empty() { shell } else { frame.project_geo(&Polygon::new(p.exterior().clone(), open)) });
            }
        }
        let g = if inside.is_empty() {
            Geom::Polygon(hole)
        } else {
            snap.add(&Geom::Polygon(hole));
            let free = drop_slivers(MultiPolygon(vec![hm]).difference(&unary_union(inside.iter())));
            if free.0.is_empty() {
                covered += 1;
                continue;
            }
            islands += inside.len();
            snap.unproject(&Geom::MultiPolygon(touching_exactly(free)))
        };
        plan.add.push((ed.layer.id, prepare_output(g)?, host_io::inherited_attributes(&ed.features[i].properties)));
    }
    let n = plan.add.len();
    if n == 0 {
        return Err(format!("{FILL_RING_USAGE}. (islands cover each of the {covered} hole(s) entirely)"));
    }
    plan.apply()?;
    let mut msg = format!("Filled {n} hole(s) with new polygons");
    if islands > 0 {
        msg += &format!("; {islands} island(s) left out of them");
    }
    if covered > 0 {
        msg += &format!("; {covered} hole(s) not filled: islands cover them entirely");
    }
    if !layer_checked {
        msg += &format!(
            "; the layer is too large to check its unselected polygons for islands: the new polygon may overlap \
             unselected polygons of '{}' lying in the hole. Select those polygons too, or undo",
            ed.layer.name
        );
    }
    ctx.finish(&msg, json!({"created": n, "islands": islands, "covered": covered, "layerChecked": layer_checked}))
}

/// A ring's first vertex, bit for bit: a key to find the same ring read twice.
fn ring_key(r: &LineString<f64>) -> [u64; 2] {
    r.0.first().map_or([0; 2], |c| [c.x.to_bits(), c.y.to_bits()])
}

/// `mp` with every vertex that lies on another ring's edge (within [`SNAP_TOL_M`], between
/// its ends) inserted into that edge. An island touching the hole's ring in the middle of an
/// edge leaves its vertex a hair off that edge; with the vertex in both rings they touch
/// exactly instead of crossing.
fn touching_exactly(mut mp: MultiPolygon<f64>) -> MultiPolygon<f64> {
    use geo::Coord;
    let p2 = |c: &Coord<f64>| [c.x, c.y];
    let rings: Vec<&LineString<f64>> =
        mp.0.iter().flat_map(|p| std::iter::once(p.exterior()).chain(p.interiors())).collect();
    // each ring's vertices by x, to look only at those near a segment
    let by_x: Vec<Vec<Coord<f64>>> = rings
        .iter()
        .map(|r| {
            let mut v = r.0.clone();
            v.sort_by(|a, b| a.x.total_cmp(&b.x));
            v
        })
        .collect();
    let t = SNAP_TOL_M;
    // (ring, segment end index, parameter, position)
    let mut ins: Vec<(usize, usize, f64, Coord<f64>)> = Vec::new();
    for (b, ring) in rings.iter().enumerate() {
        for i in 1..ring.0.len() {
            let (s0, s1) = (p2(&ring.0[i - 1]), p2(&ring.0[i]));
            let (x0, x1) = (s0[0].min(s1[0]) - t, s0[0].max(s1[0]) + t);
            let (y0, y1) = (s0[1].min(s1[1]) - t, s0[1].max(s1[1]) + t);
            for vs in by_x.iter().enumerate().filter(|(a, _)| *a != b).map(|(_, vs)| vs) {
                let from = vs.partition_point(|v| v.x < x0);
                for v in vs[from..].iter().take_while(|v| v.x <= x1).filter(|v| v.y >= y0 && v.y <= y1) {
                    let (d, at) = topo::seg_dist(p2(v), s0, s1);
                    if d <= t && topo::dist(p2(v), s0) > t && topo::dist(p2(v), s1) > t {
                        ins.push((b, i, at, *v));
                    }
                }
            }
        }
    }
    if ins.is_empty() {
        return mp;
    }
    ins.sort_by(|x, y| (x.0, x.1).cmp(&(y.0, y.1)).then(x.2.total_cmp(&y.2)));
    ins.dedup_by(|y, x| x.0 == y.0 && x.1 == y.1 && x.3 == y.3);
    let mut k = 0;
    let mut it = ins.into_iter().peekable();
    let mut fix = |l: &mut LineString<f64>| {
        let mut out = Vec::with_capacity(l.0.len());
        for (i, c) in l.0.iter().enumerate() {
            while let Some(&(_, _, _, at)) = it.peek().filter(|x| x.0 == k && x.1 == i) {
                out.push(at);
                it.next();
            }
            out.push(*c);
        }
        l.0 = out;
        k += 1;
    };
    for p in &mut mp.0 {
        p.exterior_mut(&mut fix);
        p.interiors_mut(|ls| ls.iter_mut().for_each(&mut fix));
    }
    mp
}

/// Margin of [`within`], in degrees (~1 mm): an island touching the hole's ring may lie a
/// hair beyond it.
const WITHIN_MARGIN_DEG: f64 = 1e-8;

/// Whether box `a` lies within box `b` (lon/lat), give or take [`WITHIN_MARGIN_DEG`].
fn within(a: &Rect<f64>, b: &Rect<f64>) -> bool {
    let m = WITHIN_MARGIN_DEG;
    a.min().x >= b.min().x - m && a.min().y >= b.min().y - m && a.max().x <= b.max().x + m && a.max().y <= b.max().y + m
}

/// The active layer's polygons that may be islands in `boxes` (see [`islands_in`]); `None`
/// when the layer is too large to read within what the command has left.
fn layer_islands(ed: &selection::Editable, boxes: &[Rect<f64>]) -> Res<Option<Vec<(i64, Polygon<f64>)>>> {
    let read;
    let feats = if ed.features.len() >= ed.layer.features {
        &ed.features
    } else {
        let spent = budget::edit_fuel(ed.features.len(), budget::vertices(&ed.features));
        let fuel_left = (budget::COMMAND_FUEL * 6 / 10).saturating_sub(spent);
        let max_bytes = (fuel_left / budget::READ_FUEL_PER_BYTE).min(budget::MAX_READ_BYTES as u64) as usize;
        match host_io::all_features_within(&ed.layer, LAYER_READ_CAP, max_bytes)? {
            Some((v, _)) => {
                read = v;
                &read
            }
            None => return Ok(None),
        }
    };
    Ok(Some(islands_in(feats, boxes)))
}

/// The polygons of `feats` (holes included) whose box lies within one of `boxes`, as
/// (feature id, polygon).
fn islands_in(feats: &[Feature], boxes: &[Rect<f64>]) -> Vec<(i64, Polygon<f64>)> {
    let mut out = Vec::new();
    for f in feats {
        let Some(g) = &f.geometry else { continue };
        if !g.bbox().is_some_and(|b| boxes.iter().any(|h| bboxes_intersect(h, &b))) {
            continue;
        }
        for p in polygons_of(g).0 {
            if let Some(b) = p.bounding_rect()
                && boxes.iter().any(|h| within(&b, h))
            {
                out.push((f.id, p));
            }
        }
    }
    out
}

/// Adds the tool polygons as new parts of the one target polygon.
pub fn add_part(ctx: &Ctx) -> Res<Value> {
    let spec = ToolSpec { whole_layer: ctx.settings.whole_layer, ..ToolSpec::new(ADD_PART_USAGE, POLY, POLY) };
    let sel = selection::gather(&spec)?;
    let n = sel.target_count();
    if n != 1 {
        return Err(format!("{ADD_PART_USAGE}. ({n} target polygons, the parts go to exactly one)"));
    }
    let set = &sel.targets[0];
    let target = &set.features[0];
    let mut parts = polygons_of(target.geom()?).0;
    for t in &sel.tools {
        parts.extend(polygons_of(t.geom()?).0);
    }
    let boxes: Vec<Option<Rect<f64>>> = parts.iter().map(|p| p.bounding_rect()).collect();
    let apart = boxes.iter().enumerate().all(|(i, a)| {
        boxes[i + 1..].iter().all(|b| !matches!((a, b), (Some(a), Some(b)) if bboxes_intersect(a, b)))
    });
    let total = parts.len();
    // separate parts are appended as they are; only parts that may overlap or touch go
    // through a union, which rewrites the rings (untouched vertices still come back exact)
    let merged = if apart {
        parts
    } else {
        let mut snap = VertexSnap::new(sel.frame()?, SNAP_TOL_M);
        let metric = polygons_of(&snap.project(&Geom::MultiPolygon(MultiPolygon(parts))));
        let u = drop_slivers(unary_union(metric.0.iter()));
        polygons_of(&snap.unproject(&Geom::MultiPolygon(u))).0
    };
    let after = merged.len();
    let g = prepare_output(from_polygons(merged))?;
    let mut plan = Plan::default();
    plan.set.push((set.layer.id, target.row, g));
    let tools_deleted = delete_tools(&mut plan, &sel.active, sel.tools.iter(), ctx.settings.keep_tools);
    plan.apply()?;
    let mut msg = format!("Added {} polygon(s) to the feature, which now has {after} part(s)", sel.tools.len());
    if after < total {
        msg += " (overlapping or touching parts were merged)";
    }
    if tools_deleted > 0 {
        msg += &format!(", removed {tools_deleted} tool feature(s)");
    }
    ctx.finish(&msg, json!({"added": sel.tools.len(), "parts": after, "toolsDeleted": tools_deleted}))
}

/// Deletes parts of the selected multi-part polygons: all but the largest, or the ones the
/// selected features of other layers intersect. The last part is never deleted.
pub fn delete_part(ctx: &Ctx) -> Res<Value> {
    parts_command(ctx, false)
}

/// Moves parts of the selected multi-part polygons into new features with the parent's
/// attributes: all but the largest, or the ones the selected features of other layers
/// intersect (the largest stays when they intersect every part).
pub fn split_off_part(ctx: &Ctx) -> Res<Value> {
    parts_command(ctx, true)
}

fn parts_command(ctx: &Ctx, split: bool) -> Res<Value> {
    let usage = if split { SPLIT_PART_USAGE } else { DELETE_PART_USAGE };
    let ed = selection::active_features(&EditSpec { properties: split, ..EditSpec::new(usage, POLY) })?;
    let loc = Locators::read(&ed.layer, &ed.layers, "parts")?;
    let mut plan = Plan::default();
    let (mut moved, mut all_hit) = (0, 0);
    for f in &ed.features {
        let parts = polygons_of(f.geom()?).0;
        if parts.len() < 2 {
            continue;
        }
        // comparing parts of one feature: lon/lat areas rank them like metric ones
        let largest = (0..parts.len())
            .max_by(|&a, &b| parts[a].unsigned_area().total_cmp(&parts[b].unsigned_area()))
            .unwrap_or(0);
        let mut picked: Vec<bool> =
            parts.iter().enumerate().map(|(i, p)| if loc.is_empty() { i != largest } else { loc.picks(p) }).collect();
        if picked.iter().all(|&b| b) {
            if !split {
                all_hit += 1;
                continue;
            }
            picked[largest] = false;
        }
        if !picked.iter().any(|&b| b) {
            continue;
        }
        let (gone, kept): (Vec<_>, Vec<_>) = parts.into_iter().zip(picked).partition(|(_, p)| *p);
        moved += gone.len();
        plan.set.push((ed.layer.id, f.row, prepare_output(from_polygons(kept.into_iter().map(|x| x.0).collect()))?));
        if split {
            let attrs = host_io::inherited_attributes(&f.properties);
            for (p, _) in gone {
                plan.add.push((ed.layer.id, prepare_output(Geom::Polygon(p))?, attrs.clone()));
            }
        }
    }
    if moved == 0 {
        let why = if all_hit > 0 {
            format!("the locators touch every part of {all_hit} feature(s); delete the feature instead")
        } else {
            if loc.is_empty() {
                format!("none of the {} selected polygon(s) has more than one part", ed.features.len())
            } else {
                loc.nothing("parts", ed.features.len())
            }
        };
        return Err(format!("{usage}. ({why})"));
    }
    let changed = plan.set.len();
    plan.apply()?;
    let mut msg = if split {
        format!("Split {moved} part(s) off {changed} polygon(s) into new features")
    } else {
        format!("Deleted {moved} part(s) of {changed} polygon(s)")
    };
    if all_hit > 0 {
        msg += &format!("; skipped {all_hit} feature(s) whose every part was picked");
    }
    let key = if split { "created" } else { "deleted" };
    ctx.finish(&msg, json!({key: moved, "changed": changed, "skipped": all_hit}))
}

/// Fills the enclosed gaps between the selected polygons (or the whole active layer) with
/// new features without attributes.
pub fn fill_gap(ctx: &Ctx) -> Res<Value> {
    let spec = EditSpec { whole_layer: ctx.settings.whole_layer, ..EditSpec::new(FILL_GAP_USAGE, POLY) };
    let ed = selection::active_features(&spec)?;
    let mut snap = VertexSnap::new(ed.frame()?, SNAP_TOL_M);
    let mut polys = Vec::new();
    for f in &ed.features {
        polys.extend(polygons_of(&snap.project(f.geom()?)).0);
    }
    let own_holes: Vec<(Rect<f64>, Polygon<f64>)> = polys
        .iter()
        .flat_map(|p| p.interiors())
        .map(|r| Polygon::new(r.clone(), vec![]))
        .filter_map(|h| Some((h.bounding_rect()?, h)))
        .collect();
    let mut small = 0;
    let mut plan = Plan::default();
    for gap in gaps(&polys) {
        let a = gap.unsigned_area();
        // an opening (shrink, then grow back) removes the parts narrower than the width
        if a < MIN_GAP_M2
            || gap.buffer(-0.5 * MIN_GAP_WIDTH_M).buffer(0.5 * MIN_GAP_WIDTH_M).unsigned_area() < MIN_GAP_M2
        {
            small += 1;
            continue;
        }
        let Some(bb) = gap.bounding_rect() else { continue };
        let is_own_hole = own_holes.iter().any(|(hb, h)| {
            bboxes_intersect(hb, &bb)
                && (h.unsigned_area() - a).abs() < AREA_EPS_M2
                && (gap.intersection(h).unsigned_area() - a).abs() < AREA_EPS_M2
        });
        if !is_own_hole {
            let g = prepare_output(snap.unproject(&Geom::Polygon(gap)))?;
            plan.add.push((ed.layer.id, g, Map::new()));
        }
    }
    let n = plan.add.len();
    if n == 0 {
        return Err(format!(
            "{FILL_GAP_USAGE}. (no enclosed gap between the {} polygons; gaps open to the outside are not filled{})",
            ed.features.len(),
            if small > 0 { format!(", {small} sliver(s) ignored") } else { String::new() }
        ));
    }
    plan.apply()?;
    let mut msg = format!("Filled {n} gap(s) with new polygons");
    if small > 0 {
        msg += &format!(" ({small} sliver(s) under {MIN_GAP_M2} m² or {MIN_GAP_WIDTH_M} m wide ignored)");
    }
    ctx.finish(&msg, json!({"created": n, "ignored": small}))
}

// ---------- helpers ----------

/// The polygons of a Polygon/MultiPolygon geometry (nothing for other dimensions, which the
/// specs already refuse).
/// The length of all rings of `p`.
fn perimeter(p: &Polygon<f64>) -> f64 {
    use geo::{Euclidean, Length};
    std::iter::once(p.exterior()).chain(p.interiors()).map(|r| Euclidean.length(r)).sum()
}

pub(crate) fn polygons_of(g: &Geom) -> MultiPolygon<f64> {
    g.to_multi_polygon().unwrap_or_else(|| MultiPolygon(vec![]))
}

fn drop_slivers(mut mp: MultiPolygon<f64>) -> MultiPolygon<f64> {
    mp.0.retain(|p| p.unsigned_area() >= AREA_EPS_M2);
    mp
}

fn signed_ring_area(r: &LineString<f64>) -> f64 {
    Polygon::new(r.clone(), vec![]).signed_area()
}

/// `r` as a counter-clockwise outer ring.
fn ccw(r: &LineString<f64>) -> LineString<f64> {
    let mut r = r.clone();
    if signed_ring_area(&r) < 0.0 {
        r.0.reverse();
    }
    r
}

/// `r` oriented against `exterior`, as holes are.
fn as_hole(r: &LineString<f64>, exterior: &LineString<f64>) -> LineString<f64> {
    let mut r = r.clone();
    if (signed_ring_area(&r) > 0.0) == (signed_ring_area(exterior) > 0.0) {
        r.0.reverse();
    }
    r
}

/// Queues the tool features of the active layer for deletion unless they are kept.
fn delete_tools<'a>(
    plan: &mut Plan,
    active: &LayerInfo,
    tools: impl Iterator<Item = &'a Feature>,
    keep: bool,
) -> usize {
    if keep {
        return 0;
    }
    let before = plan.delete.len();
    plan.delete.extend(tools.map(|t| (active.id, t.row)));
    plan.delete.len() - before
}

/// Gaps = holes of the union of the set, minus polygons lying inside those holes (islands).
/// Gaps open to the outside (a bay between three parcels) are not found, as in QGIS.
pub fn gaps(polys: &[Polygon<f64>]) -> Vec<Polygon<f64>> {
    let u = unary_union(polys.iter());
    let holes = MultiPolygon(
        u.0.iter().flat_map(|p| p.interiors().iter().map(|r| Polygon::new(r.clone(), vec![]))).collect(),
    );
    if holes.0.is_empty() {
        return vec![];
    }
    holes.difference(&u).0
}

/// The selected features of the other vector layers, which pick the holes or parts a
/// command acts on. Empty: act on all of them.
struct Locators(Vec<(Rect<f64>, geo::Geometry<f64>)>);

impl Locators {
    /// Refuses a selection made only of features without a usable geometry: read as "no
    /// locators", it would act on every hole or part instead of the ones meant.
    fn read(active: &LayerInfo, layers: &[LayerInfo], what: &str) -> Res<Locators> {
        let mut v = Vec::new();
        let mut unusable = Vec::new();
        for l in layers.iter().filter(|l| l.id != active.id && l.is_vector() && l.selected > 0) {
            if l.selected > LAYER_READ_CAP {
                return Err(format!("{} features are selected in '{}'; select fewer locators", l.selected, l.name));
            }
            let before = v.len();
            for f in host_io::selected_features(l.id, false)? {
                if let Some(g) = &f.geometry
                    && let Some(bb) = g.bbox()
                {
                    v.push((bb, g.to_geo()));
                }
            }
            if v.len() == before {
                unusable.push(format!("'{}'", l.name));
            }
        }
        if v.is_empty() && !unusable.is_empty() {
            return Err(format!(
                "The selected features in {} have no geometry to pick {what} with; select features with a geometry \
                 or clear that selection",
                unusable.join(", ")
            ));
        }
        Ok(Locators(v))
    }

    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Whether `p` is picked: always without locators, else when a locator intersects it.
    fn picks(&self, p: &Polygon<f64>) -> bool {
        if self.is_empty() {
            return true;
        }
        let Some(bb) = p.bounding_rect() else { return false };
        let g = geo::Geometry::Polygon(p.clone());
        self.0.iter().any(|(lb, lg)| bboxes_intersect(lb, &bb) && lg.intersects(&g))
    }

    /// Whether the area inside ring `r` (a hole) is picked.
    fn picks_ring(&self, r: &LineString<f64>) -> bool {
        self.picks(&Polygon::new(r.clone(), vec![]))
    }

    fn nothing(&self, what: &str, n: usize) -> String {
        if self.is_empty() {
            format!("the {n} selected polygon(s) have no {what}")
        } else {
            format!("none of the {what} of the {n} selected polygon(s) touch the selected features of the other layers")
        }
    }
}

/// A command's edits, applied in an order that keeps rows valid: geometry changes, deletes
/// per layer, then new features. The changed and new features end up selected.
#[derive(Default)]
struct Plan {
    set: Vec<(i64, usize, Geom)>,
    delete: Vec<(i64, usize)>,
    add: Vec<(i64, Geom, Map<String, Value>)>,
}

impl Plan {
    fn apply(self) -> Res<()> {
        for (layer, row, g) in &self.set {
            host_io::set_geometry(*layer, *row, g)?;
        }
        let mut select: Vec<(i64, usize)> = self.set.iter().map(|(l, r, _)| (*l, *r)).collect();
        let mut layers: Vec<i64> = self.delete.iter().map(|(l, _)| *l).collect();
        layers.sort_unstable();
        layers.dedup();
        for layer in layers {
            let rows: Vec<usize> = self.delete.iter().filter(|(l, _)| *l == layer).map(|(_, r)| *r).collect();
            host_io::delete_rows(layer, &rows)?;
            let mine: Vec<usize> = select.iter().filter(|(l, _)| *l == layer).map(|(_, r)| *r).collect();
            let moved = host_io::rows_after_delete(&mine, &rows);
            let mut it = moved.into_iter();
            select = select
                .into_iter()
                .filter_map(|(l, r)| if l == layer { it.next().flatten().map(|r| (l, r)) } else { Some((l, r)) })
                .collect();
        }
        for (layer, g, attrs) in &self.add {
            let row = host_io::add_feature(*layer, g, attrs)?;
            select.push((*layer, row));
        }
        let mut by_layer: Vec<(i64, Vec<usize>)> = Vec::new();
        for (l, r) in select {
            match by_layer.iter_mut().find(|(x, _)| *x == l) {
                Some((_, rows)) => rows.push(r),
                None => by_layer.push((l, vec![r])),
            }
        }
        host_io::set_selection(&by_layer)
    }
}
