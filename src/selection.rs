//! The interaction model shared by all commands. The ribbon cannot pass parameters and the
//! plugin cannot take map clicks, so the selection says what to work on:
//!
//! - **Tool features**: the selected features of the ACTIVE layer (the cutter polygon, the
//!   splitting line, the locator point).
//! - **Target features**: the selected features of the OTHER layers. When no other layer has
//!   a selection (or the "whole layer" option is on), the targets are the active layer's
//!   features that intersect the tool features, the tools themselves excluded.
//!
//! Commands without tool features (simplify, delete ring…) edit the selected features of the
//! active layer instead, see [`active_features`].

use geo::{BoundingRect, CoordsIter, Coord, Intersects, MapCoordsInPlace, Rect, Translate};

use crate::geojson::{Dim, Geom, bboxes_intersect};
use crate::host_io::{self, Feature, LAYER_READ_CAP, LayerInfo, Query, Res, budget};
use crate::proj::Frame;

/// What a tool-based command needs from the selection.
#[derive(Clone, Copy, Debug)]
pub struct ToolSpec {
    /// The instruction shown when the selection doesn't fit, e.g. "Select one line in the
    /// active layer and the polygons to split in another layer".
    pub usage: &'static str,
    /// Allowed dimensions of the tool features (the active layer's geometry type).
    pub tool_dims: &'static [Dim],
    pub min_tools: usize,
    pub max_tools: Option<usize>,
    /// Allowed dimensions of the target layers.
    pub target_dims: &'static [Dim],
    /// Fail when no target is found.
    pub need_targets: bool,
    /// Keep the field values of the tools / targets.
    pub tool_properties: bool,
    pub target_properties: bool,
    /// Take the targets from the active layer even when other layers have a selection
    /// (the panel's "Use whole active layer as targets").
    pub whole_layer: bool,
}

impl ToolSpec {
    /// A spec with one or more tools, targets required, no properties read.
    pub const fn new(usage: &'static str, tool_dims: &'static [Dim], target_dims: &'static [Dim]) -> ToolSpec {
        ToolSpec {
            usage,
            tool_dims,
            min_tools: 1,
            max_tools: None,
            target_dims,
            need_targets: true,
            tool_properties: false,
            target_properties: false,
            whole_layer: false,
        }
    }
}

/// The target features of one layer.
#[derive(Clone, Debug)]
pub struct TargetSet {
    pub layer: LayerInfo,
    pub features: Vec<Feature>,
}

/// The selection resolved into tools and targets.
#[derive(Clone, Debug)]
pub struct Selection {
    /// `host.layers` as read at the start of the command.
    pub layers: Vec<LayerInfo>,
    pub active: LayerInfo,
    pub tools: Vec<Feature>,
    /// One entry per layer with targets (the active layer itself when
    /// `targets_from_active`).
    pub targets: Vec<TargetSet>,
    pub targets_from_active: bool,
    /// What reading the tools and targets took of the command's reads, for its further reads.
    pub read: budget::ReadAllowance,
}

impl Selection {
    pub fn target_count(&self) -> usize {
        self.targets.iter().map(|t| t.features.len()).sum()
    }

    /// Every tool and target geometry (lon/lat).
    pub fn geoms(&self) -> impl Iterator<Item = &Geom> {
        self.tools
            .iter()
            .chain(self.targets.iter().flat_map(|t| t.features.iter()))
            .filter_map(|f| f.geometry.as_ref())
    }

    /// The metric frame for all tools and targets (see [`Frame::for_positions`]).
    pub fn frame(&self) -> Res<Frame> {
        Frame::for_geoms(self.geoms())
    }

    /// Selects the given rows per layer (e.g. the results), replacing the selection.
    pub fn select_results(results: &[(i64, Vec<usize>)]) -> Res<()> {
        host_io::set_selection(results)
    }
}

fn dims_text(dims: &[Dim]) -> String {
    let v: Vec<String> = dims.iter().map(|d| format!("{}s", d.noun())).collect();
    v.join(" or ")
}

fn misfit(usage: &str, why: String) -> String {
    format!("{usage}. ({why})")
}

fn read_selected(layer: &LayerInfo, properties: bool, read: &mut budget::ReadAllowance) -> Res<Vec<Feature>> {
    let q = Query { selected: true, limit: None, properties };
    host_io::features_within(layer.id, q, &format!("the {} selected features of '{}'", layer.selected, layer.name), read)
}

/// Resolves the selection into tool and target features, or fails with the spec's usage
/// line and what was wrong.
pub fn gather(spec: &ToolSpec) -> Res<Selection> {
    let layers = host_io::layers()?;
    let active = host_io::active_layer(&layers).map_err(|e| misfit(spec.usage, e))?.clone();
    let adim = active
        .dim
        .filter(|_| !active.raster)
        .ok_or_else(|| misfit(spec.usage, format!("the active layer '{}' has no vector features", active.name)))?;
    if !spec.tool_dims.contains(&adim) {
        return Err(misfit(
            spec.usage,
            format!("the active layer '{}' holds {}s, the tool must be {}", active.name, adim.noun(), dims_text(spec.tool_dims)),
        ));
    }
    if active.selected == 0 {
        return Err(misfit(spec.usage, format!("nothing is selected in the active layer '{}'", active.name)));
    }
    if active.selected > LAYER_READ_CAP {
        return Err(misfit(spec.usage, format!("{} features are selected in '{}'", active.selected, active.name)));
    }
    // the tools and every target layer are read within one allowance
    let mut read = budget::ReadAllowance::new();
    let tools: Vec<Feature> = read_selected(&active, spec.tool_properties, &mut read)?
        .into_iter()
        .filter(|f| f.geometry.is_some())
        .collect();
    if tools.len() < spec.min_tools || spec.max_tools.is_some_and(|m| tools.len() > m) {
        return Err(misfit(
            spec.usage,
            format!("{} {} feature(s) selected in the active layer '{}'", tools.len(), adim.noun(), active.name),
        ));
    }

    let others: Vec<&LayerInfo> =
        layers.iter().filter(|l| l.id != active.id && l.is_vector() && l.selected > 0).collect();
    let mut targets = Vec::new();
    let from_active = spec.whole_layer || others.is_empty();
    if !from_active {
        for l in others {
            let d = l.dim.unwrap();
            if !spec.target_dims.contains(&d) {
                return Err(misfit(
                    spec.usage,
                    format!("'{}' holds {}s, the targets must be {}", l.name, d.noun(), dims_text(spec.target_dims)),
                ));
            }
            if l.selected > LAYER_READ_CAP {
                return Err(misfit(spec.usage, format!("{} features are selected in '{}'", l.selected, l.name)));
            }
            let feats: Vec<Feature> = read_selected(l, spec.target_properties, &mut read)?
                .into_iter()
                .filter(|f| f.geometry.is_some())
                .collect();
            if !feats.is_empty() {
                targets.push(TargetSet { layer: l.clone(), features: feats });
            }
        }
    } else if spec.target_dims.contains(&adim) {
        let feats = intersecting(&active, &tools, spec.target_properties, &mut read)?;
        if !feats.is_empty() {
            targets.push(TargetSet { layer: active.clone(), features: feats });
        }
    }
    // the tools are projected too, and some commands rewrite them
    let edited: usize = targets.iter().map(|t| t.features.len()).sum();
    let vertices = budget::vertices(&tools) + targets.iter().map(|t| budget::vertices(&t.features)).sum::<usize>();
    budget::check_edit(edited, vertices)?;
    if spec.need_targets && targets.is_empty() {
        let why = if from_active {
            format!(
                "none of the {} of '{}' touch the selected tool features, and no other layer has a selection",
                dims_text(spec.target_dims),
                active.name
            )
        } else {
            "the selected features in the other layers have no geometry".to_string()
        };
        return Err(misfit(spec.usage, why));
    }
    Ok(Selection { layers, active, tools, targets, targets_from_active: from_active, read })
}

/// Features of `layer` (all of it, capped) intersecting any of `tools`, the tools excluded.
/// Each geometry is taken in one piece across the antimeridian (see [`in_one_piece`]) and
/// compared with the tools also a turn east and west, so a tool at 179.95°E finds a target
/// running from 179.9°E to 179.9°W.
fn intersecting(
    layer: &LayerInfo,
    tools: &[Feature],
    properties: bool,
    read: &mut budget::ReadAllowance,
) -> Res<Vec<Feature>> {
    let tool_geo: Vec<(Rect<f64>, geo::Geometry<f64>)> = tools
        .iter()
        .filter_map(|t| t.geometry.as_ref())
        .filter_map(|g| {
            let (bb, tg) = in_one_piece(g)?;
            Some((bb, tg.unwrap_or_else(|| g.to_geo())))
        })
        .collect();
    let all = host_io::all_features_capped_within(layer, LAYER_READ_CAP, properties, read)?;
    Ok(all
        .into_iter()
        .filter(|f| !tools.iter().any(|t| t.id == f.id))
        .filter(|f| {
            let Some(g) = f.geometry.as_ref() else { return false };
            let Some((bb, fg)) = in_one_piece(g) else { return false };
            let shifted = |turn: f64| Rect::new(bb.min() + (turn, 0.0).into(), bb.max() + (turn, 0.0).into());
            let mut cand = tool_geo
                .iter()
                .flat_map(|(tb, tg)| TURNS.iter().filter(|&&d| bboxes_intersect(tb, &shifted(d))).map(move |&d| (d, tg)))
                .peekable();
            if cand.peek().is_none() {
                return false;
            }
            let fg = fg.unwrap_or_else(|| g.to_geo());
            cand.any(|(d, tg)| if d == 0.0 { tg.intersects(&fg) } else { tg.intersects(&fg.translate(d, 0.0)) })
        })
        .collect())
}

/// Longitude shifts under which two geometries in one piece may meet.
pub(crate) const TURNS: [f64; 3] = [0.0, 360.0, -360.0];

/// The lon/lat box of `g` with its longitudes taken within 180° of its first one, so that a
/// geometry across the antimeridian is one piece (its box may then reach beyond 180°), and
/// that geometry when it differs from `g` (`None`: use `g` as it is).
pub(crate) fn in_one_piece(g: &Geom) -> Option<(Rect<f64>, Option<geo::Geometry<f64>>)> {
    let bb = g.bbox()?;
    // all longitudes within 180° of each other: already in one piece
    if bb.width() < 180.0 {
        return Some((bb, None));
    }
    let mut out = g.to_geo();
    let lon0 = out.coords_iter().next()?.x;
    out.map_coords_in_place(|c| Coord { x: c.x + 360.0 * ((lon0 - c.x) / 360.0).round(), y: c.y });
    Some((out.bounding_rect()?, Some(out)))
}

/// What a command without tool features needs: the features it edits.
#[derive(Clone, Copy, Debug)]
pub struct EditSpec {
    /// e.g. "Select the polygons whose holes to delete in the active layer".
    pub usage: &'static str,
    pub dims: &'static [Dim],
    pub min: usize,
    pub max: Option<usize>,
    pub properties: bool,
    /// Edit every feature of the active layer (capped) instead of its selection.
    pub whole_layer: bool,
}

impl EditSpec {
    pub const fn new(usage: &'static str, dims: &'static [Dim]) -> EditSpec {
        EditSpec { usage, dims, min: 1, max: None, properties: false, whole_layer: false }
    }
}

/// The features a command edits, all from the active layer.
#[derive(Clone, Debug)]
pub struct Editable {
    pub layers: Vec<LayerInfo>,
    pub layer: LayerInfo,
    pub features: Vec<Feature>,
    /// What reading them took of the command's reads, for the command's further reads.
    pub read: budget::ReadAllowance,
}

impl Editable {
    pub fn frame(&self) -> Res<Frame> {
        Frame::for_geoms(self.features.iter().filter_map(|f| f.geometry.as_ref()))
    }
}

/// The selected features of the active layer (or all of them with `whole_layer`).
pub fn active_features(spec: &EditSpec) -> Res<Editable> {
    let layers = host_io::layers()?;
    let layer = host_io::active_layer(&layers).map_err(|e| misfit(spec.usage, e))?.clone();
    let dim = layer
        .dim
        .filter(|_| !layer.raster)
        .ok_or_else(|| misfit(spec.usage, format!("the active layer '{}' has no vector features", layer.name)))?;
    if !spec.dims.contains(&dim) {
        return Err(misfit(
            spec.usage,
            format!("the active layer '{}' holds {}s, this works on {}", layer.name, dim.noun(), dims_text(spec.dims)),
        ));
    }
    let mut read = budget::ReadAllowance::new();
    let features: Vec<Feature> = if spec.whole_layer {
        host_io::all_features_capped_within(&layer, LAYER_READ_CAP, spec.properties, &mut read)?
    } else {
        if layer.selected == 0 {
            return Err(misfit(spec.usage, format!("nothing is selected in the active layer '{}'", layer.name)));
        }
        if layer.selected > LAYER_READ_CAP {
            return Err(misfit(spec.usage, format!("{} features are selected in '{}'", layer.selected, layer.name)));
        }
        read_selected(&layer, spec.properties, &mut read)?
    };
    let features: Vec<Feature> = features.into_iter().filter(|f| f.geometry.is_some()).collect();
    budget::check_edit(features.len(), budget::vertices(&features))?;
    if features.len() < spec.min || spec.max.is_some_and(|m| features.len() > m) {
        return Err(misfit(
            spec.usage,
            format!("{} {} feature(s) to work on in the active layer '{}'", features.len(), dim.noun(), layer.name),
        ));
    }
    Ok(Editable { layers, layer, features, read })
}
