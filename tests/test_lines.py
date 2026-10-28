"""Tests for src/lines.rs: split with line, reshape, offset curve, trim/extend.

Geometric checks measure in EPSG:2180 metres (through `diag-proj`), the frame the commands
work in, so lengths, areas and distances can be compared tightly."""

import math
import os
import time

from aelib import (PL, Session, Skip, check, close, feature, grid_layer, holed_layer, linestring,
                   multipolygon, offset_m, polygon, rect, write_layer)

# Commands of this module that are still stubs: remove each one as it is implemented.
STUBS = []


# ---------- helpers ----------

def coords_of(g):
    """Every position of a GeoJSON geometry, flattened."""
    t, c = g['type'], g['coordinates']
    if t == 'Point':
        return [c]
    if t in ('LineString', 'MultiPoint'):
        return list(c)
    if t in ('MultiLineString', 'Polygon'):
        return [p for part in c for p in part]
    return [p for poly in c for ring in poly for p in ring]


class Metric:
    """Projects lon/lat geometries to EPSG:2180 with the plugin's own projection. Each call
    is a plugin command (an undo step): don't call it between an edit and `undo()`."""

    def __init__(self, s):
        self.s = s

    def pts(self, positions):
        if not positions:
            return []
        out = self.s.plugin('diag-proj', {'points': positions})['points']
        return [(o['x'], o['y']) for o in out]

    def geom(self, g):
        """The geometry with metric coordinates, same nesting."""
        flat = iter(self.pts(coords_of(g)))
        t, c = g['type'], g['coordinates']
        if t == 'LineString':
            return {'type': t, 'coordinates': [next(flat) for _ in c]}
        if t in ('MultiLineString', 'Polygon'):
            return {'type': t, 'coordinates': [[next(flat) for _ in part] for part in c]}
        if t == 'MultiPolygon':
            return {'type': t, 'coordinates': [[[next(flat) for _ in r] for r in poly] for poly in c]}
        raise ValueError(t)


def ring_area(r):
    return abs(sum(a[0] * b[1] - b[0] * a[1] for a, b in zip(r, r[1:]))) / 2


def area(mg):
    polys = [mg['coordinates']] if mg['type'] == 'Polygon' else mg['coordinates']
    return sum(ring_area(p[0]) - sum(ring_area(h) for h in p[1:]) for p in polys)


def parts(mg):
    return [mg['coordinates']] if mg['type'] == 'LineString' else mg['coordinates']


def length(mg):
    return sum(math.dist(a, b) for p in parts(mg) for a, b in zip(p, p[1:]))


def seg_dist(q, a, b):
    dx, dy = b[0] - a[0], b[1] - a[1]
    l2 = dx * dx + dy * dy
    t = 0.0 if l2 == 0 else max(0.0, min(1.0, ((q[0] - a[0]) * dx + (q[1] - a[1]) * dy) / l2))
    return math.dist(q, (a[0] + t * dx, a[1] + t * dy))


def dist_to_line(q, line):
    return min(seg_dist(q, a, b) for a, b in zip(line, line[1:]))


def on_side(q, line, want, tol=1e-6):
    """Whether q lies on the `want` side (+1 left, -1 right) of a nearest segment (any of
    them where the line comes back near itself). Segments q lies beyond the end of (at an
    open line's ends) do not tell a side."""
    closed = line[0] == line[-1]
    ds = []
    for k, (a, b) in enumerate(zip(line, line[1:])):
        dx, dy = b[0] - a[0], b[1] - a[1]
        if dx == 0 and dy == 0:
            continue
        t = ((q[0] - a[0]) * dx + (q[1] - a[1]) * dy) / (dx * dx + dy * dy)
        if not closed and ((k == 0 and t < -1e-9) or (k == len(line) - 2 and t > 1 + 1e-9)):
            continue
        ds.append((seg_dist(q, a, b), a, b))
    best = min(d for d, _, _ in ds)
    for d, a, b in ds:
        if d <= best + tol:
            c = (b[0] - a[0]) * (q[1] - a[1]) - (b[1] - a[1]) * (q[0] - a[0])
            if c * want > 0:
                return True
    return False


def segments_cross(line):
    """Whether two non-adjacent segments of a path cross (a closed path's closing pair aside)."""
    n = len(line) - 1
    closed = line[0] == line[-1]

    def inter(p1, p2, p3, p4):
        d = (p2[0] - p1[0]) * (p4[1] - p3[1]) - (p2[1] - p1[1]) * (p4[0] - p3[0])
        if d == 0:
            return False
        t = ((p3[0] - p1[0]) * (p4[1] - p3[1]) - (p3[1] - p1[1]) * (p4[0] - p3[0])) / d
        u = ((p3[0] - p1[0]) * (p2[1] - p1[1]) - (p3[1] - p1[1]) * (p2[0] - p1[0])) / d
        return 1e-9 < t < 1 - 1e-9 and 1e-9 < u < 1 - 1e-9

    for i in range(n):
        for j in range(i + 2, n):
            if closed and i == 0 and j == n - 1:
                continue
            if inter(line[i], line[i + 1], line[j], line[j + 1]):
                return True
    return False


def same_geoms(a, b, eps=1e-11):
    if len(a) != len(b):
        return False
    for fa, fb in zip(a, b):
        ca, cb = coords_of(fa['geometry']), coords_of(fb['geometry'])
        if fa['geometry']['type'] != fb['geometry']['type'] or len(ca) != len(cb):
            return False
        if any(abs(p[0] - q[0]) > eps or abs(p[1] - q[1]) > eps for p, q in zip(ca, cb)):
            return False
    return True


def lines_layer(name, lines, **props):
    lon, lat = PL
    return write_layer(name, [feature(linestring(lon, lat, pts), name='%s%d' % (name, i), **props)
                              for i, pts in enumerate(lines)])


def has_vertex(g, p, eps=1e-11):
    return any(abs(q[0] - p[0]) <= eps and abs(q[1] - p[1]) <= eps for q in coords_of(g))


def test_stubs_answer_not_implemented():
    if STUBS:
        from aelib import assert_stubs
        assert_stubs(STUBS)


# ---------- split with line ----------

def test_split_polygons_with_line():
    cut = lines_layer('cut', [[(-10, 50), (310, 50)]])
    with Session(grid_layer('parcels', nx=3), cut) as s:
        m = Metric(s)
        before = s.features('parcels')
        total = sum(area(m.geom(f['geometry'])) for f in before)
        s.select('parcels', [0, 1, 2])
        s.activate('cut')
        s.select('cut', [0], add=True)
        r = s.plugin('split-with-line')
        check(r['split'] == 3 and r['created'] == 3, r)
        after = s.features('parcels')
        s.undo()
        check(same_geoms(s.features('parcels'), before), 'undo did not restore the parcels')
        s.redo()
        check(same_geoms(s.features('parcels'), after), 'redo did not split again')
        check(len(after) == 6, len(after))
        check([f['properties']['name'] for f in after] == ['c0_0', 'c1_0', 'c2_0', 'c0_0', 'c1_0', 'c2_0'], after)
        check(len(s.features('cut')) == 1, 'the cutter was kept')
        areas = [area(m.geom(f['geometry'])) for f in after]
        check(abs(sum(areas) - total) <= 1e-6 * total, (sum(areas), total))
        check(all(close(a, total / 6, 0.01 * total / 6) for a in areas), areas)
        # every original corner is still there, bit-exact
        for f in before:
            for p in coords_of(f['geometry']):
                check(any(has_vertex(g['geometry'], p) for g in after), 'lost corner %s' % p)


def test_split_keeps_largest_and_unsplit_parts():
    lon, lat = PL
    mp = multipolygon(polygon(rect(lon, lat, 40, 40)), polygon(rect(lon, lat, 40, 40, 100, 0)))
    data = write_layer('mp', [feature(mp, name='mp'), feature(polygon(rect(lon, lat, 100, 100, 200, 0),
                                                                      list(reversed(rect(lon, lat, 40, 40, 230, 30)))),
                                                              name='holed')])
    cut = lines_layer('cut', [[(10, -10), (10, 50)], [(250, -10), (250, 110)]])
    with Session(data, cut) as s:
        m = Metric(s)
        s.select('mp', [0, 1])
        s.activate('cut')
        s.select('cut', [0, 1], add=True)
        s.plugin('split-with-line', {'keepTools': False})
        after = s.features('mp')
        check(len(after) == 4, after)
        g = [m.geom(f['geometry']) for f in after]
        # multipolygon: the 30 x 40 piece stays with the untouched part, the 10 x 40 one is new
        check(g[0]['type'] == 'MultiPolygon' and len(g[0]['coordinates']) == 2, g[0]['type'])
        a = [area(x) for x in g]
        check(close(a[0] / a[2], 2800 / 400, 0.01), a)
        # the holed square is cut through its hole into two equal halves
        check(close(a[1], a[3], 1e-3 * a[1]) and close(a[1] / a[2], 4200 / 400, 0.01), a)
        check(len(s.features('cut')) == 0, 'the cutters were not deleted')
        check(sorted(s.selected_ids('mp')) == [f['id'] for f in after], 'selection')


def test_split_lines_and_closed_line():
    lines = lines_layer('roads', [[(0, 0), (300, 0)], [(0, 20), (50, 40), (100, 20), (150, 40), (200, 20)],
                                  [(0, 100), (100, 100), (100, 200), (0, 200), (0, 100)]])
    cut = lines_layer('cut', [[(120, -10), (120, 210)], [(220, -10), (220, 10)], [(50, 90), (50, 210)]])
    with Session(lines, cut) as s:
        m = Metric(s)
        before = [m.geom(f['geometry']) for f in s.features('roads')]
        s.select('roads', [0, 1, 2])
        s.activate('cut')
        s.select('cut', [0, 1, 2], add=True)
        r = s.plugin('split-with-line')
        check(r['split'] == 3 and r['created'] == 4, r)
        after = s.features('roads')
        g = [m.geom(f['geometry']) for f in after]
        check(close(sum(map(length, g)), sum(map(length, before)), 1e-6), 'length changed')
        # straight line cut at 120 and 220: the longest piece (120) stays in the feature
        check(close(length(g[0]), length(before[0]) * 120 / 300, 0.01), length(g[0]))
        # the closed square is cut twice into two pieces, not three
        ring_pieces = [x for f, x in zip(after, g) if f['properties']['name'] == 'roads2']
        check(len(ring_pieces) == 2, len(ring_pieces))
        check(close(length(ring_pieces[0]), length(ring_pieces[1]), 0.01), [length(x) for x in ring_pieces])


def test_split_in_the_tool_layer_deletes_the_tool():
    roads = lines_layer('roads', [[(50, -10), (50, 10)], [(0, 0), (100, 0)], [(0, 50), (100, 50)]])
    with Session(roads) as s:
        s.activate('roads')
        s.select('roads', [0])
        r = s.plugin('split-with-line', {'keepTools': False})
        check(r['split'] == 1 and r['created'] == 1, r)
        after = s.features('roads')
        check([f['properties']['name'] for f in after] == ['roads1', 'roads2', 'roads1'], after)
        check(s.selected_ids('roads') == [after[0]['id'], after[2]['id']], 'selection')


def test_split_refuses_a_line_that_does_not_cross():
    cut = lines_layer('cut', [[(-10, 50), (50, 50)]])
    with Session(grid_layer('parcels', nx=2), cut) as s:
        before = s.features('parcels')
        s.select('parcels', [0, 1])
        s.activate('cut')
        s.select('cut', [0], add=True)
        e = s.error_of('split-with-line')
        check('does not cross' in e and 'Select the splitting line' in e, e)
        # a polygon as the tool
        s.activate('parcels')
        s.select('parcels', [0])
        e = s.error_of('split-with-line')
        check('holds polygons' in e, e)
        check(same_geoms(s.features('parcels'), before), 'a refused split changed the data')


def test_split_big_polygon():
    lon, lat = PL
    n = 3000
    ring = [offset_m(lon, lat, 500 * math.cos(i * math.tau / n), 500 * math.sin(i * math.tau / n)) for i in range(n)]
    ring.append(ring[0])
    cut = lines_layer('cut', [[(-600, 3), (0, -2), (600, 1)], [(-600, -300), (600, 310)]])
    with Session(write_layer('big', [feature(polygon(ring), name='c')]), cut) as s:
        s.select('big', [0])
        s.activate('cut')
        s.select('cut', [0, 1], add=True)
        t = time.time()
        r = s.plugin('split-with-line')
        dt = time.time() - t
        check(r['split'] == 1 and r['created'] == 3, r)
        after = s.features('big')
        kept = sum(sum(1 for p in ring if has_vertex(f['geometry'], p, 1e-12)) for f in after)
        check(kept >= n, 'original vertices kept: %d of %d' % (kept, n))
        print('      split of %d vertices: %.2fs' % (n, dt))


def real_layer(s):
    layers = [l for l in s.layers().values() if l['dim'] == 'polygon']
    if not layers:
        raise Skip('no polygon layer')
    return layers[0]['name']


def real_data():
    path = os.environ.get('AE_REAL_DATA')
    if not path:
        raise Skip('AE_REAL_DATA is not set')
    return path


def pick_parcel(feats):
    """A middle-sized parcel and a horizontal line through the middle of its bounding box."""
    sizes = sorted(range(len(feats)), key=lambda i: len(coords_of(feats[i]['geometry'])))
    i = sizes[len(sizes) * 3 // 4]
    pts = coords_of(feats[i]['geometry'])
    xs, ys = [p[0] for p in pts], [p[1] for p in pts]
    y = (min(ys) + max(ys)) / 2
    return i, [[min(xs) - 1e-4, y], [max(xs) + 1e-4, y]]


def test_split_real_parcel():
    path = real_data()
    with Session(path) as s:
        name = real_layer(s)
        feats = s.features(name)
        m = Metric(s)
        i, line = pick_parcel(feats)
    cut = write_layer('realcut', [feature({'type': 'LineString', 'coordinates': line}, name='cut')])
    with Session(path, cut) as s:
        m = Metric(s)
        before = s.features(name)
        target = before[i]
        a0 = area(m.geom(target['geometry']))
        s.select(name, [i])
        s.activate('realcut')
        s.select('realcut', [0], add=True)
        t = time.time()
        r = s.plugin('split-with-line')
        dt = time.time() - t
        check(r['split'] == 1 and r['created'] >= 1, r)
        after = s.features(name)
        check(len(after) == len(before) + r['created'], len(after))
        pieces = [after[i]] + after[len(before):]
        a1 = sum(area(m.geom(f['geometry'])) for f in pieces)
        check(abs(a1 - a0) <= 1e-6 * a0, (a0, a1))
        check(all(f['properties'] == target['properties'] for f in pieces), 'attributes')
        # neighbours change only by the shared vertices topological editing adds, and every
        # original vertex of the parcel is kept bit-exact
        from test_topo import neighbours_only_gained
        neighbours_only_gained(before, after[:len(before)], {i}, r, [f['geometry'] for f in pieces])
        for p in coords_of(target['geometry']):
            check(any(has_vertex(f['geometry'], p, 1e-12) for f in pieces), 'lost vertex %s' % p)
        print('      real split: %d vertices, %d pieces, %.2fs' % (len(coords_of(target['geometry'])), len(pieces), dt))


# ---------- reshape ----------

def test_reshape_polygon_out_and_in():
    tools = lines_layer('tool', [[(30, 90), (30, 120), (70, 120), (70, 90)], [(-10, 80), (110, 80)],
                                 [(0, 50), (-20, 50), (-20, 80), (0, 80)]])
    with Session(grid_layer('parcels', nx=2), tools) as s:
        m = Metric(s)
        before = s.features('parcels')
        a0 = area(m.geom(before[0]['geometry']))
        s.select('parcels', [0])
        s.activate('tool')
        s.select('tool', [0], add=True)
        r = s.plugin('reshape')
        check(r['reshaped'] == 1, r)
        bumped = s.features('parcels')
        s.undo()
        check(same_geoms(s.features('parcels'), before), 'undo did not restore')
        s.select('parcels', [0])
        s.activate('tool')
        s.select('tool', [1], add=True)
        s.plugin('reshape')
        after = s.features('parcels')
        a1 = area(m.geom(bumped[0]['geometry']))
        check(close(a1 / a0, 1.08, 2e-3), a1 / a0)
        # a line snapped onto the outline at both ends (on the parcel cut to 80 % above)
        s.select('parcels', [0])
        s.activate('tool')
        s.select('tool', [2], add=True)
        s.plugin('reshape')
        snapped = s.features('parcels')
        s.undo()
        a3 = area(m.geom(snapped[0]['geometry']))
        check(close(a3 / a0, 0.86, 2e-3), a3 / a0)
        a2 = area(m.geom(after[0]['geometry']))
        check(close(a2 / a0, 0.8, 2e-3), a2 / a0)
        check(same_geoms(after[1:], before[1:], 0.0), 'the neighbour changed')
        # the corners on the shared edge below the cut are kept exactly
        for p in coords_of(before[0]['geometry'])[:2]:
            check(has_vertex(after[0]['geometry'], p, 1e-12), p)


def test_reshape_holes_and_multipart():
    lon, lat = PL
    mp = multipolygon(polygon(rect(lon, lat, 40, 40, 300, 0)), polygon(rect(lon, lat, 40, 40, 400, 0)))
    holed = holed_layer('holed')
    multi = write_layer('multi', [feature(mp, name='mp')])
    tools = lines_layer('tool', [[(40, 20), (40, 50), (60, 50), (60, 20)],     # shrinks the hole
                                 [(40, 40), (40, 10), (60, 10), (60, 40)],     # enlarges the hole
                                 [(410, -10), (410, 50)],                     # cuts the second part
                                 [(-10, 50), (110, 50)]])                     # crosses outline and hole
    with Session(holed, multi, tools) as s:
        m = Metric(s)
        a0 = area(m.geom(s.features('holed')[0]['geometry']))
        outer = a0 / (1 - 0.16)

        def run(layer, tool):
            s.select(layer, [0])
            s.activate('tool')
            s.select('tool', [tool], add=True)
            return s.plugin('reshape')

        run('holed', 0)
        shrunk = s.features('holed')[0]['geometry']
        s.undo()
        run('holed', 1)
        grown = s.features('holed')[0]['geometry']
        s.undo()
        g = m.geom(shrunk)
        check(close(area(g) / outer, 1 - 0.12, 1e-3), area(g) / outer)
        g = m.geom(grown)
        check(close(area(g) / outer, 1 - 0.20, 1e-3), area(g) / outer)
        check(len(g['coordinates']) == 2, 'the hole is still a hole')
        s.select('holed', [0])
        s.activate('tool')
        s.select('tool', [3], add=True)
        e = s.error_of('reshape')
        check('more than one ring' in e, e)
        mp0 = m.geom(s.features('multi')[0]['geometry'])
        run('multi', 2)
        mp1 = m.geom(s.features('multi')[0]['geometry'])
        check(mp1['type'] == 'MultiPolygon' and len(mp1['coordinates']) == 2, mp1['type'])
        check(close(area(mp1) / area(mp0), 2800 / 3200, 1e-3), area(mp1) / area(mp0))


def test_reshape_lines_and_extend():
    roads = lines_layer('roads', [[(0, 0), (300, 0)]])
    tools = lines_layer('tool', [[(100, -10), (100, 20), (200, 20), (200, -10)], [(300, 0), (350, 0), (350, 50)],
                                 [(0, -50), (0, 0)]])
    with Session(roads, tools) as s:
        m = Metric(s)
        l0 = length(m.geom(s.features('roads')[0]['geometry']))

        def run(tool):
            s.select('roads', [0])
            s.activate('tool')
            s.select('tool', [tool], add=True)
            s.plugin('reshape')
            return m.geom(s.features('roads')[0]['geometry'])

        g = run(0)
        check(s.selected_ids('roads') == [1], 'selection')
        check(close(length(g) / l0, 340 / 300, 2e-3), length(g) / l0)
        check(len(g['coordinates']) == 6, g)
        g = run(1)
        check(close(length(g) / l0, 440 / 300, 2e-3), length(g) / l0)
        g = run(2)
        check(close(length(g) / l0, 490 / 300, 2e-3), length(g) / l0)
        check(math.dist(g['coordinates'][0], m.pts([linestring(*PL, [(0, -50)])['coordinates'][0]])[0]) < 1e-6,
              'extended at the start')


def test_reshape_refuses_wrong_selection():
    tools = lines_layer('tool', [[(30, 90), (30, 120), (70, 120), (70, 90)], [(30, 150), (70, 150)]])
    with Session(grid_layer('parcels', nx=1), tools) as s:
        before = s.features('parcels')
        s.select('parcels', [0])
        s.activate('tool')
        s.select('tool', [0, 1], add=True)
        e = s.error_of('reshape')
        check('Select one reshaping line' in e, e)
        s.select('parcels', [0])
        s.activate('tool')
        s.select('tool', [1], add=True)
        e = s.error_of('reshape')
        check('does not cross' in e, e)
        check(same_geoms(s.features('parcels'), before, 0.0), 'a refused reshape changed the data')


def test_reshape_real_parcel():
    path = real_data()
    with Session(path) as s:
        name = real_layer(s)
        feats = s.features(name)
        i, line = pick_parcel(feats)
    cut = write_layer('realtool', [feature({'type': 'LineString', 'coordinates': line}, name='tool')])
    with Session(path, cut) as s:
        m = Metric(s)
        before = s.features(name)
        a0 = area(m.geom(before[i]['geometry']))
        s.select(name, [i])
        s.activate('realtool')
        s.select('realtool', [0], add=True)
        try:
            s.plugin('reshape')
        except Exception as e:  # noqa: BLE001 - a concave parcel crossed more than twice may refuse
            check('back and forth' in str(e) or 'more than one ring' in str(e), e)
            print('      real reshape refused: %s' % str(e)[-80:])
            return
        after = s.features(name)
        a1 = area(m.geom(after[i]['geometry']))
        check(0.5 * a0 <= a1 < a0, (a0, a1))
        check(same_geoms([f for k, f in enumerate(after) if k != i], [f for k, f in enumerate(before) if k != i], 0.0),
              'neighbours changed')


# ---------- offset ----------

def offset_ok(orig, out, d, want_side, tol=1e-3):
    """Every vertex and segment midpoint of `out` lies at d from `orig` on the wanted side, and
    no part crosses itself."""
    for p in parts(out):
        check(not segments_cross(p), 'the offset crosses itself')
        for q in list(p) + [((a[0] + b[0]) / 2, (a[1] + b[1]) / 2) for a, b in zip(p, p[1:])]:
            dq = dist_to_line(q, orig)
            check(d * (1 - 0.003) - tol <= dq <= d + tol, 'distance %.6f, want %.6f' % (dq, d))
            check(on_side(q, orig, want_side), 'wrong side at %s' % (q,))


def test_offset_lines():
    lines = lines_layer('roads', [
        [(0, 0), (100, 0)],                                                    # straight
        [(0, 50), (10, 60), (20, 50), (30, 60), (40, 50), (50, 60)],           # zigzag, d > curvature
        # touches itself, at one of its vertices (a point on an edge would land a fraction of a
        # millimetre beside it after projection, and the distance to that end counts)
        [(0, 100), (50, 100), (100, 100), (100, 150), (50, 150), (50, 100)],
        [(200, 0), (300, 0), (300, 100), (200, 100), (200, 0)],                # closed ring (CCW)
        [(0, 200), (50, 200), (50.001, 200), (50.001, 200.0005), (50.001, 200.0005), (100, 210)],  # tiny segs
    ], kind='road')
    with Session(lines) as s:
        m = Metric(s)
        orig = [m.geom(f['geometry'])['coordinates'] for f in s.features('roads')]
        for left, want in ((False, -1), (True, 1)):
            s.activate('roads')
            s.select('roads', [0, 1, 2, 3, 4])
            r = s.plugin('offset', {'distance': 6, 'left': left})
            check(r['created'] == 5, r)
            feats = s.features('roads')
            s.undo()
            check(len(s.features('roads')) == 5, 'undo')
            new = feats[5:]
            check(all(f['properties']['kind'] == 'road' for f in new), 'attributes')
            g = [m.geom(f['geometry']) for f in new]
            for o, x in zip(orig, g):
                offset_ok(o, x, 6, want)
            check(close(length(g[0]), math.dist(*orig[0]), 1e-6), length(g[0]))
            ring = parts(g[3])
            check(len(ring) == 1 and ring[0][0] == ring[0][-1], 'the ring offset is not one closed ring')
            perimeter = length({'type': 'LineString', 'coordinates': orig[3]})
            want_len = perimeter + 2 * math.pi * 6 if not left else perimeter - 8 * 6
            check(close(length(g[3]), want_len, 0.05), (length(g[3]), want_len))
            # inside the touching loop the left offset is a closed ring of its own
            check(len(parts(g[2])) == (2 if left else 1), len(parts(g[2])))


def test_offset_too_far_and_polygons():
    lines = lines_layer('roads', [[(0, 50), (0, 0), (10, 0), (10, 50)], [(100, 0), (200, 0)]])
    with Session(lines, grid_layer('parcels', nx=1)) as s:
        m = Metric(s)
        s.activate('roads')
        s.select('roads', [0])
        e = s.error_of('offset', {'distance': 20, 'left': True})   # inside a 10 m wide U
        check('nothing is left' in e, e)
        s.select('roads', [0, 1])
        r = s.plugin('offset', {'distance': 20, 'left': True})
        check(r['created'] == 1 and r['skipped'] == 1, r)
        check(s.selected_ids('roads') == [3], 'selection')
        s.activate('roads')
        s.select('roads', [0])
        s.plugin('offset', {'distance': 20, 'left': False})
        u = m.geom(s.features('roads')[-1]['geometry'])
        orig = m.geom(s.features('roads')[0]['geometry'])['coordinates']
        offset_ok(orig, u, 20, -1)
        check(len(parts(u)) == 1, 'outer side of the U is one line')
        # polygons: outwards by default, inwards to the left
        s.activate('parcels')
        s.select('parcels', [0])
        a0 = area(m.geom(s.features('parcels')[0]['geometry']))
        side_m = math.sqrt(a0)
        s.plugin('offset', {'distance': 10})
        s.select('parcels', [0])
        s.plugin('offset', {'distance': 10, 'left': True})
        a = [area(m.geom(f['geometry'])) for f in s.features('parcels')]
        check(close(a[1], a0 + 4 * side_m * 10 + math.pi * 100, 2.0), a)
        check(close(a[2], (side_m - 20) ** 2, 1.0), a)
        # nothing selected
        s.clear_selection()
        e = s.error_of('offset')
        check('Select the lines or polygons to offset' in e, e)


def test_offset_in_place():
    roads = lines_layer('roads', [[(0, 0), (100, 0)], [(0, 50), (100, 50)]], kind='road')
    with Session(roads) as s:
        m = Metric(s)
        before = s.features('roads')
        orig = m.geom(before[0]['geometry'])['coordinates']
        s.activate('roads')
        s.select('roads', [0])
        r = s.plugin('offset', {'distance': 5, 'left': True, 'move': True})
        check(r['moved'] == 1, r)
        after = s.features('roads')
        check(len(after) == 2 and after[0]['properties']['kind'] == 'road', 'replaced, not added')
        offset_ok(orig, m.geom(after[0]['geometry']), 5, 1)
        check(same_geoms([after[1]], [before[1]], 0.0), 'the other line changed')
        s.select('roads', [0])
        s.plugin('offset', {'distance': 5, 'panel': {'move-original': True}})
        back = m.geom(s.features('roads')[0]['geometry'])['coordinates']
        check(all(math.dist(p, q) < 1e-3 for p, q in zip(back, orig)), back)


def test_offset_long_line_and_budget():
    lon, lat = PL
    wig = [(i * 0.5, 30 * math.sin(i * 0.05)) for i in range(3000)]
    data = write_layer('long', [feature(linestring(lon, lat, [(x, y + 100 * k) for x, y in wig]), name='w%d' % k)
                                for k in range(4)])
    with Session(data) as s:
        s.activate('long')
        s.select('long', [0])
        t = time.time()
        r = s.plugin('offset', {'distance': 20})
        dt = time.time() - t
        check(r['created'] == 1, r)
        g = s.features('long')[-1]['geometry']
        check(g['type'] == 'LineString' and not segments_cross(g['coordinates']), g['type'])
        print('      offset of 3000 vertices: %d vertices, %.2fs' % (len(g['coordinates']), dt))
        # four such lines are more than one command's budget: refused before anything is written
        before = s.features('long')
        s.select('long', [0, 1, 2, 3])
        e = s.error_of('offset', {'distance': 20})
        check('offsetting these lines by 20 m is more than one command can do' in e, e)
        check(s.features('long') == before, 'a refused offset changed the data')
        s.select('long', [1])
        r = s.plugin('offset', {'distance': 20, 'left': True})
        check(r['created'] == 1, r)


def test_offset_budget_follows_the_shape():
    # A line folding back within the distance of itself costs far more per vertex than a
    # smooth one, and a dense polygon buffered inwards more than linearly: both are refused
    # up front, never run out of the command's budget.
    lon, lat = PL
    zig = [(i * 2.0, 3.0 * (i % 2)) for i in range(800)]
    ring = [(100 * math.cos(t) * (1 + 0.02 * math.sin(7 * t)), 100 * math.sin(t) * (1 + 0.02 * math.sin(7 * t)))
            for t in (i * math.tau / 1300 for i in range(1300))]
    small = [(x * 0.5, y * 0.5) for x, y in ring[::2]]
    lines = write_layer('zig', [feature(linestring(lon, lat, zig), name='zig')])
    polys = write_layer('dense', [feature(polygon([offset_m(lon, lat, x, y) for x, y in ring + ring[:1]]), name='dense'),
                                  feature(polygon([offset_m(lon, lat, x + 300, y) for x, y in small + small[:1]]),
                                          name='half')])
    with Session(lines, polys) as s:
        for layer, d, what in [('zig', 20, 'lines'), ('dense', 5, 'polygons')]:
            before = s.features(layer)
            s.activate(layer)
            s.select(layer, [0])
            e = s.error_of('offset', {'distance': d, 'left': True})
            check('offsetting these %s by %s m is more than one command can do' % (what, d) in e, e)
            check(s.features(layer) == before, 'a refused offset changed the data')
        # smaller inputs of the same kind fit
        s.activate('zig')
        s.select('zig', [0])
        check(s.plugin('offset', {'distance': 1})['created'] == 1)
        s.activate('dense')
        s.select('dense', [1])
        check(s.plugin('offset', {'distance': 5, 'left': True})['created'] == 1)


# ---------- trim / extend ----------

def test_trim_extend():
    bound = lines_layer('bound', [[(100, -50), (100, 50)], [(150, -50), (150, 50)]])
    roads = lines_layer('roads', [[(0, 0), (90, 0)], [(0, 10), (120, 10)], [(0, 20), (50, 20)],
                                  [(90, 25), (80, 35), (0, 35)], [(0, 40), (0, 45)]])
    with Session(bound, roads) as s:
        m = Metric(s)
        before = s.features('roads')
        g0 = [m.geom(f['geometry']) for f in before]
        s.select('roads', [0, 1, 2, 3, 4])
        s.activate('bound')
        s.select('bound', [0], add=True)
        # the Distance does not limit the extension
        r = s.plugin('trim-extend', {'distance': 0.01})
        check(r['extended'] == 3 and r['trimmed'] == 1 and r['skipped'] == 1, r)
        after = s.features('roads')
        s.undo()
        check(same_geoms(s.features('roads'), before), 'undo')
        g = [m.geom(f['geometry']) for f in after]
        check(close(length(g[0]) / length(g0[0]), 100 / 90, 1e-6), length(g[0]))
        check(close(length(g[1]) / length(g0[1]), 100 / 120, 1e-6), length(g[1]))
        check(close(length(g[2]) / length(g0[2]), 100 / 50, 1e-6), length(g[2]))
        check(same_geoms([after[4]], [before[4]], 0.0), 'the parallel line changed')
        # the bent line is extended at its start along its first segment
        r2 = math.sqrt(2)
        check(close(length(g[3]) / length(g0[3]), (80 + 20 * r2) / (80 + 10 * r2), 1e-3), length(g[3]))
        # the far end of the first line stays exactly where it was
        check(has_vertex(after[0]['geometry'], before[0]['geometry']['coordinates'][0], 1e-12), 'moved the far end')
        # parallel to the boundary
        s.select('roads', [4])
        s.activate('bound')
        s.select('bound', [0], add=True)
        e = s.error_of('trim-extend')
        check('neither end points towards the boundary' in e, e)
        # two boundaries selected
        s.select('roads', [0])
        s.activate('bound')
        s.select('bound', [0, 1], add=True)
        e = s.error_of('trim-extend')
        check('Select one boundary line' in e, e)
        check(same_geoms(s.features('roads'), before), 'a refused trim/extend changed the data')


def test_trim_extend_in_one_layer():
    # boundary x=100; 1 ends 5 m short, 2 crosses, 3 ends 40 m short, 4 is parallel
    roads = lines_layer('roads', [[(100, -50), (100, 50)], [(0, 0), (95, 0)], [(0, 10), (110, 10)],
                                  [(0, 30), (60, 30)], [(0, 60), (0, 70)]])
    with Session(roads) as s:
        m = Metric(s)
        before = s.features('roads')
        g0 = [m.geom(f['geometry']) for f in before]
        s.activate('roads')
        # Distance is a search radius: the crossing line counts at any radius, 3 is beyond it
        s.select('roads', [0])
        r = s.plugin('trim-extend', {'distance': 10})
        check(r['extended'] == 1 and r['trimmed'] == 1 and r['skipped'] == 0, r)
        after = s.features('roads')
        check(same_geoms([after[3]], [before[3]], 0.0), 'a line beyond the search radius changed')
        s.undo()
        check(same_geoms(s.features('roads'), before), 'undo')
        g = [m.geom(f['geometry']) for f in after]
        check(close(length(g[1]), length(g[2]), 1e-3), (length(g[1]), length(g[2])))
        s.select('roads', [0])
        r = s.plugin('trim-extend', {'distance': 0.01})
        check(r['extended'] == 0 and r['trimmed'] == 1, r)
        s.undo()
        # a larger radius takes the far line in too; the parallel line is never a target
        s.select('roads', [0])
        r = s.plugin('trim-extend', {'distance': 50})
        check(r['extended'] == 2 and r['trimmed'] == 1 and r['skipped'] == 0, r)
        g = [m.geom(f['geometry']) for f in s.features('roads')]
        check(close(length(g[3]), length(g[1]), 1e-2), (length(g[3]), length(g[1])))
        check(close(length(g[3]) / length(g0[3]), 100 / 60, 1e-6), length(g[3]))
        check(s.selected_ids('roads') == [2, 3, 4], 'selection')


def test_trim_extend_search_radius():
    bound = lines_layer('bound', [[(100, -50), (100, 50)]])
    roads = lines_layer('roads', [[(0, 0), (40, 0)], [(0, 20), (60, 20)]])
    with Session(bound, roads) as s:
        m = Metric(s)
        before_b, before_r = s.features('bound'), s.features('roads')
        g0 = [m.geom(f['geometry']) for f in before_r]
        # no target line in the active layer within the radius: refused, nothing changes
        s.activate('bound')
        s.select('bound', [0])
        e = s.error_of('trim-extend', {'distance': 5})
        check('No line of the active layer crosses the boundary or ends within 5 m of it' in e, e)
        check('raise Distance' in e, e)
        check(same_geoms(s.features('bound'), before_b) and same_geoms(s.features('roads'), before_r),
              'a refused trim/extend changed the data')
        # explicitly selected targets are extended without a limit, whatever the radius
        s.select('roads', [0, 1])
        s.activate('bound')
        s.select('bound', [0], add=True)
        r = s.plugin('trim-extend', {'distance': 0.01})
        check(r['extended'] == 2 and r['trimmed'] == 0, r)
        g = [m.geom(f['geometry']) for f in s.features('roads')]
        check(close(length(g[0]) / length(g0[0]), 100 / 40, 1e-6), length(g[0]))
        check(close(length(g[1]) / length(g0[1]), 100 / 60, 1e-6), length(g[1]))
