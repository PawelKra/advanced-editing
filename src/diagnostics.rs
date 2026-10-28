//! Commands for the test harness (built only with `--features diagnostics`), exercising the
//! shared modules against the real host.

use rustgis_plugin::{Map, Request, Value, json};

use crate::geojson::{self, Dim};
use crate::host_io::{self, Query, Res, budget};
use crate::proj::{Frame, FrameKind, GRS80_A, GRS80_F, SNAP_TOL_M, VertexSnap, WGS84_F};
use crate::selection::{self, EditSpec, ToolSpec};

pub fn dispatch(req: &Request) -> Option<Res<Value>> {
    let p = &req.params;
    Some(match req.id.as_str() {
        "diag-proj" => proj(p),
        "diag-geojson" => geojson_check(p),
        "diag-snap" => snap(p),
        "diag-features" => features(p),
        "diag-selection" => sel(p),
        "diag-delete" => delete(p),
        "diag-add" => add(p),
        "diag-settings" => Ok(settings(req)),
        "diag-layers" => layers(),
        "diag-bench" => bench(p),
        _ => return None,
    })
}

fn points(v: &Value) -> Vec<[f64; 2]> {
    v.as_array()
        .map(|a| a.iter().filter_map(|p| Some([p.get(0)?.as_f64()?, p.get(1)?.as_f64()?])).collect())
        .unwrap_or_default()
}

/// Largest disagreement with [`RefTm`] in metres over an `n` x `n` grid of the frame's area
/// (EPSG:2180: Poland; UTM: ±3° around the central meridian, `lat` ± 6°): forward, inverse,
/// and the round trip of the fast projection.
fn proj_grid(p: &Value) -> Res<Value> {
    let n = p["n"].as_u64().unwrap_or(40) as usize;
    let lon0 = p["lon"].as_f64().unwrap_or(19.0);
    let lat0 = p["lat"].as_f64().unwrap_or(52.0);
    let f = Frame::for_positions([[lon0, lat0]])?;
    let r = RefTm::for_frame(&f);
    let (w, s, e, nn) = match f.kind {
        FrameKind::Epsg2180 => (14.0, 49.0, 24.2, 54.9),
        FrameKind::Utm { zone, .. } => {
            let cm = zone as f64 * 6.0 - 183.0;
            (cm - 3.0, (lat0 - 6.0).max(-84.0), cm + 3.0, (lat0 + 6.0).min(84.0))
        }
    };
    let (mut fwd, mut inv, mut rt) = (0.0f64, 0.0f64, 0.0f64);
    for i in 0..=n {
        for j in 0..=n {
            let lon = w + (e - w) * i as f64 / n as f64;
            let lat = s + (nn - s) * j as f64 / n as f64;
            let (x, y) = f.fwd(lon, lat);
            let (xr, yr) = r.forward(lon, lat);
            fwd = fwd.max((x - xr).hypot(y - yr));
            let k = 111_320.0;
            let (lo, la) = f.inv(xr, yr);
            let (lor, lar) = r.inverse(xr, yr);
            inv = inv.max(((lo - lor) * k * lat.to_radians().cos()).hypot((la - lar) * k));
            let (x2, y2) = f.fwd(lo, la);
            rt = rt.max((x2 - xr).hypot(y2 - yr));
        }
    }
    Ok(json!({"frame": f.name(), "points": (n + 1) * (n + 1), "fwd_m": fwd, "inv_m": inv, "rt_m": rt}))
}

fn proj(p: &Value) -> Res<Value> {
    if p["grid"].as_bool() == Some(true) {
        return proj_grid(p);
    }
    let pts = points(&p["points"]);
    let mut out = Vec::new();
    for &[lon, lat] in &pts {
        let f = Frame::for_positions([[lon, lat]])?;
        let (x, y) = f.fwd(lon, lat);
        let (lon2, lat2) = f.inv(x, y);
        let r = RefTm::for_frame(&f);
        let (xr, yr) = r.forward(lon, lat);
        let (lonr, latr) = r.inverse(x, y);
        out.push(json!({"frame": f.name(), "x": x, "y": y, "lon": lon2, "lat": lat2,
            "rt_err_deg": (lon2 - lon).abs().max((lat2 - lat).abs()),
            "x_ref": xr, "y_ref": yr, "lon_ref": lonr, "lat_ref": latr}));
    }
    let all = Frame::for_positions(pts.iter().copied()).map(|f| f.name());
    Ok(json!({"points": out, "frame_all": all.unwrap_or_else(|e| format!("error: {e}"))}))
}

/// `count` pseudo-random f64 per magnitude band: [`geojson::write_f64`] must give serde_json's
/// text, and [`geojson::JsonReader`] must parse it back to the same bits.
fn numbers_check(p: &Value) -> Res<Value> {
    let count = p["count"].as_u64().unwrap_or(1000) as usize;
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let (mut n, mut text_diff, mut parse_diff, mut examples) = (0usize, 0usize, 0usize, Vec::new());
    let bands = [1e-4, 1e-3, 0.01, 1.0, 10.0, 52.0, 180.0, 1e3, 5e5, 6e6, 1e9, 9e15, 1e17];
    for &scale in &bands {
        for i in 0..count {
            let r = next();
            let x = match i % 4 {
                // full-precision values
                0 | 1 => scale * (1.0 + (r >> 11) as f64 / (1u64 << 53) as f64),
                // short decimals, as typed coordinates are
                2 => ((r % 10_000_000) as f64 / 1e4).round() / 1e3 * scale.max(1.0),
                // neighbours of powers of two and integers
                _ => f64::from_bits((scale.to_bits() & !0xf) + (r & 0x1f) - 8),
            };
            let x = if r & (1 << 63) != 0 { -x } else { x };
            let mut mine = String::new();
            geojson::write_f64(&mut mine, x);
            let theirs = rustgis_plugin::serde_json::to_string(&x).unwrap();
            let back = geojson::JsonReader::new(mine.as_bytes()).number()?;
            let core: f64 = theirs.parse().unwrap_or(f64::NAN);
            n += 1;
            text_diff += (mine != theirs) as usize;
            parse_diff += (back.to_bits() != x.to_bits() || core.to_bits() != x.to_bits()) as usize;
            if (mine != theirs || back.to_bits() != x.to_bits()) && examples.len() < 10 {
                examples.push(json!([mine, theirs, back]));
            }
        }
    }
    Ok(json!({"n": n, "text_diff": text_diff, "parse_diff": parse_diff, "examples": examples}))
}

fn geojson_check(p: &Value) -> Res<Value> {
    if p["numbers"].as_bool() == Some(true) {
        return numbers_check(p);
    }
    let g = geojson::from_geojson(&p["geometry"])?;
    let valid = geojson::is_valid_small(&g, geojson::MAX_VALIDATE_VERTICES);
    let out = geojson::prepare_output(g)?;
    Ok(json!({"geometry": geojson::to_geojson(&out), "valid": valid}))
}

/// Round trip through a VertexSnap, with one vertex nudged by `nudge` metres: untouched
/// vertices must come back bit-exact, the nudged one must not snap when beyond tolerance.
fn snap(p: &Value) -> Res<Value> {
    let g = geojson::from_geojson(&p["geometry"])?;
    let nudge = p["nudge"].as_f64().unwrap_or(0.0);
    let frame = Frame::for_geoms([&g])?;
    let mut s = VertexSnap::new(frame, SNAP_TOL_M);
    let m = s.project(&g);
    let mut v = Vec::new();
    m.for_each_coord(|c| v.push(c));
    let target = v[0];
    // every copy of the first vertex moves (the ring's closing one too)
    let m = m.map_coords(|c| if c == target { geo::Coord { x: c.x + nudge, y: c.y } } else { c });
    let back = s.unproject(&m);
    let plain = frame.unproject(&frame.project(&g));
    let (mut a, mut b, mut c) = (Vec::new(), Vec::new(), Vec::new());
    g.for_each_coord(|x| a.push(x));
    back.for_each_coord(|x| b.push(x));
    plain.for_each_coord(|x| c.push(x));
    let exact = a.iter().zip(&b).filter(|(x, y)| x == y).count();
    let plain_exact = a.iter().zip(&c).filter(|(x, y)| x == y).count();
    let first = a[0] == b[0];
    Ok(json!({"n": a.len(), "exact": exact, "plain_exact": plain_exact, "first_exact": first, "frame": frame.name()}))
}

fn features(p: &Value) -> Res<Value> {
    let layer = p["layer"].as_i64().ok_or("layer id needed")?;
    let q = Query {
        selected: p["selected"].as_bool().unwrap_or(false),
        limit: p["limit"].as_u64().map(|n| n as usize),
        properties: p["properties"].as_bool().unwrap_or(false),
    };
    let f = host_io::features(layer, q)?;
    // write every geometry back unchanged: the text written must parse to the same numbers
    if p["rewrite"].as_bool() == Some(true) {
        for x in &f {
            host_io::set_geometry(layer, x.row, x.geom()?)?;
        }
    }
    Ok(json!({
        "geometries": f.iter().map(|f| f.geometry.as_ref().map(geojson::to_geojson)).collect::<Vec<_>>(),
        "ids": f.iter().map(|f| f.id).collect::<Vec<_>>(),
        "rows": f.iter().map(|f| f.row).collect::<Vec<_>>(),
        "types": f.iter().map(|f| f.geometry.as_ref().map(|g| g.type_name())).collect::<Vec<_>>(),
        "props": f.iter().map(|f| Value::Object(f.properties.clone())).collect::<Vec<_>>(),
    }))
}

fn dims(v: &Value) -> &'static [Dim] {
    let s: Vec<&str> = v.as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
    match (s.contains(&"point"), s.contains(&"line"), s.contains(&"polygon")) {
        (true, true, true) => &[Dim::Point, Dim::Line, Dim::Polygon],
        (false, true, true) => &[Dim::Line, Dim::Polygon],
        (true, false, true) => &[Dim::Point, Dim::Polygon],
        (true, true, false) => &[Dim::Point, Dim::Line],
        (true, false, false) => &[Dim::Point],
        (false, true, false) => &[Dim::Line],
        _ => &[Dim::Polygon],
    }
}

fn sel(p: &Value) -> Res<Value> {
    let whole = p["whole"].as_bool().unwrap_or(false);
    if p["mode"].as_str() == Some("edit") {
        let mut spec = EditSpec::new("Select features to edit in the active layer", dims(&p["dims"]));
        spec.whole_layer = whole;
        let e = selection::active_features(&spec)?;
        return Ok(json!({"layer": e.layer.name, "ids": e.features.iter().map(|f| f.id).collect::<Vec<_>>(),
            "frame": e.frame()?.name()}));
    }
    let mut spec = ToolSpec::new(
        "Select the tool features in the active layer and the targets in another layer",
        dims(&p["toolDims"]),
        dims(&p["targetDims"]),
    );
    spec.whole_layer = whole;
    spec.max_tools = p["maxTools"].as_u64().map(|n| n as usize);
    let s = selection::gather(&spec)?;
    Ok(json!({
        "active": s.active.name,
        "tools": s.tools.iter().map(|f| f.id).collect::<Vec<_>>(),
        "targets": s.targets.iter().map(|t| json!({"layer": t.layer.name, "ids": t.features.iter().map(|f| f.id).collect::<Vec<_>>()})).collect::<Vec<_>>(),
        "fromActive": s.targets_from_active,
        "frame": s.frame()?.name(),
    }))
}

fn usizes(v: &Value) -> Vec<usize> {
    v.as_array().map(|a| a.iter().filter_map(Value::as_u64).map(|n| n as usize).collect()).unwrap_or_default()
}

fn delete(p: &Value) -> Res<Value> {
    let layer = p["layer"].as_i64().ok_or("layer id needed")?;
    let n = host_io::delete_rows(layer, &usizes(&p["rows"]))?;
    // then select `select`: [[layer, [rows]], ...], replacing the selection
    if let Some(sel) = p["select"].as_array() {
        let sel: Vec<(i64, Vec<usize>)> =
            sel.iter().map(|e| (e[0].as_i64().unwrap_or(-1), usizes(&e[1]))).collect();
        host_io::set_selection(&sel)?;
    }
    Ok(json!({"deleted": n}))
}

/// Adds `count` copies of row 0 (shifted east), deletes `deleteRows`, and compares the rows
/// predicted by `rows_after_delete` with the rows the host reports for the new ids.
fn add(p: &Value) -> Res<Value> {
    let layer_id = p["layer"].as_i64().ok_or("layer id needed")?;
    let count = p["count"].as_u64().unwrap_or(2) as usize;
    let first = host_io::features(layer_id, Query { selected: false, limit: Some(1), properties: true })?;
    let src = first.first().ok_or("empty layer")?;
    let g = src.geom()?.clone();
    let mut rows = Vec::new();
    let mut ids = Vec::new();
    for i in 0..count {
        let d = 0.01 * (i + 1) as f64;
        let moved = geojson::prepare_output(g.map_coords(|c| geo::Coord { x: c.x + d, y: c.y }))?;
        let mut attrs = Map::new();
        attrs.insert("name".into(), json!(format!("added-{i}")));
        let row = host_io::add_feature(layer_id, &moved, &attrs)?;
        let sel = host_io::selected_features(layer_id, false)?;
        if sel.len() != 1 || sel[0].row != row {
            return Err(format!("after add the selection is {:?}, expected row {row}", sel.iter().map(|f| f.row).collect::<Vec<_>>()));
        }
        rows.push(row);
        ids.push(sel[0].id);
    }
    let del = usizes(&p["deleteRows"]);
    host_io::delete_rows(layer_id, &del)?;
    let predicted = host_io::rows_after_delete(&rows, &del);
    let layer = host_io::layer_info_by_id(layer_id)?;
    let actual = host_io::resolve_rows(&layer, &ids, host_io::LAYER_READ_CAP)?;
    Ok(json!({"rows": rows, "ids": ids, "predicted": predicted, "actual": actual, "features": layer.features}))
}

fn settings(req: &Request) -> Value {
    let s = crate::Settings::from_request(req);
    json!({"distance": s.distance_m, "tolerance": s.tolerance_m, "iterations": s.iterations, "parts": s.parts,
        "percent": s.percent, "angle": s.angle_deg, "bearing": s.bearing_deg, "keepTools": s.keep_tools,
        "left": s.offset_left, "wholeLayer": s.whole_layer, "move": s.move_original, "sum": s.sum_numbers,
        "reverse": s.reverse, "topological": s.topological, "paramsNull": req.params.is_null()})
}

fn layers() -> Res<Value> {
    Ok(Value::Array(
        host_io::layers()?
            .into_iter()
            .map(|l| {
                json!({"id": l.id, "name": l.name, "geometry": l.geometry, "dim": l.dim.map(|d| d.noun()),
                    "active": l.active, "features": l.features, "selected": l.selected,
                    "fields": l.fields.iter().map(|f| json!({"name": f.name, "type": f.kind})).collect::<Vec<_>>()})
            })
            .collect(),
    ))
}

/// A closed ring of `n` vertices around (19, 52) with full-precision coordinates.
fn bench_ring(n: usize, r_m: f64) -> geojson::Geom {
    let mut v: Vec<geo::Coord<f64>> = (0..n.max(3))
        .map(|i| {
            let t = i as f64 * std::f64::consts::TAU / n.max(3) as f64;
            geo::Coord { x: 19.0 + r_m * t.cos() / 68_500.0, y: 52.0 + r_m * t.sin() / 111_320.0 }
        })
        .collect();
    v.push(v[0]);
    geojson::Geom::Polygon(geo::Polygon::new(geo::LineString(v), vec![]))
}

/// Repeats one shared operation so the harness can find how much of a command's budget it
/// takes (the largest `n` or `reps` that still finishes). `n` is the vertex count of the test
/// ring (or the number of round trips for "proj"); `geometry` replaces the ring.
fn bench(p: &Value) -> Res<Value> {
    let n = p["n"].as_u64().unwrap_or(100) as usize;
    let reps = p["reps"].as_u64().unwrap_or(1) as usize;
    let layer = p["layer"].as_i64();
    let g = if p["geometry"].is_object() { geojson::from_geojson(&p["geometry"])? } else { bench_ring(n, 50.0) };
    let mut acc = 0.0;
    match p["op"].as_str().unwrap_or("") {
        // the ring alone, to subtract
        "base" => {}
        "proj" => {
            let f = Frame::epsg2180();
            for i in 0..n {
                let (x, y) = f.fwd(19.0 + i as f64 * 1e-4, 52.0);
                let (lon, lat) = f.inv(x, y);
                acc += lon + lat;
            }
        }
        "snap" => {
            for _ in 0..reps {
                let mut s = VertexSnap::new(Frame::epsg2180(), SNAP_TOL_M);
                let m = s.project(&g);
                acc += s.unproject(&m).num_coords() as f64;
            }
        }
        // a geometry to request text through a Value tree, as before, and directly
        "ser-value" => {
            for _ in 0..reps {
                let req = json!({"op": "edit.setGeometry", "params": {"layer": 1, "row": 0, "geometry": geojson::to_geojson(&g)}});
                acc += req.to_string().len() as f64;
            }
        }
        "ser" => {
            for _ in 0..reps {
                let mut req = String::new();
                geojson::write_geojson(&mut req, &g);
                acc += req.len() as f64;
            }
        }
        "parse" => {
            let mut txt = String::new();
            geojson::write_geojson(&mut txt, &g);
            for _ in 0..reps {
                acc += geojson::JsonReader::new(txt.as_bytes()).geometry()?.map_or(0, |g| g.num_coords()) as f64;
            }
        }
        "write" => {
            let layer = layer.ok_or("layer id needed")?;
            for _ in 0..reps {
                host_io::set_geometry(layer, 0, &g)?;
            }
        }
        "read" => {
            let layer = layer.ok_or("layer id needed")?;
            for _ in 0..reps {
                acc += host_io::features(layer, Query::default())?.len() as f64;
            }
        }
        // dividing into `parts` equal parts at `angle`, without reading or writing
        "divide" => {
            let parts = p["parts"].as_u64().unwrap_or(2) as usize;
            let nrm = crate::division::sweep_normal(p["angle"].as_f64().unwrap_or(0.0), false);
            for _ in 0..reps {
                let parcel = crate::division::Parcel::new(&g, crate::division::AreaMode::Ellipsoidal)?;
                acc += parcel.divide(nrm, crate::division::Plan::Equal(parts))?.len() as f64;
            }
        }
        // the offset of a line (or polygon) by `distance`, without reading or writing; `acc`
        // is the fuel it was estimated to take
        "offset" => {
            let d = p["distance"].as_f64().unwrap_or(10.0);
            let m = Frame::for_geoms([&g])?.project(&g);
            for _ in 0..reps {
                let mut fuel = crate::lines::OffsetFuel { left: u64::MAX / 2 };
                crate::lines::offset_geom(&m, d, false, &mut fuel)?;
                acc = (u64::MAX / 2 - fuel.left) as f64;
            }
        }
        // dissolving and joining lines (the parts of a line or multi-line) as Merge With
        // Attributes does, without reading or writing; `acc` is the fuel it was estimated to take
        "merge" => {
            let m = Frame::for_geoms([&g])?.project(&g);
            let parts: Vec<Vec<geo::Coord<f64>>> = m.to_multi_line().ok_or("lines needed")?.0.into_iter().map(|l| l.0).collect();
            for _ in 0..reps {
                let mut fuel = crate::attributes::MergeFuel::new(u64::MAX / 2);
                crate::attributes::merge_metric_lines(parts.clone(), &mut fuel)?;
                acc = (u64::MAX / 2 - fuel.left) as f64;
            }
        }
        // Split With Line ("split") or Cut With Polygon ("cut") of the `targets` (GeoJSON
        // geometries) by the tool `geometry`, without reading or writing, up to "stage":
        // "snap" snapping the tool, else everything before the writes; `acc` is the fuel it
        // was estimated to take
        op @ ("split" | "cut") => {
            let targets = p["targets"].as_array().ok_or("targets needed")?;
            let targets: Vec<geojson::Geom> = targets.iter().map(geojson::from_geojson).collect::<Res<_>>()?;
            let frame = Frame::for_geoms(targets.iter().chain([&g]))?;
            let snap_only = p["stage"].as_str() == Some("snap");
            for _ in 0..reps {
                let mut fuel = budget::Meter::unlimited();
                let mut pl = crate::topo::Placer::new(frame);
                let metric: Vec<geojson::Geom> = targets.iter().map(|t| pl.add_target(t)).collect();
                let metric: Vec<&geojson::Geom> = metric.iter().collect();
                let tool = pl.add_tool(&g);
                if op == "split" {
                    let mut paths = crate::lines::paths_of(&tool);
                    if snap_only {
                        pl.snap_paths(&metric, &mut paths, &mut fuel)?;
                    } else {
                        crate::lines::split_pieces(&mut pl, &metric, paths, &mut fuel)?;
                    }
                } else if snap_only {
                    crate::polygons::snap_tool_polygons(&mut pl, &metric, crate::polygons::polygons_of(&tool).0, &mut fuel)?;
                } else {
                    let results = crate::polygons::cut_results(&mut pl, &metric, crate::polygons::polygons_of(&tool).0, false, &mut fuel)?;
                    let mut new = Vec::new();
                    let mut out = Vec::new();
                    for (owner, (_, r)) in results.into_iter().enumerate() {
                        out.push((owner, pl.unproject(&geojson::Geom::MultiPolygon(r), &mut new, &mut fuel)?));
                    }
                    crate::topo::share_within(frame, out.iter_mut().map(|(o, g)| (*o, g)), &new, &mut fuel)?;
                }
                acc = (u64::MAX / 4 - fuel.left) as f64;
            }
        }
        other => return Err(format!("unknown bench op '{other}'")),
    }
    // `pad` projection round trips on top: the largest `pad` that still finishes measures
    // what the op itself took
    let f = Frame::epsg2180();
    for i in 0..p["pad"].as_u64().unwrap_or(0) {
        let (x, y) = f.fwd(19.0 + i as f64 * 1e-9, 52.0);
        let (lon, lat) = f.inv(x, y);
        acc += lon + lat;
    }
    Ok(json!({"acc": acc}))
}

/// The projection as first written: every series term evaluated on its own. Kept as the
/// reference the faster [`crate::proj::Tm`] must agree with.
#[derive(Clone, Copy, Debug)]
struct RefTm {
    lon0: f64,
    k0: f64,
    fe: f64,
    fn_: f64,
    e: f64,
    a_rect: f64,
    alp: [f64; 6],
    bet: [f64; 6],
}

impl RefTm {
    fn new(a: f64, f: f64, lon0_deg: f64, k0: f64, fe: f64, fn_: f64) -> RefTm {
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
        RefTm { lon0: lon0_deg.to_radians(), k0, fe, fn_, e: (f * (2.0 - f)).sqrt(), a_rect, alp, bet }
    }

    fn for_frame(f: &Frame) -> RefTm {
        match f.kind {
            FrameKind::Epsg2180 => RefTm::new(GRS80_A, GRS80_F, 19.0, 0.9993, 500_000.0, -5_300_000.0),
            FrameKind::Utm { zone, south } => {
                RefTm::new(GRS80_A, WGS84_F, zone as f64 * 6.0 - 183.0, 0.9996, 500_000.0, if south { 1e7 } else { 0.0 })
            }
        }
    }

    /// (lon, lat) in degrees -> (easting, northing) in metres.
    fn forward(&self, lon_deg: f64, lat_deg: f64) -> (f64, f64) {
        let (phi, lam) = (lat_deg.to_radians(), lon_deg.to_radians() - self.lon0);
        let e = self.e;
        let tau = phi.tan();
        let sig = (e * (e * tau / (1.0 + tau * tau).sqrt()).atanh()).sinh();
        let taup = tau * (1.0 + sig * sig).sqrt() - sig * (1.0 + tau * tau).sqrt();
        let xip = taup.atan2(lam.cos());
        let etap = (lam.sin() / (taup * taup + lam.cos() * lam.cos()).sqrt()).asinh();
        let (mut xi, mut eta) = (xip, etap);
        for (j, a) in self.alp.iter().enumerate() {
            let k = 2.0 * (j + 1) as f64;
            xi += a * (k * xip).sin() * (k * etap).cosh();
            eta += a * (k * xip).cos() * (k * etap).sinh();
        }
        (self.fe + self.k0 * self.a_rect * eta, self.fn_ + self.k0 * self.a_rect * xi)
    }

    /// (easting, northing) in metres -> (lon, lat) in degrees.
    fn inverse(&self, x: f64, y: f64) -> (f64, f64) {
        let xi = (y - self.fn_) / (self.k0 * self.a_rect);
        let eta = (x - self.fe) / (self.k0 * self.a_rect);
        let (mut xip, mut etap) = (xi, eta);
        for (j, b) in self.bet.iter().enumerate() {
            let k = 2.0 * (j + 1) as f64;
            xip -= b * (k * xi).sin() * (k * eta).cosh();
            etap -= b * (k * xi).cos() * (k * eta).sinh();
        }
        let taup = xip.sin() / (etap.sinh().powi(2) + xip.cos().powi(2)).sqrt();
        let lam = etap.sinh().atan2(xip.cos());
        let e = self.e;
        let e2m = 1.0 - e * e;
        let mut tau = taup;
        // Newton on tau'(tau) = taup; converges in 2-3 steps for |lat| < 85°.
        for _ in 0..5 {
            let sig = (e * (e * tau / (1.0 + tau * tau).sqrt()).atanh()).sinh();
            let taupi = tau * (1.0 + sig * sig).sqrt() - sig * (1.0 + tau * tau).sqrt();
            let dtau = (taup - taupi) / (1.0 + taupi * taupi).sqrt() * (1.0 + e2m * tau * tau)
                / (e2m * (1.0 + tau * tau).sqrt());
            tau += dtau;
            if dtau.abs() < 1e-14 {
                break;
            }
        }
        ((lam + self.lon0).to_degrees(), tau.atan().to_degrees())
    }
}

