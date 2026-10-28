# Testing

What is tested, how, and against what. For running the tests and writing new ones, see [Running the tests](../README.md#running-the-tests) and the developer notes in [tests/README.md](../tests/README.md).

**Current state:** 201 tests, all passing (`201 passed, 0 skipped, 0 failed`, about 40 s), of which 14 use real cadastral data and are skipped without `AE_REAL_DATA` (then `187 passed, 14 skipped`).

## Contents

- [Test architecture](#test-architecture)
- [Oracles and assertions](#oracles-and-assertions)
- [Inventory](#inventory)
- [Verification in the RustGIS desktop app](#verification-in-the-rustgis-desktop-app)
- [Accuracy summary](#accuracy-summary)
- [Budget and limits](#budget-and-limits)

## Test architecture

**End to end, in a real host.** The tests do not call Rust functions. Each test starts a private, headless RustGIS, `rustgis-cli mcp --yes`, and talks to it over stdio (MCP): it opens data, sets the active layer and the selection, runs plugin commands exactly as a script or the ribbon would, reads the features back as GeoJSON, and undoes. The host gets its own in-memory project and its own plugin folder, never the desktop app's. So every test exercises the shipped WebAssembly code together with RustGIS's real feature storage, selection, undo and rollback.

**The module under test** is `pkg-diag/plugin.wasm`, built by `build.sh` from the same sources as the release with the `diagnostics` feature: it adds a few commands the release does not have (`diag-layers`, `diag-features`, `diag-selection`, `diag-proj`, `diag-geojson`, `diag-snap`, `diag-delete`, `diag-add`, `diag-settings`, `diag-bench`). The harness installs it with a test copy of `plugin.json` that declares them. They let tests look inside: project a point in the plugin's own frame, compare number formatting with serde_json, rewrite geometries unchanged, read the effective settings, or measure costs. A few tests also run the plain `rustgis-cli run` path (ribbon-style calls without parameters).

**Synthetic data per test.** Each test writes its input as GeoJSON into a temporary directory: grids of adjacent squares, irregular quadrilaterals with shared corners, polygons with holes and islands, multi-part polygons, combs, U-shapes, zigzags, dense noisy lines, blobs of 1000 vertices. Shapes are given in metres from an origin in Poland (EPSG:2180 frame) or in Italy (UTM 32N) and converted to longitude/latitude, so the expected areas and distances are known exactly.

**Real data** (`AE_REAL_DATA`): a file of adjacent cadastral parcels in Poland. 14 tests run on it: cut, clip, rings, parts, fill gap, split, reshape, simplify and division on real parcels, a zigzag split across many parcels, division at many angles, and checks that untouched vertices and neighbours stay bit-exact. Real data is never stored in the repository.

**Geometry oracles in a separate Python.** The edge-case tests compare results with shapely, run in a separate interpreter over a pipe (`AE_SHAPELY_PYTHON`, by default the running Python if it has shapely), so the harness itself needs only the standard library. Without shapely those tests are skipped.

**Every test is self-checking on undo and rollback.** Where a command succeeds, many tests undo it and compare the layer with its state before; where a command must be refused, they compare the layer after the refusal with its state before.

## Oracles and assertions

The expected results come from geometry, not from the plugin's own output.

| Oracle / assertion | How it is computed | Used for |
|---|---|---|
| **Planar-graph faces** | shapely `polygonize` of the target's boundary unioned with the tool lines, in the plugin's metric frame; faces inside the target are the expected pieces | Split With Line: piece count and each piece's area must match the faces |
| **Area conservation** | sum of the pieces' areas vs. the original's, tolerance max(1e-4 m², 1e-6·A) | split, cut, clip, division, fill gap |
| **Union = original** | area of the symmetric difference between the union of the pieces and the original, same tolerance | split, division |
| **No overlaps** | pairwise intersection areas of all output polygons, also of pieces of different targets | split, cut, clip, fill ring, division |
| **Validity** | shapely `is_valid` with `explain_validity` for every output; no pieces under 1e-3 m² | all edge tests |
| **Bit-exact untouched vertices** | coordinates of vertices a command must not move compared with the input (within one ulp, as the host re-rounds on write); neighbours that are not edited compared whole | every command that writes geometry; real-data snap test |
| **New vertices on original edges** | distance of each new vertex from the original edge's line, in the plugin's own frame (`diag-proj`) | cut, split, division (≤ 1.9e-9 m measured on 4×3 adjacent parcels) |
| **Topological editing** | neighbours may only gain vertices, each on their own edge; the counts in the answer must match | split, cut, division, line targets |
| **No new holes in the layer** | holes of the union of all features after vs. before | split and division across adjacent parcels |
| **Undo restores** | fingerprint (id, attributes, geometry of every feature) after one undo equals the one before | most command tests |
| **Refusals change nothing** | fingerprint after a refused command equals the one before; the message must say what to select, choose, set, raise or lower | every command, many wrong selections |
| **Ellipsoidal area** | each edge densified as a straight line of the transverse Mercator frame (the edge the plugin cuts), the densified ring measured exactly in the GRS80 cylindrical equal-area projection (authalic latitude) | division: each part within max(0.01 m², 1e-6·A) |
| **Ordered sweep** | every vertex of each part lies inside the original, and each part lies after the previous one along the sweep direction | division order and `reverse` |
| **Projection reference** | the Krüger n⁶ series evaluated term by term in 45-digit decimal arithmetic, and EPSG:2180 coordinates of Polish cities from PROJ | projection ≤ 3e-9 m from the reference; round trip ≤ 1e-13° |
| **Number text** | the plugin's shortest round-trip float formatting compared with serde_json | GeoJSON writer |
| **Distance and Hausdorff** | distance from offset vertices to the original line, in the plugin's frame and with shapely | Offset Curve: on the correct side, not nearer than the distance less 0.13 % |

## Inventory

| File | Tests | What it covers |
|---|---:|---|
| `test_manifest.py` | 6 | `plugin.json` against the code: every command dispatched, tab and panel name existing commands, every command has help and params, panel widgets read by the code exist, every script parameter is documented, the tool's parameters match what the code reads. |
| `test_shared.py` | 12 | Shared modules and host behaviour: projection vs. reference coordinates and round trip, UTM abroad and mixed-extent frames, GeoJSON normalisation and output checks, bit-exact vertex snapping, settings from panel, parameters and `null`, the selection model, the read cap by size, selection after deletes, safe deletes, row tracking of added features, rollback of a failing command, redo of an undone command. |
| `test_limits.py` | 8 | The budget guards on both sides of each limit (edit, read), a large selection within the guard, projection accuracy vs. the 45-digit series, vertex-snap regressions, shortest exact number text, the GeoJSON text reader and writer, and that the README's Limits section matches the budget constants. |
| `test_polygons.py` | 19 | Cut, Clip, Add Ring, Delete Ring, Fill Ring, Add Part, Delete Part, Split Off Part, Fill Gap: results, multi-part results, deleting covered targets, shared vertices kept, undo, wrong selections, neighbours in the active layer ignored; 4 real-data tests (cut, clip, rings and parts, fill gap on a whole layer). |
| `test_edge_polygons.py` | 15 | Polygon edge cases against shapely: tool equal to the target, sharing only an edge, with a hole, multi-part tools giving three pieces, adjacent targets keeping their topology, collinear and nearly collinear edges, rings touching and nested, rings in a part of a multi-part polygon, equal-sized parts, gaps touching the outside at a point, slivers and thin gaps, a gap inside a hole with an island, Add Part merging along a touching edge. |
| `test_fill_ring.py` | 10 | Fill Ring with islands: part of the same feature, another feature, touching at a point, splitting the hole, two and nested islands, islands covering the hole, no islands, a locator picking one of two holes, a layer too large to read still subtracting the feature's own parts. |
| `test_lines.py` | 21 | Split With Line (polygons, largest piece kept, unsplit parts, lines and closed lines, the tool in the target layer, a line that does not cross, a large polygon), Reshape (out and in, holes and multi-part, lines and extension, wrong selections), Offset Curve (lines, too far, polygons, in place, long lines and the budget, budget by shape), Trim/Extend (other layer, one layer, search radius); 2 real-data tests (split, reshape). |
| `test_edge_lines.py` | 29 | Split against the planar-graph oracle: zigzags with many crossings, two and three crossing lines, a 2×2 grid, a self-crossing line, lines through vertices and along edges, tangent and dangling lines (no split), crossing then dangling, holes, combs, multi-part targets, adjacent targets along a shared edge, a tool 0.1 mm from an edge, thin acute polygons, positions far from the frame's centre, degenerate tool lines, line targets and collinear overlaps. Reshape with four crossings, self-touching results, holes and multi-part, a hole pushed to the shell. Offset of zigzags longer than their segments, spikes and short segments, dense noisy lines, rings inwards beyond collapse, polygons with holes. Trim/Extend to a polygon and to a line crossed twice. |
| `test_attributes.py` | 20 | Merge (largest attributes kept, ties by lowest id, lines joined at touching ends, numeric sums, at least two features), Exchange (swaps, exactly two), Copy/Move (copy along the bearing, move, selection required), Simplify (noise and collapsed rings, a feature that would turn invalid, points refused), Densify (long segments, too many vertices refused, the write guard on both sides), Smooth (line ends kept, rings closed, selection required); 1 real-data test (simplify a parcel). |
| `test_edge_misc.py` | 15 | Merge of polygons touching at a point, overlapping and T-shaped lines, sums with nulls and numeric text; Exchange across layers refused; every command refusing an empty selection and a selection only in another layer, a point or raster active layer, mixed geometry selections; division parts out of range; Simplify/Smooth with a hole near the shell, self-touching after removal; Densify of degenerate lines; 2 real-data tests (zigzag split across parcels, division at many angles). |
| `test_division.py` | 23 | Divide Into Equal Parts on rectangles, L-, U-, holed and multi-part shapes at several angles (each part's ellipsoidal area, coverage, order), not planar, order and `reverse`, UTM abroad, undo, ribbon defaults, wrong selections; Cut Off Percent (single, `reverse`, many); the division budget on both sides and typical use; the Divide Parcel tool (equal parts leave the input unchanged, parts of given area, an exactly dividing area leaves no sliver, strips of given width, `reverse`, planar mode, selection and errors, many parts of a big parcel, budget); 2 real-data tests (neighbours kept, ellipsoidal vs. planar). |
| `test_edge_division.py` | 12 | Division edge cases: concave shapes, holed polygons into many parts, angles from 0° to 359.9° including negative and near-90° values, extreme percentages, 0 % and 100 % refused, many parts of a small polygon, multi-part polygons, `reverse`, cuts through vertices, frames far from Poland, the tool's area and width at their edges, holed and comb-shaped parcels in the tool. |
| `test_topo.py` | 9 | Shared boundaries: a split of 4×3 adjacent parcels leaves no gaps between neighbours, a real split leaves no new hairline holes, topological editing adds the shared vertices to the neighbours (split, cut, division, a line along a split line), without it the neighbours stay unchanged, a layer too large to read is skipped and named, and the topology note goes before a message's full stop. |
| `test_realdata.py` | 2 | Real data: the neighbours and the frame, bit-exact vertex snapping on real parcels. |
| **Total** | **201** | |

## Verification in the RustGIS desktop app

Besides the automated suite, the commands were run in the RustGIS desktop app on the installed release build, by hand on the ribbon, and the results were exported and checked independently with shapely and pyproj.

| Case | Result |
|---|---|
| **Split With Line, 13 synthetic cases** (sawtooth entering and leaving 6 times, zigzag, two crossing lines, self-crossing loop, U-shape, hole, adjacent pair, two crossing zigzags, line through a vertex and along an edge, comb, closed ring cutting an island) | Piece counts and areas match the planar-graph oracle; area deviation < 0.001 m²; the pieces cover the original within 1e-7 m², with zero overlap. Two crossing zigzags give 25 pieces. Tangent and dangling lines are refused. |
| **76 real adjacent cadastral parcels** cut by a zigzag and a crossing line | 74 parcels split into 169 pieces; every parcel matches the oracle. Untouched parcels move ≤ 3.3e-9 m (the host's coordinate rounding). Per-parcel union vs. original ≤ 3.7e-7 m²; total overlap 6.6e-8 m². New gaps in the union of the layer: 15 residual gaps about 6e-10 m wide, all produced by the host's own coordinate round trip (rewriting the same parcels unchanged gives the same 15). Before new vertices were placed exactly on the original edges, there were 29 hairline gaps up to 0.6 µm wide (1.8e-5 m²). |
| **Divide Into Equal Parts** on a real parcel with holes, 13 483.12 m² into 5 parts at 30° | Parts 2 696.6231–2 696.6233 m² ellipsoidal, checked with pyproj `Geod` on GRS80 after densifying the edges; the sum matches the original within 7e-5 m². Topological editing added 28 shared vertices to 10 neighbouring parcels (the building parcels lying in its holes). |
| **Fill Ring** with an island | 8 400 m² = 100² − 40²; no overlap with the island. |
| **Reshape** | A bump gives 11 800 m²; a line that would cut the polygon in two is refused. |
| **Offset Curve**, 10 m on a zigzag | Offset on the correct side; minimum distance to the line 9.988 m (round joins approximated by chords, at most 0.13 % nearer, as in GEOS). |
| **Merge With Attributes**, overlapping lines 0–100 m and 50–150 m | One 150 m line; numeric field summed 4 + 5 = 9. |
| **Cut / Clip With Polygon** | 7 500 m² / 2 500 m². |
| **Trim/Extend** | Both lines end exactly on the boundary (x = 850.0000). |
| **Fill Gap** | 20 000 m². |
| **Cut Off Percent**, 30 % with Divide from the other side | Share 0.300000 of the area, on the east side. |
| **Refusals** | `parts` = 1 and `percent` = 0 refused with clear messages; nothing changed. |

## Accuracy summary

| Quantity | Measured | Bound used by the plugin or the tests |
|---|---|---|
| Projection vs. 45-digit evaluation of the series | < 3e-9 m (asserted) | — |
| Projection vs. PROJ (pyproj) | ≤ 2.4e-9 m | — |
| Projection round trip | < 1e-13° (asserted) | — |
| New vertex on its original edge | ≤ 1.9e-9 m (split of 4×3 adjacent parcels) | placed when within 1e-5 m |
| Untouched vertices | bit-exact; ≤ 3.3e-9 m after the host's own rounding | restored when within 1e-5 m |
| Ellipsoidal area vs. geodesic (pyproj `Geod`, GRS80) | ≤ 2.5e-4 m² on parts up to 37 000 m² | — |
| Division: each part's area | ≤ max(0.01 m², 1e-6·A) | refused otherwise |
| Split / cut: pieces vs. original area | < 0.001 m² per piece, union within 1e-7 m² (synthetic) | max(1e-4 m², 1e-6·A) in the tests |
| Offset distance | ≥ 0.9987 · d | chords of round joins, 0.1 rad |

## Budget and limits

RustGIS counts a plugin's WebAssembly instructions and stops a command after a fixed budget; host calls are not counted, and geoprocessing tools get 60 times more. The plugin estimates the cost of what it was asked to do before it starts, from the input's size, and refuses with a message saying what to select when it would not fit (see [Limits](../README.md#limits)).

**How the estimates were measured.** The diagnostic command `diag-bench` runs one operation (projection, snapping, serialisation, parsing, writing, reading, division or offset) `reps` times on `n` vertices or on a given geometry. The largest `reps` that still finishes gives the share of the budget one unit takes; padding a run with extra projection round trips (`pad`) and finding the largest `pad` with and without the operation gives the cost of one run of it, to about 0.01 % of the budget. The cost formulas in the code are upper bounds fitted to such runs: wavy, zigzag, spiral, circular, hairpin, randomly turning and jittery lines and rings of 300 to 3000 vertices offset by 1 to 40 m; smooth, rectangular and comb-shaped rings of 250 to 2000 vertices buffered by 1 to 20 m; wavy and comb-shaped rings of 20 to 12 000 vertices divided into 2 to 50 parts.

**How the guards are tested.** `test_limits.py`, `test_division.py`, `test_lines.py` and `test_attributes.py` run each guard just below and just above its limit: below, the command must succeed within the host's budget; above, it must be refused before any edit, with the layer unchanged. `test_limits.py` also checks that the figures in the README's Limits section follow from the budget constants in the code.
