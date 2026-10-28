"""Edge cases for split with line, reshape, offset and trim/extend.

The expected results come from geometry, not from the plugin: polygon splits are compared with
the faces that shapely's polygonize finds for the same target and lines (in the plugin's metric
frame), and every output is checked for validity, overlaps and area conservation. Shapely runs
in a separate Python (`AE_SHAPELY_PYTHON`, default: the running Python if it has shapely) over a
pipe; the tests are skipped when it is not available.

The helpers at the top are shared by the other test_edge_*.py files."""

import json
import math
import os
import subprocess
import sys

from aelib import (ABROAD, PL, CommandError, Session, Skip, check, feature, offset_m, write_layer)
from test_division import frame_of, to_m

# ---------- shapely over a pipe ----------

_SHAPELY = {}

SHAPELY_SCRIPT = r'''
import json, sys
from shapely.geometry import shape, mapping
from shapely.ops import unary_union, polygonize
from shapely.validation import explain_validity

def faces(poly, lines):
    if not lines:
        return [poly.area]
    net = unary_union([poly.boundary] + lines)
    out = []
    for f in polygonize(net):
        if f.area > 1e-6 and poly.contains(f.representative_point()):
            out.append(f.area)
    return sorted(out)

def job(j):
    gs = [shape(g) for g in j.get('geoms', [])]
    r = {'valid': [g.is_valid for g in gs], 'why': [explain_validity(g) for g in gs],
         'area': [g.area for g in gs], 'length': [g.length for g in gs],
         'empty': [g.is_empty for g in gs]}
    polys = [g for g in gs if g.geom_type in ('Polygon', 'MultiPolygon')]
    ov = []
    for i in range(len(gs)):
        for k in range(i + 1, len(gs)):
            if gs[i].geom_type in ('Polygon', 'MultiPolygon') and gs[k].geom_type in ('Polygon', 'MultiPolygon'):
                try:
                    a = gs[i].buffer(0).intersection(gs[k].buffer(0)).area
                except Exception as e:
                    a = -1
                if a != 0:
                    ov.append([i, k, a])
    r['overlaps'] = ov
    if polys:
        try:
            u = unary_union([p.buffer(0) for p in polys])
            r['union'] = u.area
        except Exception:
            r['union'] = -1
    if j.get('orig') is not None:
        o = shape(j['orig'])
        r['orig_area'] = o.area
        r['orig_length'] = o.length
        if polys:
            try:
                u = unary_union([p.buffer(0) for p in polys])
                r['symdiff'] = u.symmetric_difference(o.buffer(0)).area
            except Exception:
                r['symdiff'] = -1
        lines = [shape(l) for l in j.get('lines', [])]
        if o.geom_type in ('Polygon', 'MultiPolygon'):
            parts = [o] if o.geom_type == 'Polygon' else list(o.geoms)
            r['faces'] = [faces(p, lines) for p in parts]
    if j.get('dist_to') is not None:
        t = shape(j['dist_to'])
        r['dist'] = [[t.distance(shape({'type': 'Point', 'coordinates': c})) for c in pts] for pts in j['pts']]
    if j.get('hausdorff') is not None:
        r['hausdorff'] = [g.hausdorff_distance(shape(j['hausdorff'])) for g in gs]
    return r

print(json.dumps([job(j) for j in json.load(sys.stdin)]))
'''


def shapely_python():
    if 'py' not in _SHAPELY:
        py = os.environ.get('AE_SHAPELY_PYTHON')
        if not py:
            try:
                import shapely  # noqa: F401
                py = sys.executable
            except ImportError:
                py = None
        _SHAPELY['py'] = py
    if not _SHAPELY['py']:
        raise Skip('shapely is not available (set AE_SHAPELY_PYTHON)')
    return _SHAPELY['py']


def shapely_jobs(jobs):
    r = subprocess.run([shapely_python(), '-I', '-c', SHAPELY_SCRIPT], input=json.dumps(jobs),
                       capture_output=True, text=True, timeout=300)
    if r.returncode != 0:
        raise RuntimeError('shapely helper failed: ' + r.stderr[-2000:])
    return json.loads(r.stdout)


def shapely_job(**job):
    return shapely_jobs([job])[0]


# ---------- coordinates ----------

def ll(origin, pts):
    """Metres east/north of `origin` in the plugin's frame (EPSG:2180, UTM abroad) -> lon/lat,
    so that straight lines and shared edges built here are straight in that frame too."""
    tm = frame_of(origin[0])
    x0, y0 = tm.fwd(*origin)
    return [list(tm.inv(x0 + x, y0 + y)) for x, y in pts]


def closed(r):
    r = list(r)
    return r if r[0] == r[-1] else r + [r[0]]


def poly(origin, *rings):
    return {'type': 'Polygon', 'coordinates': [ll(origin, closed(r)) for r in rings]}


def mpoly(origin, *polys):
    return {'type': 'MultiPolygon', 'coordinates': [[ll(origin, closed(r)) for r in rings] for rings in polys]}


def line(origin, pts):
    return {'type': 'LineString', 'coordinates': ll(origin, pts)}


def sq(x0, y0, w, h=None):
    h = w if h is None else h
    return [(x0, y0), (x0 + w, y0), (x0 + w, y0 + h), (x0, y0 + h)]


def cw(r):
    return list(reversed(r))


def gmap(g, f):
    """The geometry with every position mapped by f (same nesting)."""
    def rec(c):
        if c and isinstance(c[0], (int, float)):
            return list(f(c))
        return [rec(x) for x in c]
    return {'type': g['type'], 'coordinates': rec(g['coordinates'])}


def first_pos(g):
    c = g['coordinates']
    while not isinstance(c[0], (int, float)):
        c = c[0]
    return c


def metric(origin_geom):
    """lon/lat -> metres of the plugin's frame near `origin_geom` (independent projection)."""
    p = first_pos(origin_geom)
    return to_m(p[0], p[1])


def flat(g):
    out = []

    def rec(c):
        if c and isinstance(c[0], (int, float)):
            out.append(c)
        else:
            for x in c:
                rec(x)
    rec(g['coordinates'])
    return out


def same_geom(a, b, eps=0.0):
    if a['type'] != b['type']:
        return False
    ca, cb = flat(a), flat(b)
    return len(ca) == len(cb) and all(abs(p[0] - q[0]) <= eps and abs(p[1] - q[1]) <= eps for p, q in zip(ca, cb))


def fingerprint(feats):
    return json.dumps([(f.get('id'), f['properties'], f['geometry']) for f in feats], sort_keys=True)


def nparts(g):
    return len(g['coordinates']) if g['type'].startswith('Multi') else 1


class Problems:
    """Collects failed expectations of several cases; `done()` raises them together."""

    def __init__(self):
        self.items = []

    def add(self, label, msg):
        self.items.append('[%s] %s' % (label, msg))

    def expect(self, label, cond, msg):
        if not cond:
            self.add(label, msg)
        return cond

    def done(self):
        check(not self.items, ' || '.join(self.items))


def actionable(msg):
    m = msg.lower()
    return bool(msg.strip()) and ('select' in m or 'choose' in m or 'set ' in m or 'raise' in m or 'lower' in m)


def short(msg, n=160):
    msg = str(msg)
    i = msg.find('"message"')
    return msg[-n:] if len(msg) > n else msg


# ---------- polygon partition checks ----------

def check_partition(P, label, orig, pieces, m, want=None, faces_from=None, rel=1e-6):
    """`pieces` (lon/lat geometries) must partition `orig`: valid, no overlaps, same area and
    union. `want`: the exact piece count; `faces_from`: lines whose polygonize faces the pieces
    must reproduce (for a Polygon target)."""
    om = gmap(orig, m)
    pm = [gmap(g, m) for g in pieces]
    r = shapely_job(geoms=pm, orig=om, lines=[gmap(l, m) for l in (faces_from or [])])
    A = r['orig_area']
    tol = max(1e-4, rel * A)
    ok = True
    if want is not None:
        ok &= P.expect(label, len(pieces) == want, '%d pieces, expected %d (areas %s)'
                       % (len(pieces), want, [round(a, 3) for a in r['area']]))
    bad = [(i, w) for i, (v, w) in enumerate(zip(r['valid'], r['why'])) if not v]
    ok &= P.expect(label, not bad, 'invalid pieces: %s' % bad[:3])
    tot = sum(r['area'])
    ok &= P.expect(label, abs(tot - A) <= tol, 'sum of pieces %.6f m², original %.6f m²' % (tot, A))
    ov = [o for o in r['overlaps'] if abs(o[2]) > tol]
    ok &= P.expect(label, not ov, 'overlapping pieces (i, j, m²): %s' % ov[:3])
    ok &= P.expect(label, r.get('symdiff', 0) <= tol, 'union differs from the original by %.6f m²' % r.get('symdiff', 0))
    tiny = [a for a in r['area'] if a < 1e-3]
    ok &= P.expect(label, not tiny, 'sliver pieces: %s' % tiny)
    if faces_from is not None and orig['type'] == 'Polygon':
        faces = r['faces'][0]
        got = sorted(r['area'])
        ok &= P.expect(label, len(got) == len(faces) and all(abs(a - b) <= tol for a, b in zip(got, faces)),
                       'piece areas %s, geometry gives %s' % ([round(a, 3) for a in got], [round(a, 3) for a in faces]))
    return r


def faces_of(orig, lines, m):
    r = shapely_job(geoms=[], orig=gmap(orig, m), lines=[gmap(l, m) for l in lines])
    return r['faces']


# ---------- running a split ----------

def run_two_layers(command, targets, tools, params=None, target_props=None):
    """Targets in layer 'tg' (all selected), tools in the active layer 'tl' (all selected).
    Returns (result or None, error or None, before, after, tools_after, restored-after-undo)."""
    tp = target_props or [{} for _ in targets]
    tg = write_layer('tg', [feature(g, name='t%d' % i, **tp[i]) for i, g in enumerate(targets)])
    tl = write_layer('tl', [feature(g, name='k%d' % i) for i, g in enumerate(tools)])
    with Session(tg, tl) as s:
        before, tools_before = s.features('tg'), s.features('tl')
        s.select('tg', range(len(targets)))
        s.activate('tl')
        s.select('tl', range(len(tools)), add=True)
        r, err = None, None
        try:
            r = s.plugin(command, params or {})
        except CommandError as e:
            err = str(e)
        after, tools_after = s.features('tg'), s.features('tl')
        restored = None
        if r is not None:
            s.undo()
            restored = (s.features('tg'), s.features('tl'))
        return dict(r=r, err=err, before=before, after=after, tools_before=tools_before,
                    tools_after=tools_after, restored=restored)


def pieces_of(after, i):
    return [f for f in after if f['properties'].get('name') == 't%d' % i]


def split_case(P, label, targets, tools, want=None, use_faces=True):
    """Splits `targets` (lon/lat) with `tools`; each target must become exactly the faces the
    lines cut it into (`want[i]` pieces when given, else the polygonize count)."""
    x = run_two_layers('split-with-line', targets, tools, {'keepTools': True})
    m = metric(targets[0])
    expected = []
    for i, g in enumerate(targets):
        if want is not None and want[i] is not None:
            expected.append(want[i])
        else:
            f = faces_of(g, tools, m)
            # unsplit parts of a multipolygon go with the largest piece
            split = [len(p) for p in f if len(p) > 1]
            expected.append(sum(split) if split else 1)
    if x['err'] is not None:
        P.expect(label, all(n == 1 for n in expected), 'refused (%s) but pieces expected %s' % (short(x['err']), expected))
        P.expect(label, fingerprint(x['after']) == fingerprint(x['before']), 'a refused split changed the data')
        P.expect(label, actionable(x['err']), 'message not actionable: %s' % short(x['err']))
        return x
    for i, g in enumerate(targets):
        ps = pieces_of(x['after'], i)
        if expected[i] == 1:
            P.expect(label, len(ps) == 1 and same_geom(ps[0]['geometry'], g, 1e-12),
                     'target %d should be unchanged, got %d pieces' % (i, len(ps)))
            continue
        check_partition(P, '%s t%d' % (label, i), g, [p['geometry'] for p in ps], m, want=expected[i],
                        faces_from=tools if use_faces and g['type'] == 'Polygon' else None)
        P.expect(label, all(p['properties'] == {'name': 't%d' % i} for p in ps), 'attributes not copied')
    allp = [f['geometry'] for f in x['after'] if f['geometry']['type'] in ('Polygon', 'MultiPolygon')]
    if len(targets) > 1 and allp:
        r = shapely_job(geoms=[gmap(g, m) for g in allp])
        ov = [o for o in r['overlaps'] if abs(o[2]) > 1e-4]
        P.expect(label, not ov, 'pieces of different targets overlap: %s' % ov[:3])
    rest = x['restored']
    P.expect(label, rest is not None and fingerprint(rest[0]) == fingerprint(x['before']), 'undo did not restore')
    P.expect(label, fingerprint(x['tools_after']) == fingerprint(x['tools_before']), 'keepTools: the tools changed')
    return x


O = PL
SQ = sq(0, 0, 100)


def T(*rings):
    return poly(O, *rings)


def L(*pts):
    return line(O, list(pts))


# ---------- split: multiple crossings ----------

def test_split_zigzag_many_crossings():
    P = Problems()
    # top to bottom four times: 5 strips
    split_case(P, 'zigzag 4 full crossings', [T(SQ)],
               [L((10, -10), (30, 110), (50, -10), (70, 110), (90, -10))], want=[5])
    # three dips through the bottom edge: 3 triangles + the rest
    split_case(P, 'sawtooth 3 dips', [T(SQ)],
               [L((5, -10), (15, 10), (25, -10), (35, 10), (45, -10), (55, 10), (65, -10))], want=[4])
    # enters and leaves the same (left) edge six times: a comb of bites
    split_case(P, 'comb 6 crossings', [T(SQ)],
               [L((-10, 10), (20, 15), (-10, 20), (20, 35), (-10, 40), (20, 55), (-10, 60))], want=[4])
    P.done()


def test_split_two_lines_crossing():
    P = Problems()
    split_case(P, 'X', [T(SQ)], [L((-10, -10), (110, 110)), L((-10, 110), (110, -10))], want=[4])
    split_case(P, 'X one feature multiline-ish',
               [T(SQ)], [L((-10, 50), (110, 50)), L((50, -10), (50, 110))], want=[4])
    P.done()


def test_split_three_lines_triangle():
    P = Problems()
    # three full-crossing lines whose pairwise crossings are inside: 7 faces, one triangle
    split_case(P, 'triangle', [T(SQ)],
               [L((-10, 20), (110, 20)), L((10, -10), (70, 110)), L((90, -10), (30, 110))], want=[7])
    P.done()


def test_split_grid_two_by_two():
    P = Problems()
    split_case(P, '# grid', [T(SQ)],
               [L((30, -10), (30, 110)), L((70, -10), (70, 110)), L((-10, 30), (110, 30)), L((-10, 70), (110, 70))],
               want=[9])
    P.done()


def test_split_line_crossing_itself():
    P = Problems()
    # enters from the left, loops (crossing itself at (50, 50)), leaves on the right
    loop = L((-10, 50), (60, 50), (60, 70), (40, 70), (40, 40), (110, 40))
    split_case(P, 'loop', [T(SQ)], [loop])
    # figure of eight inside, the line going across
    # a figure of eight drawn on the way across
    split_case(P, 'figure-8', [T(SQ)], [L((-10, 20), (40, 20), (50, 30), (60, 20), (50, 10), (40, 20), (40, 20.0), (110, 20))])
    P.done()


# ---------- split: vertices, edges, tangents ----------

def test_split_through_vertices_and_along_edges():
    P = Problems()
    split_case(P, 'diagonal through 2 corners', [T(SQ)], [L((-10, -10), (110, 110))], want=[2])
    split_case(P, 'through a vertex of a pentagon',
               [T([(0, 0), (100, 0), (100, 100), (50, 150), (0, 100)])], [L((50, -10), (50, 160))], want=[2])
    split_case(P, 'collinear along bottom edge then in', [T(SQ)], [L((-10, 0), (30, 0), (30, 110))], want=[2])
    split_case(P, 'collinear along an edge then out again', [T(SQ)],
               [L((-10, 0), (30, 0), (30, 50), (60, 50), (60, 0), (110, 0))], want=[2])
    split_case(P, 'starts and ends exactly on the boundary', [T(SQ)], [L((0, 50), (100, 50))], want=[2])
    split_case(P, 'starts on a corner ends on an edge', [T(SQ)], [L((0, 0), (100, 50))], want=[2])
    P.done()


def test_split_tangent_and_dangling_lines_do_not_split():
    P = Problems()
    split_case(P, 'touches a corner', [T(SQ)], [L((-10, 10), (10, -10))], want=[1])
    split_case(P, 'touches an edge from outside', [T(SQ)], [L((40, -10), (50, 0), (60, -10))], want=[1])
    split_case(P, 'runs along an edge only', [T(SQ)], [L((-10, 0), (110, 0))], want=[1])
    split_case(P, 'dangles inside', [T(SQ)], [L((-10, 50), (50, 50))], want=[1])
    split_case(P, 'touches an edge from inside', [T(SQ)], [L((20, 50), (50, 0), (80, 50))], want=[1])
    P.done()


def test_split_crosses_then_dangles():
    P = Problems()
    # crosses side to side, comes back in and ends inside: 2 pieces, the dangle is ignored
    split_case(P, 'cross then dangle', [T(SQ)], [L((-10, 50), (110, 50), (110, 80), (60, 80))], want=[2])
    P.done()


# ---------- split: holes ----------

HOLE = cw(sq(30, 30, 40))


def test_split_polygon_with_hole():
    P = Problems()
    split_case(P, 'through the hole', [T(SQ, HOLE)], [L((-10, 50), (110, 50))], want=[2])
    split_case(P, 'grazing the hole band', [T(SQ, HOLE)], [L((-10, 35), (110, 35))], want=[2])
    split_case(P, 'twice through the hole', [T(SQ, HOLE)], [L((-10, 50), (110, 50)), L((50, -10), (50, 110))], want=[4])
    split_case(P, 'along the hole edge', [T(SQ, HOLE)], [L((-10, 30), (110, 30))], want=[2])
    split_case(P, 'through a hole corner only', [T(SQ, HOLE)], [L((-10, 70), (70, -10))], want=[2])
    split_case(P, 'from the hole to the outside once', [T(SQ, HOLE)], [L((50, 50), (50, -10))], want=[1])
    split_case(P, 'tangent to the hole from inside the hole', [T(SQ, HOLE)], [L((40, 50), (30, 60), (40, 70))], want=[1])
    P.done()


def test_split_comb_by_one_line():
    P = Problems()
    comb = [(0, 0), (100, 0), (100, 80), (90, 80), (90, 20), (70, 20), (70, 80), (60, 80), (60, 20), (40, 20),
            (40, 80), (30, 80), (30, 20), (10, 20), (10, 80), (0, 80)]
    split_case(P, 'comb 4 teeth', [T(comb)], [L((-10, 50), (110, 50))], want=[5])
    u = [(0, 0), (100, 0), (100, 100), (70, 100), (70, 30), (30, 30), (30, 100), (0, 100)]
    split_case(P, 'U', [T(u)], [L((-10, 60), (110, 60))], want=[3])
    P.done()


def test_split_multipolygon_parts():
    P = Problems()
    mp = mpoly(O, [sq(0, 0, 40)], [sq(100, 0, 40)])
    split_case(P, 'MP one part', [mp], [L((20, -10), (20, 50))], want=[2])
    split_case(P, 'MP both parts', [mp], [L((-10, 20), (150, 20))], want=[4])
    P.done()


def test_split_adjacent_polygons_along_shared_edge():
    P = Problems()
    x = split_case(P, 'shared edge', [T(sq(0, 0, 100)), T(sq(100, 0, 100))],
                   [L((-10, 30), (100, 30), (100, 70), (210, 70))], want=[2, 2])
    if x['err'] is None:
        old = [c for f in x['before'] for c in flat(f['geometry'])]
        new = [c for f in x['after'] for c in flat(f['geometry'])]
        lost = [c for c in old if not any(c == d for d in new)]
        P.expect('shared edge', not lost, 'original corners lost or moved: %s' % lost[:3])
        # every vertex on x=100 of one side is a vertex of the other side too (no T-junction slivers)
        m = metric(x['before'][0]['geometry'])
        left = [c for f in x['after'] if f['properties']['name'] == 't0' for c in flat(f['geometry'])]
        right = [c for f in x['after'] if f['properties']['name'] == 't1' for c in flat(f['geometry'])]
        xs = m(x['before'][1]['geometry']['coordinates'][0][0])[0]
        on_l = {tuple(c) for c in left if abs(m(c)[0] - xs) < 1e-6}
        on_r = {tuple(c) for c in right if abs(m(c)[0] - xs) < 1e-6}
        P.expect('shared edge', len(on_l) >= 3 and len(on_r) >= 3,
                 'vertices on the shared edge: left %d, right %d' % (len(on_l), len(on_r)))
    P.done()


def lonlat_linear(origin, pts):
    """Metres -> lon/lat linearly (a constant-latitude edge is then slightly curved in the
    plugin's frame, by well under a millimetre over 100 m: what a line digitised along a
    parcel edge in a geographic view looks like)."""
    return [offset_m(origin[0], origin[1], x, y) for x, y in pts]


def test_split_tool_within_a_tenth_of_a_millimetre_of_an_edge():
    """The plugin snaps such lines onto the edges (within 1 mm), so the polygonize faces of the
    raw lines, which include the sub-millimetre slivers, are not the expected pieces here."""
    P = Problems()
    tgt = {'type': 'Polygon', 'coordinates': [lonlat_linear(O, closed(SQ))]}

    def ln(*pts):
        return {'type': 'LineString', 'coordinates': lonlat_linear(O, pts)}
    split_case(P, 'along the bottom edge then in', [tgt], [ln((-10, 0), (30, 0), (30, 110))], want=[2],
               use_faces=False)
    split_case(P, 'ends on the edges', [tgt], [ln((0, 50), (100, 50))], want=[2], use_faces=False)
    split_case(P, 'along an edge only', [tgt], [ln((-10, 0), (110, 0))], want=[1], use_faces=False)
    two = [{'type': 'Polygon', 'coordinates': [lonlat_linear(O, closed(sq(0, 0, 100)))]},
           {'type': 'Polygon', 'coordinates': [lonlat_linear(O, closed(sq(100, 0, 100)))]}]
    split_case(P, 'along the shared edge of two parcels', two, [ln((-10, 30), (100, 30), (100, 70), (210, 70))],
               want=[2, 2], use_faces=False)
    P.done()


def test_split_thin_polygon_acute_angle():
    P = Problems()
    t = math.tan(math.radians(1))
    thin = T(sq(0, 0, 100, 0.01))
    split_case(P, '1 cm wide, 1 degree', [thin], [L((50 - 300, 0.005 - 300 * t), (50 + 300, 0.005 + 300 * t))], want=[2])
    split_case(P, '1 cm wide, across', [thin], [L((50, -1), (50, 1))], want=[2])
    P.done()


def test_split_far_from_the_centre_of_the_frame():
    P = Problems()
    for origin in [(14.2, 52.0), (24.1, 50.5), (14.13, 53.9), ABROAD, (-73.9, 40.7), (151.2, -33.9)]:
        sqo = poly(origin, SQ)
        split_case(P, 'X at %s' % (origin,), [sqo],
                   [line(origin, [(-10, -10), (110, 110)]), line(origin, [(-10, 110), (110, -10)])], want=[4])
    P.done()


def test_split_degenerate_tool_lines():
    P = Problems()
    split_case(P, 'duplicate vertices', [T(SQ)], [L((-10, 50), (50, 50), (50, 50), (50, 50), (110, 50))], want=[2])
    split_case(P, 'zero-length segment at the boundary', [T(SQ)], [L((-10, 50), (0, 50), (0, 50), (110, 50))], want=[2])
    # a closed ring wholly inside cuts out the island it encloses (see the help)
    split_case(P, 'closed ring inside', [T(SQ)], [L((30, 30), (70, 30), (70, 70), (30, 70), (30, 30))], want=[2])
    split_case(P, 'closed ring across the boundary', [T(SQ)],
               [L((-20, 30), (50, 30), (50, 70), (-20, 70), (-20, 30))], want=[2])
    P.done()


# ---------- split: line targets ----------

def line_split_case(P, label, target, tools, want, lengths=None):
    x = run_two_layers('split-with-line', [target], tools, {'keepTools': True})
    m = metric(target)
    if x['err'] is not None:
        P.expect(label, want == 1, 'refused (%s), expected %d pieces' % (short(x['err']), want))
        P.expect(label, fingerprint(x['after']) == fingerprint(x['before']), 'a refused split changed the data')
        return x
    ps = [p['geometry'] for p in pieces_of(x['after'], 0)]
    r = shapely_job(geoms=[gmap(g, m) for g in ps], orig=gmap(target, m))
    P.expect(label, len(ps) == want, '%d pieces, expected %d (lengths %s)' % (len(ps), want, [round(a, 3) for a in r['length']]))
    P.expect(label, abs(sum(r['length']) - r['orig_length']) <= 1e-6 * r['orig_length'] + 1e-6,
             'length %.6f, original %.6f' % (sum(r['length']), r['orig_length']))
    P.expect(label, all(a > 1e-6 for a in r['length']), 'zero-length pieces: %s' % r['length'])
    if lengths:
        got = sorted(r['length'])
        P.expect(label, len(got) == len(lengths) and all(abs(a - b) < 1e-3 * b for a, b in zip(got, sorted(lengths))),
                 'lengths %s, expected about %s' % ([round(a, 3) for a in got], sorted(lengths)))
    rest = x['restored']
    P.expect(label, rest is not None and fingerprint(rest[0]) == fingerprint(x['before']), 'undo did not restore')
    return x


def test_split_line_targets():
    P = Problems()
    line_split_case(P, 'zigzag crossing 5 times', L((0, 0), (300, 0)),
                    [L((25, -10), (75, 10), (125, -10), (175, 10), (225, -10), (275, 10))], want=6)
    line_split_case(P, 'crossing at an existing vertex', L((0, 0), (100, 0), (200, 0)), [L((100, -10), (100, 10))],
                    want=2, lengths=[100, 100])
    line_split_case(P, 'crossing at a bend vertex', L((0, 0), (100, 0), (100, 100)), [L((90, 10), (110, -10))],
                    want=2, lengths=[100, 100])
    line_split_case(P, 'closed ring cut through its start vertex', L((0, 0), (100, 0), (100, 100), (0, 100), (0, 0)),
                    [L((-10, -10), (110, 110))], want=2, lengths=[200, 200])
    line_split_case(P, 'two cutters at one point', L((0, 0), (100, 0)),
                    [L((50, -10), (50, 10)), L((40, -10), (60, 10))], want=2, lengths=[50, 50])
    P.done()


def test_split_line_by_collinear_overlap():
    P = Problems()
    # The cutter overlaps the target from x=50 on; it never crosses it. Whatever the plugin
    # does, it must not lose length nor produce zero-length pieces.
    x = line_split_case(P, 'collinear overlap', L((0, 0), (100, 0)), [L((50, 0), (150, 0))], want=1)
    if x['err'] is None:
        P.add('collinear overlap', 'note: split into %d pieces' % len(pieces_of(x['after'], 0)))
    P.done()


# ---------- reshape ----------

def reshape_case(P, label, target, tool, want_area=None, refuse=False):
    x = run_two_layers('reshape', [target], [tool], {'keepTools': True})
    m = metric(target)
    if x['err'] is not None:
        P.expect(label, refuse, 'refused: %s' % short(x['err']))
        P.expect(label, fingerprint(x['after']) == fingerprint(x['before']), 'a refused reshape changed the data')
        P.expect(label, bool(x['err'].strip()), 'empty message')
        return x
    g = x['after'][0]['geometry']
    r = shapely_job(geoms=[gmap(g, m)], orig=gmap(target, m))
    P.expect(label, not refuse, 'expected a refusal, got %s area %.3f' % (g['type'], r['area'][0]))
    P.expect(label, r['valid'][0], 'invalid result: %s' % r['why'][0])
    if want_area is not None:
        P.expect(label, abs(r['area'][0] - want_area) <= 1e-3 * want_area, 'area %.3f, expected %.3f' % (r['area'][0], want_area))
    P.expect(label, len(x['after']) == 1, '%d features' % len(x['after']))
    return x


def test_reshape_four_crossings():
    P = Problems()
    # crosses left edge, bottom twice, right edge: the bump under the bottom edge is added
    # to the upper half, the only valid outcome
    reshape_case(P, '4 crossings', T(SQ), L((-10, 50), (30, 50), (30, -10), (60, -10), (60, 50), (110, 50)),
                 want_area=6800)
    P.done()


def test_reshape_self_touching_results():
    """A line reaching the opposite side inside the polygon would cut it in two: refused with a
    pointer to Split With Line, rather than keeping one side or a pinched outline."""
    P = Problems()
    # The bite reaches the opposite edge along a stretch: the remainder would be two rectangles.
    x = reshape_case(P, 'bite to the opposite edge', T(SQ), L((30, 110), (30, 0), (70, 0), (70, 110)), refuse=True)
    P.expect('bite to the opposite edge', x['err'] is None or 'Split With Line' in x['err'], short(x['err']))
    # The bite touches the opposite edge at one point: two parts meeting at a point.
    x = reshape_case(P, 'bite touching a point', T(SQ), L((30, 110), (50, 0), (70, 110)), refuse=True)
    P.expect('bite touching a point', x['err'] is None or 'Split With Line' in x['err'], short(x['err']))
    P.done()


def test_reshape_hole_and_multipart():
    P = Problems()
    reshape_case(P, 'hole and shell', T(SQ, HOLE), L((-10, 50), (110, 50)), refuse=True)
    reshape_case(P, 'grow the hole', T(SQ, HOLE), L((40, 40), (40, 10), (60, 10), (60, 40)), want_area=10000 - 1600 - 400)
    mp = mpoly(O, [sq(0, 0, 40)], [sq(100, 0, 40)])
    reshape_case(P, 'line across both parts', mp, L((20, -10), (20, 20), (120, 20), (120, -10)), refuse=True)
    reshape_case(P, 'one crossing only', T(SQ), L((50, 50), (50, 150)), refuse=True)
    P.done()


def test_reshape_hole_pushed_to_shell_is_valid():
    """The hole grown until it reaches the shell: the result must stay a valid polygon."""
    P = Problems()
    reshape_case(P, 'hole to shell', T(SQ, HOLE), L((40, 40), (40, 0), (60, 0), (60, 40)), want_area=7800)
    P.done()


# ---------- offset ----------

def offset_run(target, params):
    tg = write_layer('ofs', [feature(target, name='o')])
    with Session(tg) as s:
        s.activate('ofs')
        s.select('ofs', [0])
        before = s.features('ofs')
        try:
            r = s.plugin('offset', params)
        except CommandError as e:
            return None, str(e), before, s.features('ofs')
        return r, None, before, s.features('ofs')


def offset_dist_case(P, label, pts, d, left, want_parts=None):
    tgt = L(*pts)
    r, err, before, after = offset_run(tgt, {'distance': d, 'left': left})
    m = metric(tgt)
    if err is not None:
        P.add(label, 'refused: %s' % short(err))
        P.expect(label, fingerprint(after) == fingerprint(before), 'a refused offset changed the data')
        return
    g = after[-1]['geometry']
    gm = gmap(g, m)
    parts = [gm['coordinates']] if gm['type'] == 'LineString' else gm['coordinates']
    pts_all = []
    for p in parts:
        pts_all.append(list(p) + [((a[0] + b[0]) / 2, (a[1] + b[1]) / 2) for a, b in zip(p, p[1:])])
    res = shapely_job(geoms=[], dist_to=gmap(tgt, m), pts=pts_all)
    ds = [d_ for row in res['dist'] for d_ in row]
    P.expect(label, min(ds) >= d * 0.995 - 1e-3, 'offset comes within %.4f m of the line (d=%g)' % (min(ds), d))
    P.expect(label, max(ds) <= d + 1e-3, 'offset lies %.4f m from the line (d=%g)' % (max(ds), d))
    from test_lines import segments_cross
    P.expect(label, not any(segments_cross(p) for p in parts), 'the offset crosses itself (loops)')
    if want_parts is not None:
        P.expect(label, len(parts) == want_parts, '%d parts, expected %d' % (len(parts), want_parts))


def test_offset_zigzag_larger_than_segments():
    P = Problems()
    zig = [(0, 0), (5, 5), (10, 0), (15, 5), (20, 0), (25, 5), (30, 0)]
    for left in (False, True):
        offset_dist_case(P, 'zigzag d=10 left=%s' % left, zig, 10, left)
        offset_dist_case(P, 'zigzag d=40 left=%s' % left, zig, 40, left)
    P.done()


def test_offset_spike_and_short_segments():
    P = Problems()
    offset_dist_case(P, 'spike 180 degrees right', [(0, 0), (50, 0), (20, 0)], 5, False)
    offset_dist_case(P, 'spike 180 degrees left', [(0, 0), (50, 0), (20, 0)], 5, True)
    offset_dist_case(P, 'near spike', [(0, 0), (50, 0), (0, 0.01)], 5, False)
    circ = [(10 * math.cos(2 * math.pi * k / 400), 10 * math.sin(2 * math.pi * k / 400)) for k in range(400)]
    circ.append(circ[0])
    offset_dist_case(P, 'dense ring outwards', circ, 3, False, want_parts=1)
    offset_dist_case(P, 'dense ring inwards', circ, 3, True, want_parts=1)
    mm = [(i * 0.001, (i % 2) * 0.001) for i in range(40)]
    offset_dist_case(P, '1 mm segments', mm, 1, False)
    P.done()


def test_offset_dense_noisy_line():
    """A densely digitised line (10 cm spacing, 2 cm jitter, 300 vertices) offset by 20 m:
    far below the documented 2000-vertex limit."""
    P = Problems()
    pts = [(i * 0.1, 3 * math.sin(i * 0.01) + (0.02 if i % 2 else 0)) for i in range(300)]
    offset_dist_case(P, 'noisy 300 vertices d=20', pts, 20, False)
    zig = [(i * 0.001, (i % 2) * 0.001) for i in range(100)]
    offset_dist_case(P, '100 vertices 1 mm zigzag d=1', zig, 1, False)
    P.done()


def test_offset_ring_inward_beyond_collapse():
    P = Problems()
    for d in (10, 10.000001, 15):
        r, err, before, after = offset_run(L((0, 0), (20, 0), (20, 20), (0, 20), (0, 0)), {'distance': d, 'left': True})
        P.expect('ring d=%g' % d, err is not None, 'collapsed ring offset produced %s' % (after[-1]['geometry'] if r else None))
        if err:
            P.expect('ring d=%g' % d, fingerprint(before) == fingerprint(after), 'a refused offset changed the data')
        r, err, before, after = offset_run(T(sq(0, 0, 20)), {'distance': d, 'left': True})
        P.expect('polygon d=%g' % d, err is not None, 'collapsed polygon offset produced %s' % (after[-1]['geometry'] if r else None))
    P.done()


def test_offset_polygon_with_hole():
    P = Problems()
    for d, want, holes in ((5, 10000 + 4 * 100 * 5 + math.pi * 25 - 30 * 30, 1),
                           (25, 10000 + 4 * 100 * 25 + math.pi * 625, 0)):
        tgt = T(SQ, HOLE)
        r, err, before, after = offset_run(tgt, {'distance': d})
        if not P.expect('outwards d=%g' % d, err is None, 'refused: %s' % short(err)):
            continue
        g = after[-1]['geometry']
        m = metric(tgt)
        res = shapely_job(geoms=[gmap(g, m)])
        P.expect('outwards d=%g' % d, res['valid'][0], res['why'][0])
        P.expect('outwards d=%g' % d, abs(res['area'][0] - want) < 0.005 * want, 'area %.2f, expected %.2f' % (res['area'][0], want))
        nh = sum(len(p) - 1 for p in ([g['coordinates']] if g['type'] == 'Polygon' else g['coordinates']))
        P.expect('outwards d=%g' % d, nh == holes, '%d holes, expected %d' % (nh, holes))
    # inwards: the hole grows, the shell shrinks
    tgt = T(SQ, HOLE)
    r, err, before, after = offset_run(tgt, {'distance': 5, 'left': True})
    if P.expect('inwards d=5', err is None, 'refused: %s' % short(err)):
        g = after[-1]['geometry']
        res = shapely_job(geoms=[gmap(g, metric(tgt))])
        want = 90 * 90 - (50 * 50 - (4 - math.pi) * 25)
        P.expect('inwards d=5', res['valid'][0], res['why'][0])
        P.expect('inwards d=5', abs(res['area'][0] - want) < 0.005 * want, 'area %.2f, expected %.2f' % (res['area'][0], want))
    P.done()


# ---------- trim / extend ----------

def trim_case(P, label, boundary, target, want_len=None, refuse=False, unchanged=False):
    x = run_two_layers('trim-extend', [target], [boundary], {'distance': 1})
    m = metric(target)
    if x['err'] is not None:
        P.expect(label, refuse or unchanged, 'refused: %s' % short(x['err']))
        P.expect(label, fingerprint(x['after']) == fingerprint(x['before']), 'a refused trim/extend changed the data')
        P.expect(label, bool(x['err'].strip()), 'empty message')
        return x
    P.expect(label, not refuse, 'expected a refusal; result %s' % x['r'])
    g = x['after'][0]['geometry']
    if unchanged:
        P.expect(label, same_geom(g, target, 0.0), 'the line changed')
        return x
    r = shapely_job(geoms=[gmap(g, m)])
    if want_len is not None:
        P.expect(label, abs(r['length'][0] - want_len) <= 1e-4 * want_len, 'length %.4f, expected %.4f' % (r['length'][0], want_len))
    return x


def test_trim_extend_polygon_boundary():
    P = Problems()
    box = T(sq(100, 0, 100))
    trim_case(P, 'extend to polygon', box, L((0, 50), (50, 50)), want_len=100)
    trim_case(P, 'ends exactly on the boundary', box, L((0, 50), (100, 50)), unchanged=True)
    trim_case(P, 'parallel to the boundary', box, L((0, 150), (50, 150)), refuse=True)
    trim_case(P, 'crosses the boundary twice: trim far end', box, L((0, 50), (250, 50)), want_len=200)
    trim_case(P, 'points away: extend the start', box, L((50, 50), (0, 50)), want_len=100)
    trim_case(P, 'starts inside, ends outside', box, L((150, 50), (250, 50)), want_len=50)
    trim_case(P, 'extends to a corner exactly', box, L((0, -100), (50, -50)), want_len=100 * math.sqrt(2))
    trim_case(P, 'grazes an edge when extended', box, L((0, 0), (50, 0)), want_len=100)
    P.done()


def test_trim_extend_line_boundary_crossed_twice():
    P = Problems()
    # boundary is a U; the target crosses both arms: the end nearest is trimmed back to the
    # crossing nearest to it
    bound = L((100, -50), (100, 50), (150, 50), (150, -50))
    trim_case(P, 'crosses a U twice', bound, L((0, 0), (160, 0)), want_len=150)
    trim_case(P, 'inside the U: the nearer end only', bound, L((110, 0), (130, 0)), want_len=30)
    P.done()
