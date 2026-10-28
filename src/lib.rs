//! Advanced Editing: QGIS-style advanced editing for RustGIS.
//!
//! Commands take their input from the selection (see [`selection`]) and their settings from
//! the plugin's panel (see [`Settings`]); scripts can override any setting in the command's
//! parameters. Each command is one undo step and returns `Err` on any failure, which rolls
//! all of its edits back.

pub mod attributes;
#[cfg(feature = "diagnostics")]
mod diagnostics;
pub mod division;
pub mod geojson;
pub mod host_io;
pub mod lines;
pub mod polygons;
pub mod proj;
pub mod selection;
pub mod topo;

use rustgis_plugin::{Kind, Panel, Request, Value, json};

use host_io::Res;

pub const MANIFEST: &str = include_str!("../plugin.json");

/// What a stub command answers until it is written.
pub const NOT_IMPLEMENTED: &str = "not implemented yet";

/// The distance slider's values in metres, by slider position.
pub const DISTANCES_M: [f64; 25] = [
    0.01, 0.02, 0.05, 0.1, 0.2, 0.25, 0.5, 1.0, 2.0, 2.5, 5.0, 10.0, 15.0, 20.0, 25.0, 30.0, 40.0, 50.0, 75.0,
    100.0, 150.0, 200.0, 250.0, 500.0, 1000.0,
];
/// The tolerance slider's values in metres, by slider position.
pub const TOLERANCES_M: [f64; 16] =
    [0.001, 0.002, 0.005, 0.01, 0.02, 0.05, 0.1, 0.2, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 20.0, 50.0];

/// The panel's values, with script overrides applied.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Settings {
    /// Offset distance, copy-move distance, densify spacing (m, > 0).
    pub distance_m: f64,
    /// Simplify tolerance (m, > 0).
    pub tolerance_m: f64,
    /// Smoothing iterations (1..=5).
    pub iterations: u32,
    /// Number of parts for equal division (2..=50).
    pub parts: u32,
    /// Share of the area to cut off (1..=99 %).
    pub percent: f64,
    /// Direction of division lines, degrees clockwise from north (0..180; 0 = north-south
    /// lines).
    pub angle_deg: f64,
    /// Copy-move direction, degrees clockwise from north (0..360).
    pub bearing_deg: f64,
    /// Keep the tool features (cutter, splitting line) instead of deleting them.
    pub keep_tools: bool,
    /// Offset to the left of the line direction (else to the right).
    pub offset_left: bool,
    /// Work on the whole active layer instead of its selection.
    pub whole_layer: bool,
    /// Copy-move and offset change the original instead of adding a copy.
    pub move_original: bool,
    /// Merge with attributes sums the numeric fields over the merged features.
    pub sum_numbers: bool,
    /// Division parts start on the right of the line direction instead of the left.
    pub reverse: bool,
    /// New vertices on an edge shared with a feature that is not edited are added to that
    /// feature too (see [`topo`]).
    pub topological: bool,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            distance_m: DISTANCES_M[7],
            tolerance_m: TOLERANCES_M[6],
            iterations: 2,
            parts: 2,
            percent: 50.0,
            angle_deg: 0.0,
            bearing_deg: 90.0,
            keep_tools: true,
            offset_left: false,
            whole_layer: false,
            move_original: false,
            sum_numbers: false,
            reverse: false,
            topological: true,
        }
    }
}

fn pick(table: &[f64], idx: Option<i64>, default: f64) -> f64 {
    idx.and_then(|i| usize::try_from(i).ok()).and_then(|i| table.get(i).copied()).unwrap_or(default)
}

impl Settings {
    /// Panel values first, then parameters a script passed (`distance`, `tolerance` in
    /// metres, `iterations`, `parts`, `percent`, `angle`, `bearing`, `keepTools`, `left`, `wholeLayer`,
    /// `move`, `sum`, `reverse`, `topological`). `params` may be `null` (a ribbon button).
    pub fn from_request(req: &Request) -> Settings {
        let d = Settings::default();
        let int = |id: &str, lo: i64, hi: i64, def: i64| req.panel_i64(id).unwrap_or(def).clamp(lo, hi);
        let mut s = Settings {
            distance_m: pick(&DISTANCES_M, req.panel_i64("distance"), d.distance_m),
            tolerance_m: pick(&TOLERANCES_M, req.panel_i64("tolerance"), d.tolerance_m),
            iterations: int("iterations", 1, 5, d.iterations as i64) as u32,
            parts: int("parts", 2, 50, d.parts as i64) as u32,
            percent: int("percent", 1, 99, d.percent as i64) as f64,
            angle_deg: int("angle", 0, 179, d.angle_deg as i64) as f64,
            bearing_deg: int("bearing", 0, 359, d.bearing_deg as i64) as f64,
            keep_tools: req.panel_bool("keep-tools").unwrap_or(d.keep_tools),
            offset_left: req.panel_bool("offset-left").unwrap_or(d.offset_left),
            whole_layer: req.panel_bool("whole-layer").unwrap_or(d.whole_layer),
            move_original: req.panel_bool("move-original").unwrap_or(d.move_original),
            sum_numbers: req.panel_bool("merge-sum").unwrap_or(d.sum_numbers),
            reverse: req.panel_bool("divide-reverse").unwrap_or(d.reverse),
            topological: req.panel_bool("topological").unwrap_or(d.topological),
        };
        if req.params.is_object() {
            let pos = |x: f64| (x > 0.0).then_some(x);
            if let Some(x) = req.f64("distance").and_then(pos) {
                s.distance_m = x;
            }
            if let Some(x) = req.f64("tolerance").and_then(pos) {
                s.tolerance_m = x;
            }
            if let Some(x) = req.f64("iterations") {
                s.iterations = x.round().clamp(1.0, 10.0) as u32;
            }
            if let Some(x) = req.f64("parts") {
                s.parts = x.round().clamp(2.0, 1000.0) as u32;
            }
            if let Some(x) = req.f64("percent") {
                s.percent = x.clamp(0.01, 99.99);
            }
            if let Some(x) = req.f64("angle") {
                s.angle_deg = x.rem_euclid(180.0);
            }
            if let Some(x) = req.f64("bearing") {
                s.bearing_deg = x.rem_euclid(360.0);
            }
            for (key, slot) in [
                ("keepTools", &mut s.keep_tools),
                ("left", &mut s.offset_left),
                ("wholeLayer", &mut s.whole_layer),
                ("move", &mut s.move_original),
                ("sum", &mut s.sum_numbers),
                ("reverse", &mut s.reverse),
                ("topological", &mut s.topological),
            ] {
                if let Some(b) = req.bool(key) {
                    *slot = b;
                }
            }
        }
        s
    }
}

/// What every command gets.
pub struct Ctx<'a> {
    pub req: &'a Request,
    pub settings: Settings,
}

impl Ctx<'_> {
    /// `s` in the interface language (from `plugin.json`'s `i18n`), `{name}`s filled.
    pub fn tr(&self, s: &str, args: &[(&str, &str)]) -> String {
        self.req.tr(MANIFEST, s, args)
    }

    /// Ends a successful command: shows `message` in the status bar and the panel, and
    /// answers `counts` (an object of numbers, e.g. `{"split": 3, "created": 6}`) with the
    /// message added.
    pub fn finish(&self, message: &str, counts: Value) -> Res<Value> {
        host_io::notify(message)?;
        let mut answer = match counts {
            Value::Object(m) => Value::Object(m),
            Value::Null => json!({}),
            other => json!({ "result": other }),
        };
        answer["message"] = json!(message);
        Ok(Panel::new().text("status", message).answer(answer))
    }
}

/// A stub's answer.
pub fn not_implemented(id: &str) -> Res<Value> {
    Err(format!("{id}: {NOT_IMPLEMENTED}"))
}

type CommandFn = fn(&Ctx) -> Res<Value>;

/// Every command id of `plugin.json` and its function.
pub const COMMANDS: &[(&str, CommandFn)] = &[
    ("cut-with-polygon", polygons::cut_with_polygon),
    ("clip-with-polygon", polygons::clip_with_polygon),
    ("add-ring", polygons::add_ring),
    ("delete-ring", polygons::delete_ring),
    ("fill-ring", polygons::fill_ring),
    ("add-part", polygons::add_part),
    ("delete-part", polygons::delete_part),
    ("split-off-part", polygons::split_off_part),
    ("fill-gap", polygons::fill_gap),
    ("split-with-line", lines::split_with_line),
    ("reshape", lines::reshape),
    ("offset", lines::offset),
    ("trim-extend", lines::trim_extend),
    ("merge-with-attributes", attributes::merge_with_attributes),
    ("exchange-attributes", attributes::exchange_attributes),
    ("copy-move", attributes::copy_move),
    ("simplify", attributes::simplify),
    ("densify", attributes::densify),
    ("smooth", attributes::smooth),
    ("divide-equal", division::divide_equal),
    ("divide-percent", division::divide_percent),
    ("panel-state", panel_state),
];

fn fmt_m(x: f64) -> String {
    format!("{x} m")
}

/// The panel's state command: gives the stepped sliders their labels in metres.
fn panel_state(ctx: &Ctx) -> Res<Value> {
    let labels = |t: &[f64]| t.iter().map(|x| fmt_m(*x)).collect::<Vec<_>>();
    let s = ctx.settings;
    let text = format!(
        "Distance {}, tolerance {}, {} parts, {} %, division {}°, bearing {}°",
        fmt_m(s.distance_m),
        fmt_m(s.tolerance_m),
        s.parts,
        s.percent,
        s.angle_deg,
        s.bearing_deg
    );
    Ok(Panel::new()
        .slider("distance", 0, DISTANCES_M.len() as i64 - 1, &labels(&DISTANCES_M))
        .slider("tolerance", 0, TOLERANCES_M.len() as i64 - 1, &labels(&TOLERANCES_M))
        .text("status", text)
        .answer(Value::Null))
}

fn handle(req: Request) -> Res<Value> {
    match req.kind {
        Kind::Tool => match req.id.as_str() {
            "divide-parcel" => division::divide_parcel_tool(&req),
            other => Err(format!("unknown tool '{other}'")),
        },
        Kind::Command => {
            #[cfg(feature = "diagnostics")]
            if let Some(r) = diagnostics::dispatch(&req) {
                return r;
            }
            let f = COMMANDS
                .iter()
                .find(|(id, _)| *id == req.id)
                .map(|(_, f)| *f)
                .ok_or_else(|| format!("unknown command '{}'", req.id))?;
            let ctx = Ctx { settings: Settings::from_request(&req), req: &req };
            f(&ctx)
        }
    }
}

rustgis_plugin::export!(handle);
