"""Tests for src/attributes.rs: merge with attributes, exchange attributes, copy-move,
simplify, densify, smooth."""

import math
import os

from aelib import (PL, Session, Skip, check, close, feature, linestring, offset_m, point, polygon,
                   polygon_area_m2, rect, write_layer, M_PER_DEG_LAT)

# Commands of this module that are still stubs: remove each one as it is implemented.
STUBS = []

EPS = 1e-12


def same_coords(a, b, eps=EPS):
    """Two GeoJSON coordinate trees agree within eps."""
    if isinstance(a, (int, float)):
        return abs(a - b) <= eps
    return len(a) == len(b) and all(same_coords(x, y, eps) for x, y in zip(a, b))


def same_geom(a, b):
    return a['type'] == b['type'] and same_coords(a['coordinates'], b['coordinates'])


def flat(geom):
    """All positions of a geometry."""
    out = []

    def walk(c):
        if isinstance(c[0], (int, float)):
            out.append(c)
        else:
            for x in c:
                walk(x)
    walk(geom['coordinates'])
    return out


def shift_m(a, b):
    """(dx, dy) metres from lon/lat position a to b (local equirectangular)."""
    k = M_PER_DEG_LAT * math.cos(math.radians(a[1]))
    return (b[0] - a[0]) * k, (b[1] - a[1]) * M_PER_DEG_LAT


def parcels(name='parcels'):
    """Four adjacent rectangles with text, integer and double fields; row 1 is the largest."""
    lon, lat = PL
    specs = [(0, 100, 'a', 1, 1.5), (100, 150, 'b', 2, 2.5), (250, 100, 'c', 3, None), (350, 100, 'd', 4, 4.5)]
    return write_layer(name, [feature(polygon(rect(lon, lat, w, 100, x0, 0)), name=n, n=i, w=d)
                              for x0, w, n, i, d in specs])


def props(f):
    return (f['properties']['name'], f['properties']['n'], f['properties']['w'])


def snapshot(s, layer):
    return [(f['id'], props(f), f['geometry']) for f in s.features(layer)]


def unchanged(before, after):
    return len(before) == len(after) and all(
        a[0] == b[0] and a[1] == b[1] and same_geom(a[2], b[2]) for a, b in zip(before, after))


def selected_then_undo(s, lid):
    """The selected ids of layer `lid`, then undoes the edit before. Reading the selection is a
    plugin command, and every plugin command is an undo step of its own: two undos."""
    ids = s.plugin('diag-features', {'layer': lid, 'selected': True})['ids']
    s.undo()
    s.undo()
    return ids


def test_stubs_answer_not_implemented():
    if STUBS:
        from aelib import assert_stubs
        assert_stubs(STUBS)


# ---------- merge with attributes ----------

def test_merge_polygons_keeps_the_largest_attributes():
    with Session(parcels()) as s:
        lid = s.layer_id('parcels')
        before = s.features('parcels')
        s.activate('parcels')
        s.select('parcels', [0, 1, 2])
        r = s.plugin('merge-with-attributes')
        check(r['merged'] == 3 and r['deleted'] == 2 and r['kept'] == before[1]['id'], r)
        after = s.features('parcels')
        check(len(after) == 2, len(after))
        m = after[0]
        check(m['id'] == before[1]['id'] and props(m) == ('b', 2, 2.5), m['properties'])
        check(close(polygon_area_m2(m['geometry']), 350 * 100, 5), polygon_area_m2(m['geometry']))
        check(m['geometry']['type'] == 'Polygon', m['geometry']['type'])
        # merged outline keeps the original corner coordinates exactly
        corners = flat(before[0]['geometry'])[:1] + flat(before[2]['geometry'])[1:3]
        out = flat(m['geometry'])
        check(all(any(same_coords(c, o) for o in out) for c in corners), 'corners moved')
        check(same_geom(after[1]['geometry'], before[3]['geometry']), 'the unselected neighbour changed')
        sel = selected_then_undo(s, lid)
        check(sel == [before[1]['id']], sel)
        check(unchanged([(f['id'], props(f), f['geometry']) for f in before], snapshot(s, 'parcels')),
              'undo did not restore')


def test_merge_tie_keeps_the_lowest_id():
    with Session(parcels()) as s:
        before = s.features('parcels')
        s.activate('parcels')
        s.select('parcels', [3, 2])
        r = s.plugin('merge-with-attributes')
        check(r['kept'] == before[2]['id'], r)
        after = s.features('parcels')
        check(len(after) == 3 and props(after[2]) == ('c', 3, None), [props(f) for f in after])


def test_merge_lines_joins_touching_ends():
    lon, lat = PL
    lines = write_layer('roads', [
        feature(linestring(lon, lat, [(0, 0), (100, 0)]), name='west', n=1, w=1.0),
        feature(linestring(lon, lat, [(100, 50), (100, 0)]), name='north', n=2, w=2.0),
        feature(linestring(lon, lat, [(200, 0), (360, 0)]), name='far', n=3, w=3.0),
    ])
    with Session(lines) as s:
        before = s.features('roads')
        s.activate('roads')
        s.select('roads', [0, 1, 2])
        r = s.plugin('merge-with-attributes')
        check(r['kept'] == before[2]['id'], r)
        after = s.features('roads')
        check(len(after) == 1 and props(after[0]) == ('far', 3, 3.0), after)
        g = after[0]['geometry']
        check(g['type'] == 'MultiLineString' and len(g['coordinates']) == 2, g)
        joined = [p for p in g['coordinates'] if len(p) == 3]
        check(len(joined) == 1, g['coordinates'])
        w, n = before[0]['geometry']['coordinates'], before[1]['geometry']['coordinates']
        check(same_coords(joined[0], [w[0], w[1], n[0]]), joined)
        s.undo()
        check(len(s.features('roads')) == 3)


def test_merge_sums_numbers():
    with Session(parcels()) as s:
        before = s.features('parcels')
        s.activate('parcels')
        s.select('parcels', [0, 1, 2])
        r = s.plugin('merge-with-attributes', {'sum': True})
        check(r['kept'] == before[1]['id'] and r['summed'] == 2, r)
        m = s.features('parcels')[0]
        # text from the largest feature, integers stay integers, a null counts as 0
        check(props(m) == ('b', 6, 4.0) and isinstance(m['properties']['n'], int), m['properties'])
    lon, lat = PL
    data = write_layer('nulls', [feature(polygon(rect(lon, lat, w, 100, x0, 0)), n=n, w=x)
                                 for x0, w, n, x in [(0, 100, 1, None), (100, 150, None, None), (300, 100, 5, 2.5)]])
    with Session(data) as s:
        s.activate('nulls')
        s.select('nulls', [0, 1])
        r = s.plugin('merge-with-attributes', {'panel': {'merge-sum': True}})
        f = s.features('nulls')[0]['properties']
        # the largest feature's null n becomes 1; w is null everywhere and stays null
        check(r['summed'] == 2, r)
        check(f['n'] == 1 and f['w'] is None, f)


def test_merge_needs_two_features():
    with Session(parcels()) as s:
        before = snapshot(s, 'parcels')
        s.activate('parcels')
        s.select('parcels', [0])
        e = s.error_of('merge-with-attributes')
        check('two or more' in e, e)
        check(unchanged(before, snapshot(s, 'parcels')), 'data changed')


# ---------- exchange attributes ----------

def test_exchange_attributes_swaps_values():
    with Session(parcels()) as s:
        before = s.features('parcels')
        s.activate('parcels')
        s.select('parcels', [1, 2])
        r = s.plugin('exchange-attributes')
        check(r['fields'] == 3, r)
        after = s.features('parcels')
        check(props(after[1]) == ('c', 3, None) and props(after[2]) == ('b', 2, 2.5),
              [props(f) for f in after])
        check([f['id'] for f in after] == [f['id'] for f in before], 'ids changed')
        check(all(same_geom(a['geometry'], b['geometry']) for a, b in zip(before, after)), 'geometry changed')
        check(props(after[0]) == props(before[0]) and props(after[3]) == props(before[3]), 'others changed')
        s.undo()
        check([props(f) for f in s.features('parcels')] == [props(f) for f in before], 'undo did not restore')


def test_exchange_needs_exactly_two():
    with Session(parcels()) as s:
        before = snapshot(s, 'parcels')
        s.activate('parcels')
        s.select('parcels', [0, 1, 2])
        e = s.error_of('exchange-attributes')
        check('exactly two' in e, e)
        check(unchanged(before, snapshot(s, 'parcels')), 'data changed')


# ---------- copy / move ----------

def test_copy_moves_a_copy_along_the_bearing():
    with Session(parcels()) as s:
        lid = s.layer_id('parcels')
        before = s.features('parcels')
        s.activate('parcels')
        s.select('parcels', [0, 2])
        r = s.plugin('copy-move', {'distance': 25, 'bearing': 90, 'move': False})
        check(r['copied'] == 2, r)
        after = s.features('parcels')
        check(len(after) == 6, len(after))
        check(all(same_geom(a['geometry'], b['geometry']) for a, b in zip(before, after)), 'originals moved')
        for src, new in ((before[0], after[4]), (before[2], after[5])):
            check(props(new) == props(src), (props(new), props(src)))
            for a, b in zip(flat(src['geometry']), flat(new['geometry'])):
                dx, dy = shift_m(a, b)
                check(close(dx, 25, 0.05) and close(dy, 0, 0.05), (dx, dy))
            check(close(polygon_area_m2(new['geometry']), polygon_area_m2(src['geometry']), 1), 'area changed')
        sel = selected_then_undo(s, lid)
        check(sel == [after[4]['id'], after[5]['id']], sel)
        check(len(s.features('parcels')) == 4)


def test_move_moves_the_original():
    lon, lat = PL
    pts = write_layer('pts', [feature(point(lon, lat, 0, 0), name='p', n=1, w=0.5),
                              feature(point(lon, lat, 50, 0), name='q', n=2, w=None)])
    with Session(pts) as s:
        before = s.features('pts')
        s.activate('pts')
        s.select('pts', [1])
        r = s.plugin('copy-move', {'distance': 40, 'bearing': 0, 'move': True})
        check(r['moved'] == 1, r)
        after = s.features('pts')
        check(len(after) == 2 and same_geom(after[0]['geometry'], before[0]['geometry']))
        dx, dy = shift_m(before[1]['geometry']['coordinates'], after[1]['geometry']['coordinates'])
        check(close(dx, 0, 0.05) and close(dy, 40, 0.05), (dx, dy))
        check(props(after[1]) == ('q', 2, None))
        s.undo()
        check(same_geom(s.features('pts')[1]['geometry'], before[1]['geometry']), 'undo did not restore')


def test_copy_move_needs_a_selection():
    with Session(parcels()) as s:
        before = snapshot(s, 'parcels')
        s.activate('parcels')
        e = s.error_of('copy-move', {'distance': 5})
        check('Select the features to copy or move' in e and 'nothing is selected' in e, e)
        check(unchanged(before, snapshot(s, 'parcels')), 'data changed')


# ---------- simplify ----------

def noisy_square(lon, lat, size, x0, n_per_side=10, noise=0.05):
    """A square ring with extra vertices along its sides, alternately `noise` metres off."""
    pts = []
    corners = [(x0, 0), (x0 + size, 0), (x0 + size, size), (x0, size)]
    for k in range(4):
        (ax, ay), (bx, by) = corners[k], corners[(k + 1) % 4]
        nx, ny = (by - ay) / size, -(bx - ax) / size
        for i in range(n_per_side):
            t = i / n_per_side
            off = 0 if i == 0 else noise * (1 if i % 2 else -1)
            pts.append((ax + (bx - ax) * t + nx * off, ay + (by - ay) * t + ny * off))
    pts.append(pts[0])
    return [offset_m(lon, lat, x, y) for x, y in pts]


def generalize_layer():
    lon, lat = PL
    hole = list(reversed(rect(lon, lat, 0.3, 0.3, 50, 50)))
    return write_layer('shapes', [
        feature(polygon(noisy_square(lon, lat, 100, 0), hole), name='noisy', n=1, w=1.0),
        feature(polygon(rect(lon, lat, 100, 100, 100, 0)), name='neighbour', n=2, w=2.0),
        feature(polygon(rect(lon, lat, 0.2, 0.2, 300, 0)), name='tiny', n=3, w=3.0),
    ])


def test_simplify_removes_noise_and_collapsed_rings():
    with Session(generalize_layer()) as s:
        lid = s.layer_id('shapes')
        before = s.features('shapes')
        s.activate('shapes')
        s.select('shapes', [0, 2])
        r = s.plugin('simplify', {'tolerance': 0.5})
        check(r['changed'] == 1 and r['collapsed'] == 1, r)
        check('collapse' in r['message'] and 'shared with neighbours' in r['message'], r['message'])
        after = s.features('shapes')
        g = after[0]['geometry']
        check(g['type'] == 'Polygon' and len(g['coordinates']) == 1, 'the collapsed hole was kept')
        check(len(g['coordinates'][0]) == 5, len(g['coordinates'][0]))
        orig = flat(before[0]['geometry'])
        check(all(any(same_coords(c, o) for o in orig) for c in flat(g)), 'kept vertices moved')
        check(same_geom(after[1]['geometry'], before[1]['geometry']), 'the neighbour changed')
        check(same_geom(after[2]['geometry'], before[2]['geometry']), 'the collapsing feature changed')
        check(props(after[0]) == props(before[0]), 'attributes changed')
        sel = selected_then_undo(s, lid)
        check(sel == [before[0]['id']], sel)
        check(same_geom(s.features('shapes')[0]['geometry'], before[0]['geometry']), 'undo did not restore')


def test_simplify_leaves_a_feature_that_would_turn_invalid():
    # Simplifying drops the 0.4 m bump of the outline, and the hole reaching into it would
    # then cross the outline.
    lon, lat = PL
    shell = [offset_m(lon, lat, x, y) for x, y in [(0, 0), (50, -0.4), (100, 0), (100, 100), (0, 100), (0, 0)]]
    hole = [offset_m(lon, lat, x, y) for x, y in [(45, 1), (55, 1), (50, -0.2), (45, 1)]]
    with Session(write_layer('dent', [feature(polygon(shell, hole), name='d', n=1, w=1.0)])) as s:
        before = s.features('dent')
        s.activate('dent')
        s.select('dent', [0])
        r = s.plugin('simplify', {'tolerance': 0.5})
        check(r['changed'] == 0 and r['invalid'] == 1 and 'invalid' in r['message'], r)
        check(same_geom(s.features('dent')[0]['geometry'], before[0]['geometry']), 'the feature changed')


def test_simplify_refuses_points():
    lon, lat = PL
    with Session(write_layer('pts', [feature(point(lon, lat), name='p', n=1, w=1.0)])) as s:
        s.activate('pts')
        s.select('pts', [0])
        e = s.error_of('simplify')
        check('Select the lines or polygons to simplify' in e and 'holds points' in e, e)


# ---------- densify ----------

def test_densify_splits_long_segments():
    lon, lat = PL
    data = write_layer('mixed', [
        feature(linestring(lon, lat, [(0, 0), (95, 0), (95, 35)]), name='l', n=1, w=1.0),
        feature(linestring(lon, lat, [(0, 10), (5, 10)]), name='short', n=2, w=2.0),
    ])
    with Session(data, generalize_layer()) as s:
        before = s.features('mixed')
        s.activate('mixed')
        s.select('mixed', [0, 1])
        r = s.plugin('densify', {'distance': 10})
        check(r['changed'] == 1 and r['unchanged'] == 1, r)
        g = s.features('mixed')[0]['geometry']
        pts = g['coordinates']
        check(len(pts) == 11 + 4, len(pts))
        for c in before[0]['geometry']['coordinates']:
            check(any(same_coords(c, p) for p in pts), 'an original vertex moved')
        for a, b in zip(pts, pts[1:]):
            dx, dy = shift_m(a, b)
            check(math.hypot(dx, dy) <= 10.05, math.hypot(dx, dy))
        check(same_geom(s.features('mixed')[1]['geometry'], before[1]['geometry']))
        s.undo()
        check(same_geom(s.features('mixed')[0]['geometry'], before[0]['geometry']), 'undo did not restore')
        # polygons: rings stay closed (the 100 m square is a bit over 100 m east-west in
        # EPSG:2180, so 30 m spacing gives 4 segments per side)
        s.activate('shapes')
        s.select('shapes', [1])
        s.plugin('densify', {'distance': 30})
        ring = s.features('shapes')[1]['geometry']['coordinates'][0]
        check(len(ring) == 17 and ring[0] == ring[-1], len(ring))


def test_densify_refuses_too_many_vertices():
    lon, lat = PL
    with Session(write_layer('long', [feature(linestring(lon, lat, [(0, 0), (5000, 0)]), name='l', n=1, w=1.0)])) as s:
        before = s.features('long')
        s.activate('long')
        s.select('long', [0])
        e = s.error_of('densify', {'distance': 0.01})
        check('one command writes about 41 000 vertices' in e, e)
        check(same_geom(s.features('long')[0]['geometry'], before[0]['geometry']))
        s.clear_selection()
        e = s.error_of('densify', {'distance': 1})
        check('nothing is selected' in e, e)


def test_densify_write_guard_on_both_sides():
    # budget::check_write: about 41 600 written vertices (6000 fuel each in half the budget)
    lon, lat = PL
    lines = write_layer('long', [feature(linestring(lon, lat, [(0, 0), (4150, 0)]), name='a'),
                                 feature(linestring(lon, lat, [(0, 50), (4180, 50)]), name='b')])
    with Session(lines) as s:
        before = s.features('long')
        s.activate('long')
        s.select('long', [1])
        e = s.error_of('densify', {'distance': 0.1})
        check('The result would be 1 features with about 41 8' in e and 'one command writes about 41 000 vertices' in e, e)
        check(s.features('long') == before, 'a refused densify changed the data')
        s.select('long', [0])
        r = s.plugin('densify', {'distance': 0.1})
        check(r['changed'] == 1 and len(s.features('long')[0]['geometry']['coordinates']) > 41_500, r)


# ---------- smooth ----------

def test_smooth_keeps_line_ends_and_closes_rings():
    lon, lat = PL
    data = write_layer('lines', [feature(linestring(lon, lat, [(0, 0), (50, 50), (100, 0)]), name='v', n=1, w=1.0)])
    with Session(data, generalize_layer()) as s:
        before = s.features('lines')
        s.activate('lines')
        s.select('lines', [0])
        r = s.plugin('smooth', {'iterations': 2})
        check(r['changed'] == 1, r)
        pts = s.features('lines')[0]['geometry']['coordinates']
        orig = before[0]['geometry']['coordinates']
        check(len(pts) == 12, len(pts))
        check(same_coords(pts[0], orig[0]) and same_coords(pts[-1], orig[-1]), 'line ends moved')
        s.undo()
        check(same_geom(s.features('lines')[0]['geometry'], before[0]['geometry']), 'undo did not restore')

        shapes = s.features('shapes')
        s.activate('shapes')
        s.select('shapes', [1])
        r = s.plugin('smooth', {'iterations': 2})
        check('shared with neighbours' in r['message'], r['message'])
        after = s.features('shapes')
        ring = after[1]['geometry']['coordinates'][0]
        check(len(ring) == 17 and ring[0] == ring[-1], len(ring))
        a = polygon_area_m2(after[1]['geometry'])
        check(7000 < a < 10000, a)
        check(same_geom(after[0]['geometry'], shapes[0]['geometry']), 'the neighbour changed')


def test_smooth_needs_lines_or_polygons_selected():
    with Session(generalize_layer()) as s:
        before = snapshot(s, 'shapes')
        s.activate('shapes')
        e = s.error_of('smooth')
        check('Select the lines or polygons to smooth' in e, e)
        check(unchanged(before, snapshot(s, 'shapes')), 'data changed')


# ---------- real data ----------

def test_real_simplify_one_parcel():
    path = os.environ.get('AE_REAL_DATA')
    if not path:
        raise Skip('AE_REAL_DATA is not set')
    with Session(path) as s:
        layer = next((l for l in s.layers().values() if l['dim'] == 'polygon'), None)
        if not layer:
            raise Skip('no polygon layer in %s' % path)
        name = layer['name']
        before = s.features(name)
        row = max(range(len(before)), key=lambda i: len(flat(before[i]['geometry'])))
        s.activate(name)
        s.select(name, [row])
        r = s.plugin('simplify', {'tolerance': 0.1})
        after = s.features(name)
        check(len(after) == len(before), 'feature count changed')
        others = [i for i in range(len(before)) if i != row]
        check(all(same_geom(before[i]['geometry'], after[i]['geometry']) for i in others), 'a neighbour changed')
        a0, a1 = polygon_area_m2(before[row]['geometry']), polygon_area_m2(after[row]['geometry'])
        check(abs(a1 - a0) / a0 < 0.01, (a0, a1, r))
        if r['changed']:
            check(len(flat(after[row]['geometry'])) < len(flat(before[row]['geometry'])), r)
        s.undo()
        check(same_geom(s.features(name)[row]['geometry'], before[row]['geometry']), 'undo did not restore')
