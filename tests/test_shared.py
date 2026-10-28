"""The shared modules: proj, geojson, host_io, selection, settings; and host behaviour the
commands rely on (rollback, undo/redo, selection after edits)."""

import math

from aelib import (ABROAD, PL, CommandError, Session, check, close, feature, grid_layer, linestring,
                   offset_m, polygon, rect, run_once, write_layer)

# EPSG:2180 coordinates of Polish cities, agreeing with PROJ (pyproj) to well below 1 mm.
REF_2180 = [
    ((21.0122, 52.2297), (637382.2044361836, 486757.20948080625)),
    ((19.945, 50.0647), (567613.6192704574, 244587.53116263545)),
    ((18.6466, 54.352), (477037.5994362044, 720936.5209313687)),
]


def test_projection_reference_and_round_trip():
    with Session() as s:
        pts = [p for p, _ in REF_2180] + [(14.1, 49.1), (24.1, 54.8), (9.0, 45.5), (10.5, 45.5),
                                           (-70.0, -33.0), (150.0, 60.0)]
        r = s.plugin('diag-proj', {'points': pts})
        out = r['points']
        for (p, (x, y)), o in zip(REF_2180, out):
            check(o['frame'] == 'EPSG:2180', o)
            check(close(o['x'], x, 1e-3) and close(o['y'], y, 1e-3), 'EPSG:2180 of %s: %s' % (p, o))
        frames = [o['frame'] for o in out]
        check(frames[3:] == ['EPSG:2180', 'EPSG:2180', 'UTM 32N', 'UTM 32N', 'UTM 19S', 'UTM 56N'], frames)
        check(close(out[5]['x'], 500000.0, 1e-6), 'central meridian of UTM 32: %s' % out[5])
        for o in out:
            check(o['rt_err_deg'] < 1e-10, 'round trip %s' % o)
        # Mixed input: one frame for all, by the centre of the extent.
        mixed = s.plugin('diag-proj', {'points': [[19.0, 52.0], [10.5, 45.5]]})
        check(mixed['frame_all'] == 'UTM 33N', mixed)
        far = s.plugin('diag-proj', {'points': [[0, 50], [30, 50]]})
        check(far['frame_all'].startswith('error'), far)


def test_geojson_normalize_and_checks():
    with Session() as s:
        lon, lat = PL
        sq = rect(lon, lat, 10, 10)
        # unclosed ring with a duplicated position, in a one-part MultiPolygon
        ring = [sq[0], sq[1], sq[1], sq[2], sq[3]]
        r = s.plugin('diag-geojson', {'geometry': {'type': 'MultiPolygon', 'coordinates': [[ring]]}})
        g = r['geometry']
        check(g['type'] == 'Polygon', g)
        check(len(g['coordinates'][0]) == 5 and g['coordinates'][0][0] == g['coordinates'][0][-1], g)
        check(r['valid'] is True, r)
        # a bow tie is closed but not valid
        bow = [sq[0], sq[2], sq[1], sq[3], sq[0]]
        check(s.plugin('diag-geojson', {'geometry': polygon(bow)})['valid'] is False)
        # degenerate input is refused
        e = s.error_of('diag-geojson', {'geometry': polygon([sq[0], sq[1], sq[0]])})
        check('degenerate' in e, e)
        e = s.error_of('diag-geojson', {'geometry': {'type': 'GeometryCollection', 'geometries': []}})
        check('not supported' in e or 'no coordinates' in e, e)


def test_vertex_snap_keeps_original_vertices():
    lon, lat = PL
    ring = [offset_m(lon, lat, 40 * math.cos(i * math.tau / 50), 40 * math.sin(i * math.tau / 50)) for i in range(50)]
    ring.append(ring[0])
    with Session() as s:
        r = s.plugin('diag-snap', {'geometry': polygon(ring)})
        check(r['n'] == 51 and r['exact'] == 51, r)
        r = s.plugin('diag-snap', {'geometry': polygon(ring), 'nudge': 1e-6})
        check(r['first_exact'] and r['exact'] == 51, r)
        r = s.plugin('diag-snap', {'geometry': polygon(ring), 'nudge': 1e-3})
        check(not r['first_exact'] and r['exact'] == 49, r)


def test_settings_from_panel_params_and_null():
    # `rustgis-cli run --cmd id` with no `=` passes no parameters, as the ribbon does.
    code, out = run_once([('plugin:advanced-editing/diag-settings', None)])
    check(code == 0 and '"paramsNull": true' in out, out[-600:])
    check('"distance": 1.0' in out and '"keepTools": true' in out, out[-600:])
    with Session() as s:
        r = s.plugin('diag-settings', {'panel': {'distance': 10, 'percent': 25, 'keep-tools': False}})
        check(r['distance'] == 5.0 and r['percent'] == 25 and r['keepTools'] is False, r)
        # panel values persist for the next call; parameters override them
        r = s.plugin('diag-settings', {'distance': 0.3, 'bearing': 370})
        check(r['distance'] == 0.3 and r['percent'] == 25 and r['bearing'] == 10, r)
        check(r['sum'] is False and r['reverse'] is False, r)
        r = s.plugin('diag-settings', {'panel': {'merge-sum': True, 'divide-reverse': True}})
        check(r['sum'] is True and r['reverse'] is True, r)
        r = s.plugin('diag-settings', {'sum': False, 'reverse': False})
        check(r['sum'] is False and r['reverse'] is False, r)
        # every Settings field is reported
        check(set(r) == {'distance', 'tolerance', 'iterations', 'parts', 'percent', 'angle', 'bearing', 'keepTools',
                         'left', 'wholeLayer', 'move', 'sum', 'reverse', 'topological', 'paramsNull'}, sorted(r))
        # topological: on by default, off from the panel, a script parameter overrides the panel
        check(r['topological'] is True, r)
        r = s.plugin('diag-settings', {'panel': {'topological': False}})
        check(r['topological'] is False, r)
        r = s.plugin('diag-settings', {'topological': True})
        check(r['topological'] is True, r)
        r = s.plugin('diag-settings', {})
        check(r['topological'] is False, r)
        st = s.plugin('panel-state')
        check(st['panel']['items']['distance']['labels'][7] == '1 m', st)


def _two_layers():
    parcels = grid_layer('parcels', nx=3)
    lon, lat = PL
    cutter = write_layer('cutter', [feature(polygon(rect(lon, lat, 120, 50, 40, 25)), name='cut')])
    lines = write_layer('lines', [feature(linestring(lon, lat, [(-10, 50), (310, 50)]), name='l')])
    return parcels, cutter, lines


def test_selection_model():
    with Session(*_two_layers()) as s:
        poly = ['polygon']
        # tools in the active layer, targets selected in another layer
        s.select('parcels', [1, 2])
        s.activate('cutter')
        s.select('cutter', [0], add=True)
        r = s.plugin('diag-selection', {'toolDims': poly, 'targetDims': poly})
        check(r['active'] == 'cutter' and r['tools'] == [1] and not r['fromActive'], r)
        check(r['targets'] == [{'layer': 'parcels', 'ids': [2, 3]}], r)
        check(r['frame'] == 'EPSG:2180', r)
        # the whole-layer option takes targets from the active layer (only the cutter there)
        e = s.error_of('diag-selection', {'toolDims': poly, 'targetDims': poly, 'whole': True})
        check('touch the selected tool' in e and 'Select the tool features' in e, e)
        # no selection elsewhere: targets = active-layer features touching the tools
        s.activate('parcels')
        s.select('parcels', [0])
        r = s.plugin('diag-selection', {'toolDims': poly, 'targetDims': poly})
        check(r['fromActive'] and r['tools'] == [1] and r['targets'] == [{'layer': 'parcels', 'ids': [2]}], r)
        # a line tool against polygon targets: the dimension of the active layer decides
        s.activate('lines')
        s.select('lines', [0])
        s.select('parcels', [0, 1, 2], add=True)
        r = s.plugin('diag-selection', {'toolDims': ['line'], 'targetDims': poly, 'maxTools': 1})
        check(r['tools'] == [1] and r['targets'][0]['ids'] == [1, 2, 3], r)
        e = s.error_of('diag-selection', {'toolDims': poly, 'targetDims': poly})
        check("holds lines" in e, e)
        # nothing selected in the active layer
        s.clear_selection()
        e = s.error_of('diag-selection', {'toolDims': ['line'], 'targetDims': poly})
        check('nothing is selected' in e, e)
        # edit mode: the active layer's selection, or all of it
        s.activate('parcels')
        s.select('parcels', [0, 2])
        check(s.plugin('diag-selection', {'mode': 'edit', 'dims': poly})['ids'] == [1, 3])
        check(s.plugin('diag-selection', {'mode': 'edit', 'dims': poly, 'whole': True})['ids'] == [1, 2, 3])


def test_utm_frame_abroad():
    with Session(grid_layer('abroad', origin=ABROAD)) as s:
        s.activate('abroad')
        s.select('abroad', [0])
        r = s.plugin('diag-selection', {'mode': 'edit', 'dims': ['polygon']})
        check(r['frame'] == 'UTM 32N', r)


def test_read_cap_is_by_size():
    # 1001 small squares are ~0.2 MB: read whole (the guard counts bytes, not features; see
    # test_limits.py for a layer that is too big)
    lon, lat = PL
    feats = [feature(polygon(rect(lon, lat, 1, 1, 2 * (i % 40), 2 * (i // 40))), n=i) for i in range(1001)]
    with Session(write_layer('big', feats)) as s:
        s.activate('big')
        r = s.plugin('diag-selection', {'mode': 'edit', 'dims': ['polygon'], 'whole': True})
        check(len(r['ids']) == 1001, len(r['ids']))


def test_set_selection_after_delete():
    with Session(*_two_layers()) as s:
        parcels, cutter = s.layer_id('parcels'), s.layer_id('cutter')
        # after a delete nothing is selected: an empty selection must not fail
        s.select('parcels', [1])
        r = s.plugin('diag-delete', {'layer': parcels, 'rows': [0], 'select': []})
        check(r['deleted'] == 1, r)
        check(s.selected_ids('parcels') == [] and s.selected_ids('cutter') == [])
        # a selection in two layers replaces whatever was selected before
        s.select('lines', [0])
        r = s.plugin('diag-delete', {'layer': parcels, 'rows': [], 'select': [[parcels, [1]], [cutter, [0]]]})
        check(s.selected_ids('parcels') == [3] and s.selected_ids('cutter') == [1], 'wrong selection')
        check(s.selected_ids('lines') == [], 'the old selection stayed')
        # an empty selection clears an existing one
        s.plugin('diag-delete', {'layer': parcels, 'rows': [], 'select': [[cutter, []]]})
        check(s.selected_ids('parcels') == [] and s.selected_ids('cutter') == [], 'not cleared')


def test_safe_delete_only_touches_the_target_layer():
    with Session(*_two_layers()) as s:
        s.select('parcels', [1])
        s.select('cutter', [0], add=True)
        r = s.plugin('diag-delete', {'layer': s.layer_id('parcels'), 'rows': [2, 0, 2]})
        check(r['deleted'] == 2, r)
        # no plugin command before the undo: each one is an undo step of its own
        check([f['properties']['name'] for f in s.features('parcels')] == ['c1_0'])
        check(len(s.features('cutter')) == 1)
        s.undo()
        check(len(s.features('parcels')) == 3)


def test_add_feature_row_tracking():
    with Session(grid_layer('parcels', nx=3)) as s:
        lid = s.layer_id('parcels')
        r = s.plugin('diag-add', {'layer': lid, 'count': 3, 'deleteRows': [0, 3]})
        check(r['rows'] == [3, 4, 5], r)
        check(r['predicted'] == r['actual'] == [None, 2, 3], r)
        check(r['features'] == 4, r)
        names = [f['properties']['name'] for f in s.features('parcels')]
        check(names == ['c1_0', 'c2_0', 'added-1', 'added-2'], names)


def test_failing_command_rolls_back():
    with Session(grid_layer('parcels', nx=3)) as s:
        lid = s.layer_id('parcels')
        try:
            s.plugin('diag-delete', {'layer': lid, 'rows': [0, 99]})
            raise AssertionError('deleting a missing row succeeded')
        except CommandError:
            pass
        check(s.layers()['parcels']['features'] == 3)


def test_redo_restores_an_undone_plugin_command():
    with Session(grid_layer('parcels', nx=3)) as s:
        lid = s.layer_id('parcels')
        s.plugin('diag-delete', {'layer': lid, 'rows': [0]})
        check(len(s.features('parcels')) == 2)
        s.undo()
        check(len(s.features('parcels')) == 3)
        s.redo()
        check(len(s.features('parcels')) == 2)
