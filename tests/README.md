# Tests

The tests drive the plugin in a private, headless RustGIS: `rustgis-cli mcp --yes` over stdio,
with its own in-memory project and its own plugin folder (never the desktop app). They use the
standard library only. What the suite checks, and against which oracles, is described in
[docs/TESTING.md](../docs/TESTING.md).

```sh
./build.sh                          # builds pkg-diag/plugin.wasm, which the tests load
python -I tests/run.py              # every tests/test_*.py
python -I tests/run.py polygons     # tests/test_polygons.py only
python -I tests/run.py -k split     # test functions whose name contains "split"
```

Environment:

- `AE_REAL_DATA`: a file of adjacent parcels in Poland (e.g. a GeoPackage) for `test_realdata.py`;
  skipped when unset. Real data is never stored in the repository.
- `AE_SHAPELY_PYTHON`: a Python with shapely for the geometry-oracle tests (`test_edge_*.py`,
  `test_fill_ring.py`, parts of `test_topo.py`), run over a pipe; by default the running Python
  if it has shapely, otherwise those tests are skipped.
- `RUSTGIS_CLI`: the `rustgis-cli` to use (default: the one on `PATH`).
- `AE_TMP`: where the temporary directory goes; `AE_KEEP_TMP=1` keeps it after the run.

## Adding tests for a module

Each command module has its own file: `test_polygons.py`, `test_lines.py`,
`test_attributes.py`, `test_division.py`. Any `test_*` function in a `tests/test_*.py` file
runs, in file order; a new file needs no registration. A test passes when it returns, fails on
any exception, and is skipped on `aelib.Skip`.

Each module file lists its commands that are still stubs in `STUBS`: remove a command from it
when it is implemented, and add real tests.

```python
from aelib import PL, Session, check, feature, grid_layer, polygon, rect, write_layer, polygon_area_m2

def test_cut_removes_the_cutter_area():
    lon, lat = PL
    cutter = write_layer('cutter', [feature(polygon(rect(lon, lat, 50, 100, 25, 0)), name='cut')])
    with Session(grid_layer('parcels', nx=2), cutter) as s:
        s.select('parcels', [0, 1])          # targets: selected in another layer
        s.activate('cutter')                 # tools: selected in the active layer
        s.select('cutter', [0], add=True)
        r = s.plugin('cut-with-polygon', {'keepTools': True})
        areas = [polygon_area_m2(f['geometry']) for f in s.features('parcels')]
        check(all(abs(a - 7500) < 1 for a in areas), areas)
```

## aelib

- Data: `write_layer(name, features)` writes `<name>.geojson` into the run's temporary
  directory (the layer is called `name`); `feature(geometry, **props)`, `polygon(*rings)`,
  `multipolygon(...)`, `linestring(lon, lat, [(x, y) metres])`, `point(...)`,
  `rect(lon, lat, w, h, x0, y0)` (a ring in metres from an origin), `grid_layer(name, nx, ny,
  size)` (adjacent squares named `c<i>_<j>`), `holed_layer(name)`. `PL` is an origin in Poland
  (EPSG:2180 frame), `ABROAD` one in Italy (UTM 32N).
- `Session(*files)`: `run(command, params)` any RustGIS command, `plugin(command, params)` this
  plugin's, `error_of(command, params)` a plugin command that must fail (returns the message),
  `activate(layer)`, `select(layer, rows, add=False)`, `clear_selection()`, `features(layer)`
  (GeoJSON features in row order, with `id`), `layers()` (by name: id, dim, counts, fields),
  `selected_ids(layer)`, `undo()`, `redo()`.
- `run_once([(command, params-or-None)], data)`: one `rustgis-cli run`; `None` passes no
  parameters at all, as a ribbon button does.
- Assertions: `check(cond, msg)`, `close(a, b, tol)`, `polygon_area_m2(geometry)`.

## Host behaviour to keep in mind

- Every plugin command is an undo step, even a read-only one such as `diag-layers`
  (`Session.layers()`), and clears redo. Between an edit and `undo()`, read with
  `Session.features()` (a built-in command, no undo step).
- A command that fails rolls all of its edits back.
- Rows shift after deletes; feature `id`s stay. `edit.addFeature` selects the new feature.
- The host stores a one-part MultiPolygon as a Polygon.
- Coordinates written back can differ from what was sent by about one ulp: compare with a
  tolerance, never `==`.

## What one command can afford

RustGIS counts the plugin's own WebAssembly instructions and stops a command after a fixed
budget ("it ran too long"); host calls are free, tools get 60 times more. `host_io::budget`
refuses work that would not fit before it starts, with a message that says what to do:

- Every `host.features` reply over `MAX_READ_BYTES` (1.2 MB, 30 % of the budget) is refused
  before parsing: "Reading the 35 features of 'blobs' means 1.4 MB of data; one command reads
  about 1.2 MB. Select fewer features." (`host_io::features*`, `all_features_capped`).
- `selection::gather` and `selection::active_features` refuse selections whose reading,
  projecting and writing would take more than half the budget, about 20 000 vertices: "This
  would edit 812 features with 61 000 vertices; one command handles about 20 000 vertices.
  Select fewer features and repeat the command for the rest." The rest is left for the
  geometry work.
- Commands whose output or work grows beyond their input (more parts, denser lines) check it
  themselves before computing: `budget::check_write(features, vertices)` for the vertices they
  create (about 40 000), `budget::edit_fuel` and `budget::write_fuel` for estimates to compare
  with `budget::COMMAND_FUEL` together with their own costs. Division estimates its cutting
  from each polygon's vertices and parts (`division::check_division`, also for the tool with
  `budget::TOOL_FUEL`); Offset Curve counts its work as it goes and buffers polygons by an
  estimate (`lines::OffsetFuel`), and computes every offset before writing any. Cut, Clip and
  Split With Line charge a `budget::Meter` for snapping (`topo::Placer::snap_paths`), the
  boolean ops and slices (`polygons::overlay_fuel` from `polygons::XRanges`), placing and
  sharing the new vertices and the writes, all before the first write, and give topological
  editing what is left (`topo::Scope::limit`).

`test_limits.py`, `test_division.py`, `test_lines.py` and `test_attributes.py` check the
guards on both sides of the limit. To measure a cost, the diagnostic build has `diag-bench`
(`{"op": "proj" | "snap" | "ser" | "parse" | "write" | "read" | "divide" | "offset" |
"split" | "cut", "n": vertices, "reps": repetitions, "geometry": a GeoJSON geometry instead of
the test ring (the tool of "split" and "cut", whose targets are "targets", with "stage":
"snap" for snapping alone), "parts", "distance", "pad": extra projection round trips}`): the largest `reps` that still
finishes gives the share of the budget one unit takes, and the largest `pad` with and without
the op (`"base"`) gives what one run of it takes, to about 0.01 % of the budget.

## Diagnostic commands

`pkg-diag/plugin.wasm` is built with `--features diagnostics` (`src/diagnostics.rs`); the
harness declares these commands in the test copy of `plugin.json`. They never ship:
`diag-layers`, `diag-features` (`rewrite` writes every geometry back unchanged),
`diag-selection`, `diag-proj` (`grid` compares with the term-by-term reference projection),
`diag-geojson` (`numbers` checks number text against serde_json), `diag-snap`, `diag-delete`
(`select` then sets the selection), `diag-add`, `diag-settings` (every panel setting),
`diag-bench`.
