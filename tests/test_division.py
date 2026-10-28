"""Tests for src/division.rs: Divide Into Equal Parts, Cut Off Percent and the Divide Parcel
tool.

Areas are checked against an ellipsoidal area computed here independently of the plugin:
each edge is densified as a straight line of the transverse Mercator frame (that is the edge
the plugin cuts), and the densified ring is measured exactly in the GRS80 cylindrical
equal-area projection (authalic latitude)."""

import json
import math
import os

from aelib import (ABROAD, PL, PREFIX, CommandError, Session, Skip, check, feature, linestring,
                   multipolygon, offset_m, polygon, rect, run_once, tmpdir, write_layer)

# Commands and tools of this module that are still stubs: remove each one as it is
# implemented.
STUBS = []
TOOL_STUBS = []

TOOL = PREFIX + 'divide-parcel'

# ---------- independent ellipsoidal area ----------

GRS80_A = 6378137.0
GRS80_F = 1 / 298.257222101


class TM:
    """Transverse Mercator, Krueger series to n^4 (plenty for densifying edges)."""

    def __init__(self, lon0, k0, f=GRS80_F, a=GRS80_A):
        n = f / (2 - f)
        self.lon0, self.k0 = math.radians(lon0), k0
        self.e = math.sqrt(f * (2 - f))
        self.A = a / (1 + n) * (1 + n * n / 4 + n ** 4 / 64)
        self.alp = [n / 2 - 2 * n * n / 3 + 5 * n ** 3 / 16 + 41 * n ** 4 / 180,
                    13 * n * n / 48 - 3 * n ** 3 / 5 + 557 * n ** 4 / 1440,
                    61 * n ** 3 / 240 - 103 * n ** 4 / 140,
                    49561 * n ** 4 / 161280]
        self.bet = [n / 2 - 2 * n * n / 3 + 37 * n ** 3 / 96 - n ** 4 / 360,
                    n * n / 48 + n ** 3 / 15 - 437 * n ** 4 / 1440,
                    17 * n ** 3 / 480 - 37 * n ** 4 / 840,
                    4397 * n ** 4 / 161280]

    def fwd(self, lon, lat):
        e, phi, lam = self.e, math.radians(lat), math.radians(lon) - self.lon0
        tau = math.tan(phi)
        sig = math.sinh(e * math.atanh(e * tau / math.sqrt(1 + tau * tau)))
        taup = tau * math.sqrt(1 + sig * sig) - sig * math.sqrt(1 + tau * tau)
        xip = math.atan2(taup, math.cos(lam))
        etap = math.asinh(math.sin(lam) / math.sqrt(taup * taup + math.cos(lam) ** 2))
        xi, eta = xip, etap
        for j, a in enumerate(self.alp, 1):
            xi += a * math.sin(2 * j * xip) * math.cosh(2 * j * etap)
            eta += a * math.cos(2 * j * xip) * math.sinh(2 * j * etap)
        return self.k0 * self.A * eta, self.k0 * self.A * xi

    def inv(self, x, y):
        xi, eta = y / (self.k0 * self.A), x / (self.k0 * self.A)
        xip, etap = xi, eta
        for j, b in enumerate(self.bet, 1):
            xip -= b * math.sin(2 * j * xi) * math.cosh(2 * j * eta)
            etap -= b * math.cos(2 * j * xi) * math.sinh(2 * j * eta)
        taup = math.sin(xip) / math.sqrt(math.sinh(etap) ** 2 + math.cos(xip) ** 2)
        lam = math.atan2(math.sinh(etap), math.cos(xip))
        e = self.e
        tau = taup
        for _ in range(6):
            sig = math.sinh(e * math.atanh(e * tau / math.sqrt(1 + tau * tau)))
            ti = tau * math.sqrt(1 + sig * sig) - sig * math.sqrt(1 + tau * tau)
            tau += (taup - ti) / math.sqrt(1 + ti * ti) * (1 + (1 - e * e) * tau * tau) / (
                (1 - e * e) * math.sqrt(1 + tau * tau))
        return math.degrees(lam + self.lon0), math.degrees(math.atan(tau))


def frame_of(lon):
    """The plugin's frame for a feature at `lon`: EPSG:2180 in Poland, else its UTM zone."""
    if 14 <= lon <= 24.2:
        return TM(19.0, 0.9993)
    return TM((math.floor((lon + 180) / 6) + 1) * 6 - 183, 0.9996, f=1 / 298.257223563)


def _q(phi, e):
    s = math.sin(phi)
    return (1 - e * e) * (s / (1 - e * e * s * s) - math.log((1 - e * s) / (1 + e * s)) / (2 * e))


def ring_area_ell(ring, tm, step=2.0):
    e = tm.e
    pts = []
    for a, b in zip(ring, ring[1:]):
        (xa, ya), (xb, yb) = tm.fwd(*a), tm.fwd(*b)
        k = max(1, int(math.hypot(xb - xa, yb - ya) / step))
        pts.append(a)
        pts.extend(tm.inv(xa + (xb - xa) * i / k, ya + (yb - ya) * i / k) for i in range(1, k))
    pts.append(ring[-1])
    cea = [(GRS80_A * math.radians(x), GRS80_A * _q(math.radians(y), e) / 2) for x, y in pts]
    x0, y0 = cea[0]
    cea = [(x - x0, y - y0) for x, y in cea]
    return abs(sum(p[0] * q[1] - q[0] * p[1] for p, q in zip(cea, cea[1:]))) / 2


def area_ell(geom):
    """Ellipsoidal area (m²) of a Polygon/MultiPolygon with TM-straight edges."""
    polys = [geom['coordinates']] if geom['type'] == 'Polygon' else geom['coordinates']
    tm = frame_of(polys[0][0][0][0])
    return sum(ring_area_ell(p[0], tm) - sum(ring_area_ell(h, tm) for h in p[1:]) for p in polys)


def tol(a):
    return max(0.01, 1e-6 * a)


# ---------- geometry checks ----------

def coords(geom):
    polys = [geom['coordinates']] if geom['type'] == 'Polygon' else geom['coordinates']
    return [c for p in polys for r in p for c in r]


def to_m(lon0, lat0):
    """lon/lat -> metres of the plugin's frame, relative to (lon0, lat0)."""
    tm = frame_of(lon0)
    x0, y0 = tm.fwd(lon0, lat0)

    def m(c):
        x, y = tm.fwd(c[0], c[1])
        return x - x0, y - y0
    return m


def sweep(bearing):
    b = math.radians(bearing)
    return (math.cos(b), -math.sin(b))


def seg_dist(p, a, b):
    dx, dy = b[0] - a[0], b[1] - a[1]
    t = max(0.0, min(1.0, ((p[0] - a[0]) * dx + (p[1] - a[1]) * dy) / (dx * dx + dy * dy or 1)))
    return math.hypot(p[0] - a[0] - t * dx, p[1] - a[1] - t * dy)


def inside(p, geom, m, eps=0.01):
    """`p` (metres) lies in `geom` or within `eps` of its boundary."""
    polys = [geom['coordinates']] if geom['type'] == 'Polygon' else geom['coordinates']
    hit = False
    for poly in polys:
        for r in poly:
            rm = [m(c) for c in r]
            for a, b in zip(rm, rm[1:]):
                if seg_dist(p, a, b) <= eps:
                    return True
                if (a[1] > p[1]) != (b[1] > p[1]) and p[0] < a[0] + (p[1] - a[1]) * (b[0] - a[0]) / (b[1] - a[1]):
                    hit = not hit
    return hit


def check_cover(orig, pieces, bearing, reverse=False):
    """The pieces cover the original: same total area, every vertex inside it, and each piece
    lies after the previous one along the sweep (so they don't overlap)."""
    total = area_ell(orig)
    got = sum(area_ell(p) for p in pieces)
    check(abs(got - total) <= tol(total), 'pieces sum %.4f, original %.4f' % (got, total))
    c0 = orig['coordinates'][0][0] if orig['type'] == 'Polygon' else orig['coordinates'][0][0][0]
    m = to_m(*c0)
    n = sweep(bearing)
    if reverse:
        n = (-n[0], -n[1])
    prev_max = -math.inf
    for i, p in enumerate(pieces):
        pts = [m(c) for c in coords(p)]
        bad = [q for q in pts if not inside(q, orig, m)]
        check(not bad, 'piece %d has vertices outside the original: %s' % (i, bad[:3]))
        t = [n[0] * q[0] + n[1] * q[1] for q in pts]
        check(min(t) >= prev_max - 0.01, 'piece %d starts at %.3f before the previous end %.3f' % (i, min(t), prev_max))
        prev_max = max(t)


def check_parts_equal(pieces, total, parts):
    for i, p in enumerate(pieces):
        a = area_ell(p)
        check(abs(a - total / parts) <= tol(a), 'piece %d: %.4f m², expected %.4f' % (i, a, total / parts))


def same_features(a, b, eps=1e-9):
    if len(a) != len(b):
        return False
    for f, g in zip(a, b):
        if f['properties'] != g['properties'] or f['geometry']['type'] != g['geometry']['type']:
            return False
        ca, cb = coords(f['geometry']), coords(g['geometry'])
        if len(ca) != len(cb) or any(abs(x[0] - y[0]) > eps or abs(x[1] - y[1]) > eps for x, y in zip(ca, cb)):
            return False
    return True


# ---------- synthetic shapes (metres from an origin) ----------

def shape_m(origin, *rings_m):
    lon, lat = origin
    return polygon(*[[offset_m(lon, lat, x, y) for x, y in r] for r in rings_m])


def l_shape(origin=PL):
    return shape_m(origin, [(0, 0), (200, 0), (200, 80), (80, 80), (80, 240), (0, 240), (0, 0)])


def u_shape(origin=PL):
    """300 x 300 m, opening north: a 100 m wide notch from y = 100 up."""
    return shape_m(origin, [(0, 0), (300, 0), (300, 300), (200, 300), (200, 100), (100, 100),
                            (100, 300), (0, 300), (0, 0)])


def holed(origin=PL):
    return shape_m(origin, [(0, 0), (150, 0), (150, 120), (0, 120), (0, 0)],
                   [(50, 40), (50, 80), (100, 80), (100, 40), (50, 40)])


def two_squares(origin=PL):
    lon, lat = origin
    return multipolygon(polygon(rect(lon, lat, 100, 100)), polygon(rect(lon, lat, 120, 80, 150, 10)))


def shapes_layer():
    lon, lat = PL
    return write_layer('shapes', [
        feature(polygon(rect(lon, lat, 300, 100)), name='rect', n=0),
        feature(l_shape(), name='L', n=1),
        feature(holed(), name='holed', n=2),
        feature(u_shape(), name='U', n=3),
        feature(two_squares(), name='multi', n=4),
    ])


# ---------- divide-equal ----------

CASES = [('rect', 3, 0), ('L', 4, 45), ('holed', 3, 90), ('U', 2, 90), ('multi', 3, 90), ('rect', 7, 135)]


def test_divide_equal_shapes():
    with Session(shapes_layer()) as s:
        s.activate('shapes')
        ids0 = {f['properties']['name']: f['id'] for f in s.features('shapes')}
        for name, parts, angle in CASES:
            feats = s.features('shapes')
            row = next(i for i, f in enumerate(feats) if f['id'] == ids0[name])
            src = feats[row]
            s.select('shapes', [row])
            r = s.plugin('divide-equal', {'parts': parts, 'angle': angle})
            check(r['divided'] == 1 and r['created'] == parts - 1, r)
            check(r['maxError'] <= 0.01, r)
            after = s.features('shapes')
            check(len(after) == len(feats) + parts - 1, 'feature count')
            ids = s.selected_ids('shapes')
            check(len(ids) == parts and src['id'] in ids, 'selection %s' % ids)
            pieces = [next(f for f in after if f['id'] == i) for i in sorted(ids)]
            check(pieces[0]['id'] == src['id'], 'the first part keeps the original feature')
            for p in pieces:
                check(p['properties'] == src['properties'], p['properties'])
            total = area_ell(src['geometry'])
            check_parts_equal([p['geometry'] for p in pieces], total, parts)
            for p, a in zip(pieces, r['areas'][0]):
                check(abs(area_ell(p['geometry']) - a) <= tol(a), 'reported %.4f vs %.4f' % (a, area_ell(p['geometry'])))
            check_cover(src['geometry'], [p['geometry'] for p in pieces], angle)
            if name == 'U':
                # E-W cut through both arms: the first (northern) part is the two arm ends.
                check(pieces[0]['geometry']['type'] == 'MultiPolygon', pieces[0]['geometry']['type'])
                check(pieces[1]['geometry']['type'] == 'Polygon', pieces[1]['geometry']['type'])
            if name == 'multi':
                check(all(p['geometry']['type'] == 'MultiPolygon' for p in pieces), 'multipart pieces')
            if name == 'holed':
                rings = [len(q) for p in pieces for q in
                         ([p['geometry']['coordinates']] if p['geometry']['type'] == 'Polygon' else p['geometry']['coordinates'])]
                # The cuts pass north and south of the hole: the middle part keeps it.
                check(rings == [1, 2, 1], 'rings per part: %s' % rings)


def test_divide_equal_is_not_planar():
    """At 19°E, EPSG:2180's scale is 0.9993: planar parts would be 0.14 % too large."""
    lon, lat = PL
    with Session(write_layer('one', [feature(polygon(rect(lon, lat, 200, 100)), name='a')])) as s:
        s.activate('one')
        s.select('one', [0])
        r = s.plugin('divide-equal', {'parts': 2})
        total = area_ell(polygon(rect(lon, lat, 200, 100)))
        check(all(abs(a - total / 2) <= tol(a) for a in r['areas'][0]), (r['areas'], total))
        check(total / 2 - 1 < r['areas'][0][0] < total / 2 + 1, r)
        check('ellipsoidal' in r['message'], r['message'])


def test_divide_equal_order_and_reverse():
    lon, lat = PL
    cases = [(0, False, 'west'), (0, True, 'east'), (90, False, 'north'), (90, 'panel', 'south')]
    for angle, reverse, first in cases:
        g = polygon(rect(lon, lat, 300, 100) if angle == 0 else rect(lon, lat, 100, 300))
        with Session(write_layer('one', [feature(g, name='a')])) as s:
            s.activate('one')
            s.select('one', [0])
            params = {'parts': 3, 'angle': angle}
            if reverse == 'panel':
                params['panel'] = {'divide-reverse': True}
            else:
                params['reverse'] = reverse
            s.plugin('divide-equal', params)
            reverse = bool(reverse)
            f = s.features('one')
            cx = [sum(c[0] for c in coords(x['geometry'])) for x in f]
            cy = [sum(c[1] for c in coords(x['geometry'])) for x in f]
            ok = {'west': cx[0] < cx[1] < cx[2], 'east': cx[0] > cx[1] > cx[2],
                  'north': cy[0] > cy[1] > cy[2], 'south': cy[0] < cy[1] < cy[2]}[first]
            check(ok, '%s %s: part order %s %s' % (angle, reverse, cx, cy))
            check_cover(g, [x['geometry'] for x in f], angle, reverse)


def test_divide_equal_abroad_utm():
    lon, lat = ABROAD
    g = l_shape(ABROAD)
    with Session(write_layer('it', [feature(g, name='L')])) as s:
        s.activate('it')
        s.select('it', [0])
        r = s.plugin('divide-equal', {'parts': 5, 'angle': 30})
        pieces = [f['geometry'] for f in s.features('it')]
        check(len(pieces) == 5, r)
        check_parts_equal(pieces, area_ell(g), 5)
        check_cover(g, pieces, 30)


def test_divide_equal_undo_restores():
    with Session(shapes_layer()) as s:
        s.activate('shapes')
        before = s.features('shapes')
        s.select('shapes', [1, 3])
        r = s.plugin('divide-equal', {'parts': 4, 'angle': 10})
        check(r['divided'] == 2 and r['created'] == 6, r)
        check(len(s.features('shapes')) == 11, 'count after')
        s.undo()
        check(same_features(s.features('shapes'), before), 'undo did not restore the layer')


def test_divide_equal_ribbon_defaults():
    """No parameters (a ribbon button): the panel defaults, 2 parts with north-south lines."""
    lon, lat = PL
    data = write_layer('one', [feature(polygon(rect(lon, lat, 200, 100)), name='a')])
    out = tmpdir() / 'ribbon.geojson'
    code, text = run_once([('select.rows', {'layer': 'one', 'rows': [0]}),
                           (PREFIX + 'divide-equal', None),
                           ('data.convert', {'layer': 'one', 'format': 'geojson', 'path': str(out)})], [data])
    check(code == 0, text)
    f = json.loads(out.read_text())['features']
    check(len(f) == 2, len(f))
    check_cover(polygon(rect(lon, lat, 200, 100)), [x['geometry'] for x in f], 0)


def test_divide_wrong_selection():
    lon, lat = PL
    lines = write_layer('lines', [feature(linestring(lon, lat, [(0, 0), (100, 0)]), name='l')])
    with Session(shapes_layer(), lines) as s:
        s.activate('shapes')
        before = s.features('shapes')
        for c in ('divide-equal', 'divide-percent'):
            e = s.error_of(c)
            check('Select the polygons to divide' in e and 'nothing is selected' in e, e)
        s.activate('lines')
        s.select('lines', [0])
        e = s.error_of('divide-equal')
        check('Select the polygons to divide' in e and 'line' in e, e)
        check(same_features(s.features('shapes'), before), 'a failed command changed the data')


# ---------- divide-percent ----------

def test_divide_percent():
    with Session(shapes_layer()) as s:
        s.activate('shapes')
        orig = s.features('shapes')
        s.select('shapes', [2])
        r = s.plugin('divide-percent', {'percent': 30, 'angle': 90})
        after = s.features('shapes')
        check(len(after) == 6, len(after))
        rest, cut = after[2], after[5]
        check(rest['id'] == orig[2]['id'] and cut['properties'] == orig[2]['properties'], (rest, cut['properties']))
        total = area_ell(orig[2]['geometry'])
        check(abs(area_ell(cut['geometry']) - 0.3 * total) <= tol(total), (area_ell(cut['geometry']), total))
        check(abs(area_ell(rest['geometry']) - 0.7 * total) <= tol(total), area_ell(rest['geometry']))
        check(abs(r['areas'][0][0] - 0.3 * total) <= tol(total), r)
        # 90°: east-west lines, the cut-off share is the northern one.
        check(max(c[1] for c in coords(cut['geometry'])) > max(c[1] for c in coords(rest['geometry'])), 'north side')
        check_cover(orig[2]['geometry'], [cut['geometry'], rest['geometry']], 90)
        s.undo()
        check(same_features(s.features('shapes'), orig), 'undo did not restore the layer')


def test_divide_percent_reverse():
    lon, lat = PL
    g = polygon(rect(lon, lat, 100, 300))
    with Session(write_layer('one', [feature(g, name='a')])) as s:
        s.activate('one')
        s.select('one', [0])
        s.plugin('divide-percent', {'percent': 20, 'angle': 90, 'reverse': True})
        rest, cut = s.features('one')
        check(abs(area_ell(cut['geometry']) - 0.2 * area_ell(g)) <= tol(area_ell(g)), 'share')
        check(min(c[1] for c in coords(cut['geometry'])) < min(c[1] for c in coords(rest['geometry'])), 'south side')


def test_divide_percent_many():
    with Session(shapes_layer()) as s:
        s.activate('shapes')
        orig = s.features('shapes')
        s.select('shapes', [0, 1, 3, 4])
        r = s.plugin('divide-percent', {'percent': 12.5, 'angle': 170})
        check(r['divided'] == 4 and r['created'] == 4, r)
        after = s.features('shapes')
        check(sorted(s.selected_ids('shapes')) == sorted(f['id'] for f in after if f['id'] not in (orig[2]['id'],)),
              'selection: the parts')
        for k, row in enumerate([0, 1, 3, 4]):
            total = area_ell(orig[row]['geometry'])
            cut = after[5 + k]
            check(abs(area_ell(cut['geometry']) - total / 8) <= tol(total), (row, area_ell(cut['geometry']), total))
            check_cover(orig[row]['geometry'], [cut['geometry'], after[row]['geometry']], 170)


# ---------- what one command can divide ----------

def wavy(cx, n, r=100.0):
    """A closed wavy ring of n + 1 vertices around (cx, 0) metres from PL."""
    lon, lat = PL
    ring = []
    for i in range(n):
        t = i * math.tau / n
        rr = r * (1 + 0.02 * math.sin(7 * t))
        ring.append(offset_m(lon, lat, cx + rr * math.cos(t), rr * math.sin(t)))
    return ring + ring[:1]


def wavy_layer(name, count, n):
    return write_layer(name, [feature(polygon(wavy(300 * i, n)), n=i) for i in range(count)])


def test_division_budget_on_both_sides():
    # division's own estimate: 235 parcels of 21 vertices into 2 parts fit, 236 do not; into
    # 4 parts 160 and 161; one parcel of 8051 and 8052 vertices into 2 parts
    for count, n, parts, fits in [(236, 20, 2, False), (235, 20, 2, True), (161, 20, 4, False), (160, 20, 4, True),
                                  (1, 8051, 2, False), (1, 8050, 2, True)]:
        with Session(wavy_layer('parcels', count, n)) as s:
            s.activate('parcels')
            s.select('parcels', range(count))
            if fits:
                r = s.plugin('divide-equal', {'parts': parts})
                check(r['divided'] == count and r['created'] == count * (parts - 1), r)
                continue
            before = s.features('parcels')
            e = s.error_of('divide-equal', {'parts': parts})
            check('Dividing %d polygon(s) with %s vertices into %d parts is more than one command can do. '
                  'Select fewer polygons, or use the Divide Parcel tool' % (count, '{:,}'.format(count * (n + 1)).replace(',', ' '), parts)
                  in e, e)
            if parts == 2:
                e = s.error_of('divide-percent', {'percent': 30})
                check('into 2 parts is more than one command can do' in e, e)
            check(s.features('parcels') == before, 'a refused division changed the data')


def test_division_budget_allows_typical_use():
    with Session(wavy_layer('parcels', 50, 19)) as s:
        s.activate('parcels')
        s.select('parcels', range(50))
        r = s.plugin('divide-equal', {'parts': 4})
        check(r['divided'] == 50 and r['created'] == 150, r)
    with Session(wavy_layer('parcels', 1, 2999)) as s:
        s.activate('parcels')
        s.select('parcels', [0])
        r = s.plugin('divide-equal', {'parts': 2, 'angle': 30})
        check(r['divided'] == 1 and r['created'] == 1, r)
        r = s.plugin('divide-percent', {'percent': 25})
        check(r['divided'] == 2, r)


# ---------- divide-parcel (tool) ----------

def tool(s, **params):
    params.setdefault('input', 'shapes')
    return s.run('gp.run', {'tool': TOOL, 'params': params})


def test_tool_equal_parts_leaves_input():
    with Session(shapes_layer()) as s:
        before = s.features('shapes')
        r = tool(s, mode='Equal parts', count=4, angle=60)
        check(r['layersAdded'] == ['shapes divided'], r)
        out = s.features('shapes divided')
        check(len(out) == 20, len(out))
        check(same_features(s.features('shapes'), before), 'the tool changed its input')
        for k, src in enumerate(before):
            mine = [f for f in out if f['properties']['src_id'] == src['id']]
            check([f['properties']['part'] for f in mine] == [1, 2, 3, 4], mine)
            check(all(f['properties']['name'] == src['properties']['name'] for f in mine), 'attributes')
            total = area_ell(src['geometry'])
            check_parts_equal([f['geometry'] for f in mine], total, 4)
            for f in mine:
                check(abs(f['properties']['area_m2'] - total / 4) <= tol(total), f['properties'])
            check_cover(src['geometry'], [f['geometry'] for f in mine], 60)


def test_tool_parts_of_given_area():
    lon, lat = PL
    g = polygon(rect(lon, lat, 300, 100))
    with Session(write_layer('shapes', [feature(g, name='r')])) as s:
        tool(s, mode='Parts of given area', area=7000, angle=0)
        out = s.features('shapes divided')
        total = area_ell(g)
        check(len(out) == 5, [f['properties'] for f in out])
        for f in out[:4]:
            check(abs(area_ell(f['geometry']) - 7000) <= tol(7000), f['properties'])
        check(abs(area_ell(out[4]['geometry']) - (total - 28000)) <= tol(total), out[4]['properties'])
        check_cover(g, [f['geometry'] for f in out], 0)


def test_tool_area_that_divides_exactly_leaves_no_sliver():
    lon, lat = PL
    g = polygon(rect(lon, lat, 300, 100))
    total = area_ell(g)
    with Session(write_layer('shapes', [feature(g, name='r')])) as s:
        tool(s, mode='Parts of given area', area=round(total / 3, 4) + 0.001, angle=0)
        out = s.features('shapes divided')
        check(len(out) == 3, [f['properties'] for f in out])


def test_tool_strips_of_given_width():
    lon, lat = PL
    g = polygon(rect(lon, lat, 300, 100))
    with Session(write_layer('shapes', [feature(g, name='r')])) as s:
        tool(s, mode='Strips of given width', width=120, angle=0)
        out = s.features('shapes divided')
        check(len(out) == 3, [f['properties'] for f in out])
        total = area_ell(g)
        # Ground widths 120, 120 and the rest of the ~300 m (aelib's rectangle is approximate).
        w = [area_ell(f['geometry']) / total * 300 for f in out]
        check(abs(w[0] - 120) < 0.5 and abs(w[1] - 120) < 0.5 and abs(w[2] - 60) < 1, w)
        check(abs(w[0] - w[1]) < 1e-3, w)
        check_cover(g, [f['geometry'] for f in out], 0)


def test_tool_reverse():
    lon, lat = PL
    g = polygon(rect(lon, lat, 300, 100))
    with Session(write_layer('shapes', [feature(g, name='r')])) as s:
        tool(s, mode='Strips of given width', width=120, angle=0, reverse=True)
        out = s.features('shapes divided')
        cx = [sum(c[0] for c in coords(f['geometry'])) / len(coords(f['geometry'])) for f in out]
        check([f['properties']['part'] for f in out] == [1, 2, 3] and cx[0] > cx[1] > cx[2], cx)
        check_cover(g, [f['geometry'] for f in out], 0, reverse=True)


def test_tool_planar_mode():
    lon, lat = PL
    g = polygon(rect(lon, lat, 200, 100))
    with Session(write_layer('shapes', [feature(g, name='r')])) as s:
        tool(s, mode='Equal parts', count=2, areaMode='Planar')
        planar = [f['properties']['area_m2'] for f in s.features('shapes divided')]
    total = area_ell(g)
    # k = 0.9993 on the central meridian: planar = ellipsoidal * k².
    for a in planar:
        check(abs(a - total / 2 * 0.9993 ** 2) < 0.02, (a, total / 2 * 0.9993 ** 2))


def test_tool_selection_and_errors():
    with Session(shapes_layer()) as s:
        s.select('shapes', [1])
        tool(s, count=3)
        out = s.features('shapes divided')
        check(len(out) == 3 and {f['properties']['name'] for f in out} == {'L'}, 'only the selected parcel')
        for bad, msg in [({'count': 1}, 'Count'), ({'mode': 'Parts of given area', 'area': -5}, 'Area'),
                         ({'mode': 'Strips of given width', 'width': 0}, 'Width'), ({'areaMode': 'cubic'}, 'area mode')]:
            try:
                tool(s, **bad)
                raise AssertionError('tool accepted %s' % bad)
            except CommandError as e:
                check(msg in str(e), str(e))
        try:
            tool(s, mode='Parts of given area', area=1e9)
            raise AssertionError('tool accepted an area larger than the parcel')
        except CommandError as e:
            check('not larger than one part' in str(e), str(e))


def test_tool_big_parcel_many_parts():
    """A 5000-vertex parcel into 200 parts: the background budget is enough."""
    lon, lat = PL
    ring = [offset_m(lon, lat, 400 * math.cos(2 * math.pi * i / 5000) * (1 + 0.1 * math.sin(14 * math.pi * i / 5000)),
                     300 * math.sin(2 * math.pi * i / 5000)) for i in range(5000)]
    ring.append(ring[0])
    g = polygon(ring)
    with Session(write_layer('shapes', [feature(g, name='big')])) as s:
        tool(s, count=200, angle=20)
        out = s.features('shapes divided')
        check(len(out) == 200, len(out))
        total = area_ell(g)
        for f in out[::37]:
            check(abs(area_ell(f['geometry']) - total / 200) <= tol(total / 200), f['properties'])
        check(abs(sum(f['properties']['area_m2'] for f in out) - total) <= tol(total), 'sum')


def test_tool_budget():
    # 60 commands' budget, but 2000 parts of a 5000-vertex parcel are still too many
    lon, lat = PL
    with Session(write_layer('shapes', [feature(polygon(wavy(0, 5000)), name='big')])) as s:
        try:
            tool(s, count=2000)
            raise AssertionError('the tool accepted 2000 parts of 5001 vertices')
        except CommandError as e:
            check('Dividing 1 polygon(s) with 5 001 vertices into about 2 000 parts is more than one run of the '
                  'tool can do' in str(e), str(e))
        check('shapes divided' not in s.layers(), 'a refused tool added a layer')
        # parts of a given area are counted from the parcel's area before cutting
        try:
            tool(s, mode='Parts of given area', area=15)
            raise AssertionError('the tool accepted parts of 15 m²')
        except CommandError as e:
            check('more than one run of the tool can do' in str(e), str(e))


# ---------- real data ----------

def real_layer(s):
    path = os.environ.get('AE_REAL_DATA')
    if not path:
        raise Skip('AE_REAL_DATA is not set')
    layers = [l for l in s.layers().values() if l['dim'] == 'polygon']
    if not layers:
        raise Skip('no polygon layer in %s' % path)
    return layers[0]['name']


def real_session():
    path = os.environ.get('AE_REAL_DATA')
    if not path:
        raise Skip('AE_REAL_DATA is not set')
    return Session(path)


def test_real_divide_parcel_keeps_neighbours():
    with real_session() as s:
        name = real_layer(s)
        feats = s.features(name)
        # The largest parcel with the most vertices among the bigger ones.
        big = sorted(range(len(feats)), key=lambda i: -area_ell(feats[i]['geometry']))[:10]
        row = max(big, key=lambda i: len(coords(feats[i]['geometry'])))
        src = feats[row]
        s.activate(name)
        s.select(name, [row])
        r = s.plugin('divide-equal', {'parts': 3, 'angle': 37})
        after = s.features(name)
        ids = s.selected_ids(name)
        pieces = [f for f in after if f['id'] in ids]
        check(len(pieces) == 3, r)
        total = area_ell(src['geometry'])
        check_parts_equal([p['geometry'] for p in pieces], total, 3)
        check(abs(sum(area_ell(p['geometry']) for p in pieces) - total) <= tol(total), 'sum')
        # Other features change only by the shared vertices topological editing adds, and the
        # parcel's own vertices keep their positions.
        from test_topo import neighbours_only_gained
        by_id = {f['id']: f for f in after}
        check(all(f['id'] in by_id for f in feats), 'a feature is gone')
        neighbours_only_gained(feats, [by_id[f['id']] for f in feats], {row}, r, [p['geometry'] for p in pieces])
        kept = {(round(c[0], 11), round(c[1], 11)) for p in pieces for c in coords(p['geometry'])}
        lost = [c for c in coords(src['geometry']) if (round(c[0], 11), round(c[1], 11)) not in kept]
        check(not lost, 'original vertices moved: %s' % lost[:3])
        print('      real parcel %s: %.2f m², parts %s m² (%d vertices)'
              % (src['properties'], total, r['areas'][0], len(coords(src['geometry']))))


def test_real_ellipsoidal_vs_planar():
    with real_session() as s:
        name = real_layer(s)
        s.run('gp.run', {'tool': TOOL, 'params': {'input': name, 'mode': 'Equal parts', 'count': 2}})
        ell = s.features(name + ' divided')
    with real_session() as s:
        s.run('gp.run', {'tool': TOOL, 'params': {'input': name, 'mode': 'Equal parts', 'count': 2, 'areaMode': 'Planar'}})
        pla = s.features(name + ' divided')
    check(len(ell) == len(pla), (len(ell), len(pla)))
    big = [(e['properties']['area_m2'], p['properties']['area_m2']) for e, p in zip(ell, pla)
           if e['properties']['area_m2'] > 100]
    rel = [p / e - 1 for e, p in big]
    diff = [p - e for e, p in big]
    print('      planar EPSG:2180 vs ellipsoidal: %+.4f %% to %+.4f %% (%d parts > 100 m²), up to %.2f m² per part'
          % (100 * min(rel), 100 * max(rel), len(big), max(diff, key=abs)))
    sample = ell[::25]
    for f in sample:
        a = area_ell(f['geometry'])
        check(abs(f['properties']['area_m2'] - a) <= tol(a), (f['properties'], a))
