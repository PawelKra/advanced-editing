"""Shared boundaries: new vertices exactly on the original edges, no gaps between the pieces
of neighbouring features, and topological editing (the neighbours that are not edited get the
same vertices).

Vertices are checked in the plugin's own frame (`diag-proj`), the union with shapely in the
test's (see test_edge_lines.py; skipped without it)."""

import json
import math
import os
import subprocess
from decimal import Decimal

from aelib import Session, Skip, check, feature, write_layer
from test_division import frame_of, to_m
from test_edge_lines import O, flat, ll, shapely_python
from test_limits import blob, tm_exact

# ---------- helpers ----------

UNION_SCRIPT = r'''
import json, sys
from shapely.geometry import shape, Polygon
from shapely.ops import unary_union
j = json.load(sys.stdin)
def holes(gs):
    u = unary_union([shape(g).buffer(0) for g in gs])
    return u, [Polygon(r) for p in getattr(u, 'geoms', [u]) for r in p.interiors]
ub, hb = holes(j['before'])
ua, ha = holes(j['after'])
new = [h for h in ha if not any(h.representative_point().within(o) for o in hb)]
print(json.dumps({'area_before': ub.area, 'area_after': ua.area,
                  'new_holes': [[h.area, 2 * h.area / max(h.length, 1e-300)] for h in new]}))
'''


def union_report(before, after, m):
    """New holes in the union of `after` against `before` (lon/lat polygons), in metres."""
    job = {'before': [gmap_m(g, m) for g in before], 'after': [gmap_m(g, m) for g in after]}
    r = subprocess.run([shapely_python(), '-I', '-c', UNION_SCRIPT], input=json.dumps(job),
                       capture_output=True, text=True, timeout=600)
    if r.returncode != 0:
        raise RuntimeError('shapely helper failed: ' + r.stderr[-2000:])
    return json.loads(r.stdout)


def gmap_m(g, m):
    def rec(c):
        if c and isinstance(c[0], (int, float)):
            return list(m(c))
        return [rec(x) for x in c]
    return {'type': g['type'], 'coordinates': rec(g['coordinates'])}


def rings(g):
    """The rings or lines of a geometry, as lists of positions."""
    c = g['coordinates']
    if g['type'] == 'Polygon' or g['type'] == 'MultiLineString':
        return list(c)
    if g['type'] == 'MultiPolygon':
        return [r for p in c for r in p]
    return [c]


def same_pos(a, b, eps=1e-13):
    return abs(a[0] - b[0]) <= eps and abs(a[1] - b[1]) <= eps


def gained(old, new):
    """The positions `new` adds to `old` when it keeps all of old's positions in order (within
    one ulp or so), else None."""
    ro, rn = rings(old), rings(new)
    if old['type'] != new['type'] or len(ro) != len(rn):
        return None
    out = []
    for a, b in zip(ro, rn):
        k = 0
        for c in b:
            if k < len(a) and same_pos(a[k], c):
                k += 1
            else:
                out.append(c)
        if k != len(a):
            return None
    return out


def line_dist(p, a, b):
    """Distance from p to the line through a and b (metres)."""
    dx, dy = b[0] - a[0], b[1] - a[1]
    return abs((p[0] - a[0]) * dy - (p[1] - a[1]) * dx) / math.hypot(dx, dy)


def seg_dist(p, a, b):
    dx, dy = b[0] - a[0], b[1] - a[1]
    t = max(0.0, min(1.0, ((p[0] - a[0]) * dx + (p[1] - a[1]) * dy) / (dx * dx + dy * dy)))
    return math.hypot(p[0] - a[0] - t * dx, p[1] - a[1] - t * dy)


def segments(g):
    for r in rings(g):
        for a, b in zip(r, r[1:]):
            yield a, b


def neighbours_only_gained(before, after, edited, result, pieces):
    """Every feature but the `edited` rows is unchanged except for the vertices topological
    editing added, which the answer counts and which are vertices of the edited `pieces`."""
    piece_pos = [c for g in pieces for c in flat(g)]
    changed, added = 0, 0
    for i, (b, a) in enumerate(zip(before, after)):
        if i in edited:
            continue
        extra = gained(b['geometry'], a['geometry'])
        check(extra is not None, 'feature %s lost or moved vertices' % b.get('id'))
        if extra:
            changed += 1
            added += len(extra)
            check(all(any(same_pos(c, d) for d in piece_pos) for c in extra),
                  'feature %s got a vertex the pieces do not have' % b.get('id'))
    check(changed == result.get('sharedFeatures', 0) and added == result.get('sharedVertices', 0),
          'neighbours changed %d / %d vertices, the answer says %s' % (changed, added, result))
    return changed, added


def plugin_frame(*feature_lists):
    """lon/lat -> metres with the plugin's own projection, for the positions of the features."""
    pos = sorted({tuple(c) for fs in feature_lists for f in fs for c in flat(f['geometry'])})
    with Session() as s:
        out = s.plugin('diag-proj', {'points': [list(p) for p in pos]})['points']
    table = {p: (o['x'], o['y']) for p, o in zip(pos, out)}
    return lambda c: table[tuple(c)]


def exact_2180(c):
    """lon/lat -> EPSG:2180 in 45-digit arithmetic (test_limits.tm_exact), to ~1e-10 m."""
    return tm_exact('298.257222101', Decimal(19), '0.9993', '-5300000', c[0], c[1])


def on_original_edges(before, after, tol=3e-9, shared=lambda f: True):
    """Every vertex the command added lies on an original edge within `tol` metres, measured in
    EPSG:2180 with an exact projection, and every feature whose boundary passes it has it as a
    vertex (of those `shared` picks). The tolerance is the resolution of the plugin's double
    precision frame: its northings come out of ~5.8e6 m sums (ulp 9.3e-10 m), a latitude ulp
    is 7.9e-10 m; the boolean ops alone used to leave crossings ~1e-7 m off."""
    m = plugin_frame(before, after)
    orig = [c for f in before for c in flat(f['geometry'])]
    edges = [(m(a), m(b)) for f in before for a, b in segments(f['geometry']) if a != b]
    worst, missing = 0.0, []
    for f in after:
        for c in flat(f['geometry']):
            if any(same_pos(c, o, 1e-12) for o in orig):
                continue
            p = m(c)
            near = [(a, b) for a, b in edges if seg_dist(p, a, b) < 1e-6]
            if not near:
                continue  # a vertex inside, on the cut line only
            ends = [(a, b) for f in before for a, b in segments(f['geometry']) if a != b and seg_dist(p, m(a), m(b)) < 1e-6]
            worst = max(worst, min(line_dist(exact_2180(c), exact_2180(a), exact_2180(b)) for a, b in ends))
            for g in filter(shared, after):
                passes = any(seg_dist(p, m(a), m(b)) < 1e-6 for a, b in segments(g['geometry']))
                if passes and not any(same_pos(c, d) for d in flat(g['geometry'])):
                    missing.append((c, g.get('id')))
    check(worst <= tol, 'a new vertex lies %.2e m off its original edge' % worst)
    check(not missing, 'features pass a new vertex without having it: %s' % missing[:3])
    return worst


# ---------- data ----------

def nodes(nx, ny, size=100.0, jitter=15.0):
    """Grid nodes in metres, the inner ones moved pseudo-randomly so that edges are slanted."""
    pts = {}
    for j in range(ny + 1):
        for i in range(nx + 1):
            x, y = i * size, j * size
            if 0 < i < nx:
                x += jitter * math.sin(1.7 * i + 2.3 * j)
            if 0 < j < ny:
                y += jitter * math.cos(1.3 * i + 0.7 * j)
            pts[i, j] = (x, y)
    return pts


def parcels(nx, ny):
    """Adjacent irregular quadrilaterals; shared corners have identical lon/lat."""
    n = nodes(nx, ny)
    lonlat = dict(zip(n, ll(O, list(n.values()))))
    out = []
    for j in range(ny):
        for i in range(nx):
            ring = [lonlat[i, j], lonlat[i + 1, j], lonlat[i + 1, j + 1], lonlat[i, j + 1], lonlat[i, j]]
            out.append(feature({'type': 'Polygon', 'coordinates': [ring]}, name='p%d_%d' % (i, j)))
    return out


def line(*pts):
    return {'type': 'LineString', 'coordinates': ll(O, list(pts))}


def sq(x, y, w=100.0):
    return ll(O, [(x, y), (x + w, y), (x + w, y + w), (x, y + w), (x, y)])


def poly(*rings_):
    return {'type': 'Polygon', 'coordinates': list(rings_)}


def m_of():
    return to_m(*O)


def split_all(feats, tools, params=None, select=None):
    pl = write_layer('pl', feats)
    tl = write_layer('tl', [feature(t, name='t%d' % i) for i, t in enumerate(tools)])
    with Session(pl, tl) as s:
        before = s.features('pl')
        s.select('pl', select if select is not None else range(len(feats)))
        s.activate('tl')
        s.select('tl', range(len(tools)), add=True)
        r = s.plugin('split-with-line', dict({'keepTools': True}, **(params or {})))
        after = s.features('pl')
        s.undo()
        restored = s.features('pl')
    return r, before, after, restored


# ---------- no gaps along shared edges ----------

def test_split_leaves_no_gaps_between_neighbours():
    """Adjacent parcels with slanted shared edges, split by a zigzag and a crossing line: the
    crossings land exactly on the edges, identical in the pieces on both sides, and the union
    of the layer gains no hole."""
    feats = parcels(4, 3)
    zig = line((-20, 40), (60, 260), (140, 30), (220, 270), (330, 50), (420, 280))
    cross = line((-10, 290), (415, -12))
    r, before, after, restored = split_all(feats, [zig, cross])
    check(r['split'] == 12 and r['created'] > 12, r)
    m = m_of()
    worst = on_original_edges(before, after)
    u = union_report([f['geometry'] for f in before], [f['geometry'] for f in after], m)
    check(not u['new_holes'], 'new holes (area, width): %s' % u['new_holes'][:5])
    check(abs(u['area_after'] - u['area_before']) < 1e-6, u)
    check(restored == before, 'undo did not restore')
    print('      %d pieces, new vertices within %.1e m of their edges' % (len(after), worst))


def test_real_split_leaves_no_new_hairline_holes():
    """The real-parcel case: a zigzag and a crossing line over the middle of the layer, every
    parcel selected. Rewritten features come back through the layer's own CRS, which moves
    their vertices by ~1e-9 m, so holes that thin are the host's; the cut used to leave slivers
    ~1e-7 m wide along whole shared edges."""
    path = os.environ.get('AE_REAL_DATA')
    if not path:
        raise Skip('AE_REAL_DATA is not set')
    with Session(path) as s:
        name = next((n for n, l in s.layers().items() if l['dim'] == 'polygon'), None)
        if name is None:
            raise Skip('no polygon layer in %s' % path)
        before = s.features(name)
    if len(before) > 2000:
        raise Skip('the real layer is too large for one split')
    xs = [c for f in before for c in flat(f['geometry'])]
    lon0, lat0 = sum(c[0] for c in xs) / len(xs), sum(c[1] for c in xs) / len(xs)
    m = to_m(lon0, lat0)
    pts = [m(c) for c in xs]
    x0, x1 = min(p[0] for p in pts), max(p[0] for p in pts)
    y0, y1 = min(p[1] for p in pts), max(p[1] for p in pts)
    cx, cy, w, h = (x0 + x1) / 2, (y0 + y1) / 2, 0.3 * (x1 - x0), 0.3 * (y1 - y0)
    tm = frame_of(lon0)
    ox, oy = tm.fwd(lon0, lat0)

    def back(p):
        return list(tm.inv(ox + p[0], oy + p[1]))
    zig = [(cx - w + i * w / 4, cy + (h / 3 if i % 2 else -h / 3)) for i in range(9)]
    cross = [(cx - 0.2 * w, cy - h), (cx + 0.3 * w, cy + h)]
    tl = write_layer('reallines', [feature({'type': 'LineString', 'coordinates': [back(p) for p in l]}, name=n)
                                   for n, l in (('zig', zig), ('cross', cross))])
    with Session(path, tl) as s:
        s.select(name, range(len(before)))
        s.activate('reallines')
        s.select('reallines', [0, 1], add=True)
        r = s.plugin('split-with-line', {'keepTools': True})
        after = s.features(name)
    check(r['split'] > 10, r)
    u = union_report([f['geometry'] for f in before], [f['geometry'] for f in after], m)
    wide = [h for h in u['new_holes'] if h[1] > 1e-8]
    check(not wide, 'new holes wider than 1e-8 m (area, width): %s' % wide[:5])
    print('      %s; %d new holes, all thinner than 1e-8 m' % (r['message'], len(u['new_holes'])))


# ---------- topological editing ----------

def row_layer():
    """A, B, C side by side; D a two-part polygon whose second part sits on top of B."""
    return [feature(poly(sq(0, 0)), name='A'), feature(poly(sq(100, 0)), name='B'),
            feature(poly(sq(200, 0)), name='C'),
            feature({'type': 'MultiPolygon', 'coordinates': [[sq(0, 300)], [sq(100, 100)]]}, name='D')]


def test_split_adds_the_shared_vertices_to_the_neighbours():
    tools = [line((-10, 50), (210, 50)), line((150, -10), (150, 110))]
    r, before, after, restored = split_all(row_layer(), tools, select=[0, 1])
    check(r['split'] == 2 and r['sharedVertices'] == 2 and r['sharedFeatures'] == 2, r)
    check('added 2 shared vertices to 2 neighbouring feature(s)' in r['message'], r['message'])
    pieces = [f['geometry'] for f in after if f['properties']['name'] in ('A', 'B')]
    neighbours_only_gained(before, after, {0, 1}, r, pieces)
    # C gets the crossing on its left edge, D's second part the one on its bottom edge
    check(len(flat(after[2]['geometry'])) == 6, after[2]['geometry'])
    check(len(after[3]['geometry']['coordinates'][1][0]) == 6, after[3]['geometry'])
    on_original_edges(before, after)
    check(restored == before, 'undo did not restore in one step')


def test_split_without_topological_editing_leaves_the_neighbours():
    tools = [line((-10, 50), (210, 50)), line((150, -10), (150, 110))]
    r, before, after, _ = split_all(row_layer(), tools, {'topological': False}, select=[0, 1])
    check(r['sharedVertices'] == 0 and 'shared' not in r['message'], r)
    check([f['geometry'] for f in after[2:4]] == [f['geometry'] for f in before[2:4]], 'a neighbour changed')
    # the pieces' crossings still lie exactly on the neighbours' edges
    on_original_edges(before, after, shared=lambda f: f['properties']['name'] in ('A', 'B'))


def test_cut_adds_the_shared_vertices_to_the_neighbour():
    tg = write_layer('tg', row_layer()[:3])
    cutter = write_layer('cutter', [feature(poly(ll(O, [(50, 20), (150, 20), (150, 80), (50, 80), (50, 20)])))])
    with Session(tg, cutter) as s:
        before = s.features('tg')
        s.select('tg', [0])
        s.activate('cutter')
        s.select('cutter', [0], add=True)
        r = s.plugin('cut-with-polygon', {'keepTools': True})
        after = s.features('tg')
    check(r['changed'] == 1 and r['sharedVertices'] == 2 and r['sharedFeatures'] == 1, r)
    neighbours_only_gained(before, after, {0}, r, [after[0]['geometry']])
    on_original_edges(before, after)


def test_divide_adds_the_shared_vertices_to_the_neighbours():
    pl = write_layer('pl', parcels(3, 1))
    with Session(pl) as s:
        before = s.features('pl')
        s.activate('pl')
        s.select('pl', [1])
        r = s.plugin('divide-equal', {'parts': 2, 'angle': 90})
        after = s.features('pl')
    check(r['divided'] == 1 and r['sharedVertices'] == 2 and r['sharedFeatures'] == 2, r)
    pieces = [f['geometry'] for f in after if f['properties']['name'] == 'p1_0']
    neighbours_only_gained(before, after, {1}, r, pieces)
    m = m_of()
    on_original_edges(before, after)
    u = union_report([f['geometry'] for f in before], [f['geometry'] for f in after], m)
    check(not u['new_holes'], u)


def test_shared_note_goes_before_the_full_stop():
    # the division messages are sentences; the topology note joins them before the full stop
    pl = write_layer('pl', parcels(3, 1))
    with Session(pl) as s:
        s.activate('pl')
        s.select('pl', [1])
        eq = s.plugin('divide-equal', {'parts': 2, 'angle': 90})['message']
        s.undo()
        s.select('pl', [1])
        pct = s.plugin('divide-percent', {'percent': 30, 'angle': 90})['message']
    for msg in (eq, pct):
        check('.;' not in msg and '(ellipsoidal); added 2 shared vertices to 2 neighbouring feature(s).' in msg
              and msg.endswith('.') and not msg.endswith('..'), msg)
    r, _, _, _ = split_all(row_layer(), [line((-10, 50), (210, 50))], select=[0, 1])
    check('.;' not in r['message'] and not r['message'].endswith('.'), r['message'])


def test_split_line_adds_the_vertex_to_a_line_along_it():
    l1 = line((0, 0), (100, 0))
    l2 = line((0, 0), (100, 0), (100, 50))
    other = line((0, 20), (100, 20))
    r, before, after, _ = split_all([feature(g, name=n) for n, g in (('l1', l1), ('l2', l2), ('o', other))],
                                    [line((50, -10), (50, 10))], select=[0])
    check(r['split'] == 1 and r['sharedVertices'] == 1 and r['sharedFeatures'] == 1, r)
    neighbours_only_gained(before, after, {0}, r, [f['geometry'] for f in after if f['properties']['name'] == 'l1'])
    check(len(after[1]['geometry']['coordinates']) == 4, after[1]['geometry'])


def test_topology_skips_a_layer_too_large_to_read():
    # two small parcels next to 35 blobs of 1001 vertices: ~1.4 MB, more than a command reads
    feats = [feature(poly(sq(0, -500)), name='A'), feature(poly(sq(100, -500)), name='B')]
    feats += [feature(poly(blob(300 * i, 0, 100, 1000)), name='blob') for i in range(35)]
    r, before, after, _ = split_all(feats, [line((-10, -450), (210, -450))], select=[0])
    check(r['split'] == 1 and r['sharedVertices'] == 0, r)
    check("shared vertices not added in 'pl'" in r['message'], r['message'])
    check(after[1]['geometry'] == before[1]['geometry'], 'the neighbour changed')


def test_split_shares_a_crossing_of_two_edited_edges_with_both_neighbours():
    """A and B overlap; A's right edge (shared with NA) and B's slanted edge (shared with NB)
    cross where the split line has a vertex. The cut puts that vertex on both edges with the
    same coordinates: each neighbour must get it, not only the one whose edge was kept."""
    a = feature(poly(sq(0, 0)), name='A')
    b = feature(poly(ll(O, [(50, -50), (150, -50), (150, 150), (50, -50)])), name='B')
    na = feature(poly(sq(100, 0)), name='NA')
    nb = feature(poly(ll(O, [(50, -50), (150, 150), (50, 150), (50, -50)])), name='NB')
    r, before, after, restored = split_all([a, b, na, nb], [line((-10, 40), (100, 50), (210, 45))], select=[0, 1])
    check(r['split'] == 2 and r['sharedVertices'] == 2 and r['sharedFeatures'] == 2, r)
    pieces = [f['geometry'] for f in after if f['properties']['name'] in ('A', 'B')]
    neighbours_only_gained(before, after, {0, 1}, r, pieces)
    on_original_edges(before, after)
    u = union_report([f['geometry'] for f in before], [f['geometry'] for f in after], m_of())
    check(not u['new_holes'], 'new holes (area, width): %s' % u['new_holes'][:5])
    check(restored == before, 'undo did not restore')
