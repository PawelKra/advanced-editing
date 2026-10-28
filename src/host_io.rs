//! Typed access to the host. Every function returns `Result` and every caller must propagate
//! errors with `?`: a failing host call inside a command that still returns `Ok` is NOT rolled
//! back, while a command returning `Err` undoes all of its edits.
//!
//! Layers are referenced by their numeric `id` from `host.layers` (names can repeat).
//! Features are identified by their stable `id`; `row` is their current index and shifts on
//! every delete.

use rustgis_plugin::{Map, Value, json};

use crate::geojson::{self, Dim, Geom, JsonReader};

pub type Res<T> = Result<T, String>;

/// A sanity limit on the features read from a whole layer. What a command can afford is
/// decided by the size of the reply, see [`budget::MAX_READ_BYTES`].
pub const LAYER_READ_CAP: usize = 100_000;

#[link(wasm_import_module = "rustgis")]
unsafe extern "C" {
    #[link_name = "call"]
    fn host_call(ptr: *const u8, len: usize) -> i32;
    #[link_name = "result"]
    fn host_result(ptr: *mut u8);
}

/// Sends a request (a JSON text) and returns the raw reply bytes.
pub fn call_text(op: &str, req: &str) -> Res<Vec<u8>> {
    // SAFETY: the host reads `len` bytes at `ptr`, which `req` owns for the call.
    let n = unsafe { host_call(req.as_ptr(), req.len()) };
    let len = usize::try_from(n).map_err(|_| format!("{op}: RustGIS didn't answer"))?;
    let mut buf: Vec<u8> = Vec::with_capacity(len);
    // SAFETY: the host writes exactly `len` bytes (the reply it measured) into the capacity.
    unsafe {
        host_result(buf.as_mut_ptr());
        buf.set_len(len);
    }
    Ok(buf)
}

/// `host::call` without the SDK's copy of the reply: the `ok` value is moved out, which
/// matters for `host.features` where the reply is the biggest thing a command handles.
pub fn call(op: &str, params: Value) -> Res<Value> {
    let req = json!({"op": op, "params": params}).to_string();
    reply(op, call_text(op, &req)?)
}

fn reply(op: &str, buf: Vec<u8>) -> Res<Value> {
    let mut reply: Value = rustgis_plugin::serde_json::from_slice(&buf).map_err(|e| format!("{op}: unreadable reply: {e}"))?;
    drop(buf);
    if let Some(e) = reply.get("error") {
        return Err(e.as_str().map(str::to_string).unwrap_or_else(|| e.to_string()));
    }
    Ok(reply.get_mut("ok").map(Value::take).unwrap_or(Value::Null))
}

// ---------- layers ----------

#[derive(Clone, Debug)]
pub struct FieldInfo {
    pub name: String,
    /// The host's type name: "Text", "Integer", "Real", "Boolean"…
    pub kind: String,
}

#[derive(Clone, Debug)]
pub struct LayerInfo {
    pub id: i64,
    pub name: String,
    /// The host's geometry name ("Point", "Polyline", "Polygon"; multi layers report the
    /// single name).
    pub geometry: String,
    pub dim: Option<Dim>,
    pub raster: bool,
    pub visible: bool,
    pub active: bool,
    /// Feature count.
    pub features: usize,
    /// Selected feature count.
    pub selected: usize,
    pub fields: Vec<FieldInfo>,
}

impl LayerInfo {
    pub fn is_vector(&self) -> bool {
        !self.raster && self.dim.is_some()
    }

    pub fn has_field(&self, name: &str) -> bool {
        self.fields.iter().any(|f| f.name == name)
    }
}

fn layer_info(v: &Value) -> Res<LayerInfo> {
    let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
    let u = |k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0) as usize;
    let b = |k: &str| v.get(k).and_then(Value::as_bool).unwrap_or(false);
    let geometry = s("geometry");
    Ok(LayerInfo {
        id: v.get("id").and_then(Value::as_i64).ok_or("host.layers: a layer without an id")?,
        name: s("name"),
        dim: Dim::from_layer_type(&geometry),
        geometry,
        raster: b("raster"),
        visible: b("visible"),
        active: b("active"),
        features: u("features"),
        selected: u("selected"),
        fields: v
            .get("fields")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .map(|f| FieldInfo {
                        name: f.get("name").and_then(Value::as_str).unwrap_or_default().into(),
                        kind: f.get("type").and_then(Value::as_str).unwrap_or_default().into(),
                    })
                    .collect()
            })
            .unwrap_or_default(),
    })
}

/// All layers, as `host.layers` lists them (top of the map first).
pub fn layers() -> Res<Vec<LayerInfo>> {
    let v = call("host.layers", json!({}))?;
    v.as_array().ok_or("host.layers: not a list")?.iter().map(layer_info).collect()
}

/// The active layer (the one `edit.*` acts on without `layer`).
pub fn active_layer(layers: &[LayerInfo]) -> Res<&LayerInfo> {
    layers.iter().find(|l| l.active).ok_or_else(|| "No active layer: click a layer in the Layers pane first.".into())
}

pub fn layer_by_id(layers: &[LayerInfo], id: i64) -> Res<&LayerInfo> {
    layers.iter().find(|l| l.id == id).ok_or_else(|| format!("layer {id} is gone"))
}

/// A fresh `host.layers` entry for one layer (counts change after edits).
pub fn layer_info_by_id(id: i64) -> Res<LayerInfo> {
    let all = layers()?;
    layer_by_id(&all, id).cloned()
}

// ---------- features ----------

#[derive(Clone, Debug)]
pub struct Feature {
    /// Stable feature id: survives edits, deletes of other features and undo.
    pub id: i64,
    /// Current index in the layer, valid until the next add/delete.
    pub row: usize,
    /// Lon/lat geometry; `None` when the feature has none or an unsupported type
    /// (GeometryCollection).
    pub geometry: Option<Geom>,
    /// Field values; empty unless asked for (see [`Query::properties`]).
    pub properties: Map<String, Value>,
}

impl Feature {
    pub fn geom(&self) -> Res<&Geom> {
        self.geometry.as_ref().ok_or_else(|| format!("feature {} has no usable geometry", self.id))
    }
}

/// What to read with [`features`].
#[derive(Clone, Copy, Debug, Default)]
pub struct Query {
    /// Only the selected features.
    pub selected: bool,
    /// At most this many features (the first ones, in row order).
    pub limit: Option<usize>,
    /// Keep the field values (otherwise dropped right after parsing to save memory).
    pub properties: bool,
}

fn feature(mut f: Value, props: bool) -> Res<Feature> {
    let row = f.get("row").and_then(Value::as_u64).ok_or("host.features: a feature without a row")? as usize;
    let id = f.get("id").and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok())));
    let id = id.ok_or("host.features: a feature without a numeric id")?;
    let geometry = match f.get("geometry") {
        None | Some(Value::Null) => None,
        Some(g) if g.get("type").and_then(Value::as_str) == Some("GeometryCollection") => None,
        Some(g) => Some(geojson::from_geojson(g).map_err(|e| format!("feature {id}: {e}"))?),
    };
    let properties = match (props, f.get_mut("properties").map(Value::take)) {
        (true, Some(Value::Object(m))) => m,
        _ => Map::new(),
    };
    Ok(Feature { id, row, geometry, properties })
}

/// A feature of a `host.features` reply, read straight from the text.
fn feature_text(r: &mut JsonReader, props: bool) -> Res<Feature> {
    let (mut id, mut row, mut geometry, mut properties) = (None, None, None, Map::new());
    r.object(|r, k| {
        match k {
            b"geometry" => {
                if !r.null() {
                    geometry = r.geometry()?;
                }
            }
            b"id" => {
                let t = r.skip()?;
                id = std::str::from_utf8(t).ok().map(|t| t.trim_matches('"')).and_then(|t| t.parse().ok());
            }
            b"row" => row = std::str::from_utf8(r.skip()?).ok().and_then(|t| t.parse().ok()),
            b"properties" if props => {
                properties = rustgis_plugin::serde_json::from_slice(r.skip()?).map_err(|e| e.to_string())?;
            }
            _ => {
                r.skip()?;
            }
        }
        Ok(())
    })?;
    match (id, row) {
        (Some(id), Some(row)) => Ok(Feature { id, row, geometry, properties }),
        _ => Err("a feature without a numeric id and row".into()),
    }
}

/// The features of a `{"ok": {"features": [...]}}` reply without a `Value` tree; any surprise
/// is an error and the caller reads the reply the general way.
fn features_text(buf: &[u8], props: bool) -> Res<Vec<Feature>> {
    let mut out = None;
    let mut r = JsonReader::new(buf);
    r.object(|r, k| {
        if k != b"ok" {
            return Err("not an ok reply".into());
        }
        r.object(|r, k| {
            if k == b"features" {
                let mut v = Vec::new();
                r.array(|r| {
                    v.push(feature_text(r, props)?);
                    Ok(())
                })?;
                out = Some(v);
            } else {
                r.skip()?;
            }
            Ok(())
        })
    })?;
    out.ok_or_else(|| "no feature list".into())
}

/// Reads features of a layer, refusing replies over [`budget::MAX_READ_BYTES`] before
/// parsing them. `what` names the features in that message ("the selected features of
/// 'parcels'").
pub fn features_named(layer: i64, q: Query, what: &str) -> Res<Vec<Feature>> {
    let mut p = json!({"layer": layer});
    if q.selected {
        p["selected"] = json!(true);
    }
    if let Some(n) = q.limit {
        p["limit"] = json!(n);
    }
    let req = json!({"op": "host.features", "params": p}).to_string();
    let buf = call_text("host.features", &req)?;
    budget::check_read(buf.len(), what)?;
    if let Ok(v) = features_text(&buf, q.properties) {
        return Ok(v);
    }
    let mut fc = reply("host.features", buf)?;
    let list = match fc.get_mut("features").map(Value::take) {
        Some(Value::Array(a)) => a,
        _ => return Err("host.features: no feature list".into()),
    };
    drop(fc);
    list.into_iter().map(|f| feature(f, q.properties)).collect()
}

/// Reads features of a layer (see [`features_named`]).
pub fn features(layer: i64, q: Query) -> Res<Vec<Feature>> {
    let what = if q.selected { format!("the selected features of layer {layer}") } else { format!("layer {layer}") };
    features_named(layer, q, &what)
}

/// The selected features of a layer.
pub fn selected_features(layer: i64, properties: bool) -> Res<Vec<Feature>> {
    features(layer, Query { selected: true, limit: None, properties })
}

/// Every feature of a layer, refusing layers with more than `cap` features (use
/// [`LAYER_READ_CAP`]) or more data than [`budget::MAX_READ_BYTES`], with a message that
/// tells the user to select instead.
pub fn all_features_capped(layer: &LayerInfo, cap: usize, properties: bool) -> Res<Vec<Feature>> {
    if layer.features > cap {
        return Err(too_big(layer, cap));
    }
    let q = Query { selected: false, limit: Some(cap.saturating_add(1)), properties };
    let v = features_named(layer.id, q, &format!("the {} features of '{}'", layer.features, layer.name))?;
    if v.len() > cap {
        return Err(too_big(layer, cap));
    }
    Ok(v)
}

/// Every feature of a layer and the size of the reply, or `None` when the reply is over
/// `max_bytes` (it is then not parsed, which is what would cost fuel) or the layer has more
/// than `cap` features. For optional reads that a command skips rather than fails on.
pub fn all_features_within(layer: &LayerInfo, cap: usize, max_bytes: usize) -> Res<Option<(Vec<Feature>, usize)>> {
    if layer.features > cap {
        return Ok(None);
    }
    let p = json!({"layer": layer.id, "limit": cap.saturating_add(1)});
    let req = json!({"op": "host.features", "params": p}).to_string();
    let buf = call_text("host.features", &req)?;
    let bytes = buf.len();
    if bytes > max_bytes {
        return Ok(None);
    }
    let v = match features_text(&buf, false) {
        Ok(v) => v,
        Err(_) => {
            let mut fc = reply("host.features", buf)?;
            let list = match fc.get_mut("features").map(Value::take) {
                Some(Value::Array(a)) => a,
                _ => return Err("host.features: no feature list".into()),
            };
            list.into_iter().map(|f| feature(f, false)).collect::<Res<Vec<_>>>()?
        }
    };
    Ok((v.len() <= cap).then_some((v, bytes)))
}

/// Inserts a vertex at (lon, lat) after vertex `index` of ring `ring` (0: the outer ring) of
/// part `part` of a feature (`edit.vertex`); every other vertex keeps its coordinates.
pub fn insert_vertex(layer: i64, row: usize, part: usize, ring: usize, index: usize, lon: f64, lat: f64) -> Res<()> {
    call(
        "edit.vertex",
        json!({"layer": layer, "row": row, "action": "insert", "part": part, "ring": ring, "index": index,
            "lon": lon, "lat": lat}),
    )?;
    Ok(())
}

fn too_big(layer: &LayerInfo, cap: usize) -> String {
    format!(
        "'{}' has {} features, more than the {cap} this command reads at once. Select the features to work on.",
        layer.name, layer.features
    )
}

/// Current rows of the features with `ids` (`None` for ids no longer in the layer). Reads
/// the whole layer (capped), so prefer [`rows_after_delete`] bookkeeping where possible.
pub fn resolve_rows(layer: &LayerInfo, ids: &[i64], cap: usize) -> Res<Vec<Option<usize>>> {
    let all = all_features_capped(layer, cap, false)?;
    Ok(ids.iter().map(|id| all.iter().find(|f| f.id == *id).map(|f| f.row)).collect())
}

/// Rows of a layer after `deleted` rows were removed: each row in `rows` maps to its new
/// index, or `None` when it was deleted itself. (Rows above a deleted one shift down.)
pub fn rows_after_delete(rows: &[usize], deleted: &[usize]) -> Vec<Option<usize>> {
    let mut d = deleted.to_vec();
    d.sort_unstable();
    d.dedup();
    rows.iter()
        .map(|&r| match d.binary_search(&r) {
            Ok(_) => None,
            Err(below) => Some(r - below),
        })
        .collect()
}

// ---------- edits ----------

/// Replaces a feature's geometry. The geometry must be lon/lat and pass
/// [`geojson::sanity_check`]; run [`geojson::prepare_output`] on results first.
pub fn set_geometry(layer: i64, row: usize, g: &Geom) -> Res<()> {
    geojson::sanity_check(g).map_err(|e| format!("not writing feature at row {row}: {e}"))?;
    edit_call("edit.setGeometry", layer, Some(row), g, None)?;
    Ok(())
}

/// An edit request carrying a geometry, written as text (see [`geojson::write_geojson`]).
fn edit_call(op: &str, layer: i64, row: Option<usize>, g: &Geom, attributes: Option<&Map<String, Value>>) -> Res<Value> {
    let mut req = format!("{{\"op\":\"{op}\",\"params\":{{\"layer\":{layer},");
    if let Some(row) = row {
        req.push_str(&format!("\"row\":{row},"));
    }
    if let Some(a) = attributes {
        req.push_str("\"attributes\":");
        req.push_str(&rustgis_plugin::serde_json::to_string(a).map_err(|e| format!("{op}: {e}"))?);
        req.push(',');
    }
    req.push_str("\"geometry\":");
    geojson::write_geojson(&mut req, g);
    req.push_str("}}");
    reply(op, call_text(op, &req)?)
}

/// Fields that identify a feature: a feature made from another never inherits them, and
/// merging never sums them. `host.layers` does not say which field is a provider key, so
/// the usual key names decide (any case).
pub const ID_FIELDS: [&str; 3] = ["id", "fid", "ogc_fid"];

pub fn is_id_field(name: &str) -> bool {
    ID_FIELDS.iter().any(|f| name.eq_ignore_ascii_case(f))
}

/// The field values a feature made from one with `props` inherits: all but [`ID_FIELDS`],
/// which would repeat a unique key.
pub fn inherited_attributes(props: &Map<String, Value>) -> Map<String, Value> {
    props.iter().filter(|(k, _)| !is_id_field(k)).map(|(k, v)| (k.clone(), v.clone())).collect()
}

/// Appends a feature and returns its row. The host replaces the selection with the new
/// feature. `attributes` may hold any subset of the layer's fields.
pub fn add_feature(layer: i64, g: &Geom, attributes: &Map<String, Value>) -> Res<usize> {
    geojson::sanity_check(g).map_err(|e| format!("not adding a feature: {e}"))?;
    let r = edit_call("edit.addFeature", layer, None, g, Some(attributes).filter(|a| !a.is_empty()))?;
    r.get("row").and_then(Value::as_u64).map(|n| n as usize).ok_or_else(|| "edit.addFeature: no row in the answer".into())
}

/// Sets one field of one feature.
pub fn set_value(layer: i64, row: usize, field: &str, value: Value) -> Res<()> {
    call("edit.setValue", json!({"layer": layer, "row": row, "field": field, "value": value}))?;
    Ok(())
}

/// Deletes exactly `rows` of `layer`. `edit.delete` deletes the selection of every layer,
/// so the selection is first replaced by just these rows (which clears other layers'
/// selections). Fails unless the host reports exactly that many deleted. Afterwards nothing
/// is selected and rows above the deleted ones have shifted (see [`rows_after_delete`]).
pub fn delete_rows(layer: i64, rows: &[usize]) -> Res<usize> {
    let mut r = rows.to_vec();
    r.sort_unstable();
    r.dedup();
    if r.is_empty() {
        return Ok(0);
    }
    select_rows(layer, &r, false)?;
    let out = call("edit.delete", json!({}))?;
    let n = out.get("deleted").and_then(Value::as_u64).unwrap_or(0) as usize;
    if n != r.len() {
        return Err(format!("expected to delete {} features, the host deleted {n}", r.len()));
    }
    Ok(n)
}

/// Makes `layer` the active layer.
pub fn select_layer(layer: i64) -> Res<()> {
    call("layer.select", json!({"layer": layer}))?;
    Ok(())
}

/// Selects `rows` of `layer`. Without `add` the selection of every layer is replaced.
pub fn select_rows(layer: i64, rows: &[usize], add: bool) -> Res<()> {
    let mut p = json!({"layer": layer, "rows": rows});
    if add {
        p["mode"] = json!("add");
    }
    call("select.rows", p)?;
    Ok(())
}

pub fn clear_selection() -> Res<()> {
    call("select.clear", json!({}))?;
    Ok(())
}

/// Replaces the whole selection with `rows` per layer (e.g. the results of a command). The
/// first `select.rows` without `add` replaces every layer's selection; `select.clear` is only
/// sent when something is selected, since it fails otherwise.
pub fn set_selection(sel: &[(i64, Vec<usize>)]) -> Res<()> {
    let mut first = true;
    for (layer, rows) in sel.iter().filter(|(_, r)| !r.is_empty()) {
        select_rows(*layer, rows, !first)?;
        first = false;
    }
    if first && layers()?.iter().any(|l| l.selected > 0) {
        clear_selection()?;
    }
    Ok(())
}

/// A status-bar message.
pub fn notify(text: &str) -> Res<()> {
    call("host.notify", json!({"text": text}))?;
    Ok(())
}

/// A status-bar message shown as an error (the command itself may still succeed).
pub fn notify_error(text: &str) -> Res<()> {
    call("host.notify", json!({"text": text, "error": true}))?;
    Ok(())
}

/// A line in RustGIS's log.
pub fn log(text: &str) {
    rustgis_plugin::host::log(text);
}

/// What one command can afford. RustGIS counts the plugin's own WebAssembly instructions
/// ("fuel"; host calls are free) and stops a command that uses up its budget with "ran too
/// long". The checks here refuse such work before it starts, saying what to do instead.
///
/// The costs are measured on the release build with real parcel data: reading is parsing the
/// `host.features` reply (about 40 bytes per vertex), editing a vertex is projecting it
/// through a [`crate::proj::VertexSnap`], unprojecting and writing it. Reading may take 30 %
/// of a command's budget, reading and editing the edited features 50 %, which leaves at
/// least 40 % for the geometry work itself. Tools run with [`TOOL_FUEL`], 60 times more, and
/// are not checked.
pub mod budget {
    use super::{Feature, Res};

    /// The budget of one command (fixed by RustGIS).
    pub const COMMAND_FUEL: u64 = 500_000_000;
    /// The budget of one geoprocessing tool run: 60 commands.
    pub const TOOL_FUEL: u64 = 30_000_000_000;

    /// Fuel per byte of a `host.features` reply.
    pub const READ_FUEL_PER_BYTE: u64 = 125;
    /// Reply bytes per vertex (coordinates with 16-17 digits).
    pub const BYTES_PER_VERTEX: u64 = 40;
    /// Fuel per edited vertex: reading it, projecting, snapping back and writing.
    pub const EDIT_FUEL_PER_VERTEX: u64 = BYTES_PER_VERTEX * READ_FUEL_PER_BYTE + 7_000;
    /// Fuel per edited feature: its read, request and reply.
    pub const EDIT_FUEL_PER_FEATURE: u64 = 25_000;
    /// Fuel per vertex a command creates (beyond those it read): projecting it back, snapping
    /// and writing.
    pub const WRITE_FUEL_PER_VERTEX: u64 = 6_000;

    /// The largest `host.features` reply a command parses: 30 % of its budget (1.2 MB).
    pub const MAX_READ_BYTES: usize = (COMMAND_FUEL * 3 / 10 / READ_FUEL_PER_BYTE) as usize;
    /// What reading and editing the edited features may take.
    pub const MAX_EDIT_FUEL: u64 = COMMAND_FUEL / 2;
    /// The most vertices a command edits (about 20 000), fewer with many small features.
    pub const MAX_EDIT_VERTICES: usize = (MAX_EDIT_FUEL / EDIT_FUEL_PER_VERTEX) as usize;
    /// The most vertices a command creates (about 40 000) on top of what it read.
    pub const MAX_WRITE_VERTICES: usize = (MAX_EDIT_FUEL / WRITE_FUEL_PER_VERTEX) as usize;

    /// Groups thousands for messages: 61 000.
    pub fn count(n: usize) -> String {
        let s = n.to_string();
        let mut out = String::new();
        for (i, c) in s.chars().enumerate() {
            if i > 0 && (s.len() - i) % 3 == 0 {
                out.push(' ');
            }
            out.push(c);
        }
        out
    }

    /// `n` cut to two significant digits, for "about" numbers.
    fn about(n: usize) -> String {
        let mut m = 1;
        while n / m >= 100 {
            m *= 10;
        }
        count(n / m * m)
    }

    /// Vertices of the features' geometries.
    pub fn vertices(features: &[Feature]) -> usize {
        features.iter().filter_map(|f| f.geometry.as_ref()).map(|g| g.num_coords()).sum()
    }

    /// Estimated fuel to read, project and write `features` with `vertices` in total.
    pub fn edit_fuel(features: usize, vertices: usize) -> u64 {
        features as u64 * EDIT_FUEL_PER_FEATURE + vertices as u64 * EDIT_FUEL_PER_VERTEX
    }

    /// Estimated fuel to write `features` with `vertices` in total that a command computed.
    pub fn write_fuel(features: usize, vertices: usize) -> u64 {
        features as u64 * EDIT_FUEL_PER_FEATURE + vertices as u64 * WRITE_FUEL_PER_VERTEX
    }

    /// Fuel a command may still spend on work it charges as it goes, all of it computed before
    /// the first write: running out refuses the command with `why` and changes nothing.
    pub struct Meter {
        pub left: u64,
        pub spent: u64,
        /// Whether a `spend` was refused: the error to return is `why`, whatever the work in
        /// progress made of it.
        pub out: bool,
        pub why: String,
    }

    impl Meter {
        pub fn new(left: u64, why: impl Into<String>) -> Meter {
            Meter { left, spent: 0, out: false, why: why.into() }
        }

        /// A meter that never runs out, to measure what some work is charged.
        pub fn unlimited() -> Meter {
            Meter::new(u64::MAX / 4, "")
        }

        pub fn spend(&mut self, fuel: u64) -> Res<()> {
            if fuel > self.left {
                self.left = 0;
                self.out = true;
                return Err(self.why.clone());
            }
            self.left -= fuel;
            self.spent += fuel;
            Ok(())
        }
    }

    /// Refuses a reply of `bytes` before it is parsed; `what` names it ("the 812 features of
    /// 'parcels'").
    pub fn check_read(bytes: usize, what: &str) -> Res<()> {
        if bytes <= MAX_READ_BYTES {
            return Ok(());
        }
        Err(format!(
            "Reading {what} means {:.1} MB of data; one command reads about {:.1} MB. Select fewer features.",
            bytes as f64 / 1e6,
            MAX_READ_BYTES as f64 / 1e6
        ))
    }

    /// Refuses editing `features` with `vertices` in total within one command.
    pub fn check_edit(features: usize, vertices: usize) -> Res<()> {
        if edit_fuel(features, vertices) <= MAX_EDIT_FUEL {
            return Ok(());
        }
        Err(format!(
            "This would edit {} features with {} vertices; one command handles about {} vertices. \
             Select fewer features and repeat the command for the rest.",
            count(features),
            count(vertices),
            about(MAX_EDIT_VERTICES)
        ))
    }

    /// Refuses writing `features` with `vertices` in total that a command is about to create
    /// on top of what it read (densified or smoothed lines, offset copies), within the half
    /// of the budget [`check_edit`] leaves.
    pub fn check_write(features: usize, vertices: usize) -> Res<()> {
        if write_fuel(features, vertices) <= MAX_EDIT_FUEL {
            return Ok(());
        }
        Err(format!(
            "The result would be {} features with about {} vertices; one command writes about {} vertices. \
             Select fewer features or use a coarser setting, and repeat the command for the rest.",
            count(features),
            count(vertices),
            about(MAX_WRITE_VERTICES)
        ))
    }
}
