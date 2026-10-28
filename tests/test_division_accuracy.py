"""Accuracy of ellipsoidal division on large parcels far from the central meridian, where
the projection's area scale changes most across a parcel.

Squares of 1 km to the largest accepted size are built in the plugin's own frame (EPSG:2180
in Poland, UTM elsewhere) and divided; each part is then measured independently with
GeographicLib's geodesic area (pyproj's Geod, in `AE_SHAPELY_PYTHON` or the running Python),
its edges densified as straight lines of that frame. Without pyproj, the parts are measured
on the cylindrical equal-area projection of the ellipsoid in pure Python instead. Every part
must be within max(0.01 m², 1e-6·A) of its share, its reported area must agree with the
independent one, and the reported largest error must not hide a larger deviation."""

import json
import math
import os
import subprocess
import sys

from aelib import PREFIX, CommandError, Session, check, feature, polygon, write_layer
from test_division import TM, ring_area_ell, tol

TOOL = PREFIX + 'divide-parcel'

POLAND = (14.0, 49.0, 24.2, 54.9)
WGS84_F = 1 / 298.257223563

# The largest parcel (bounding box side in the frame) divided by ellipsoidal area.
MAX_EXTENT_KM = 1000


# ---------- frames ----------

class Frame:
    """EPSG:2180 or a WGS84 UTM zone (north), as the plugin chooses them."""

    def __init__(self, zone=None):
        self.zone = zone
        if zone is None:
            self.tm, self.fe, self.fn, self.ellps = TM(19.0, 0.9993), 500000.0, -5300000.0, 'GRS80'
            self.proj = '+proj=tmerc +lon_0=19 +k=0.9993 +x_0=500000 +y_0=-5300000 +ellps=GRS80'
        else:
            lon0 = zone * 6 - 183
            self.tm, self.fe, self.fn, self.ellps = TM(lon0, 0.9996, f=WGS84_F), 500000.0, 0.0, 'WGS84'
            self.proj = '+proj=tmerc +lon_0=%d +k=0.9996 +x_0=500000 +y_0=0 +ellps=WGS84' % lon0

    def fwd(self, lon, lat):
        x, y = self.tm.fwd(lon, lat)
        return x + self.fe, y + self.fn

    def inv(self, x, y):
        return self.tm.inv(x - self.fe, y - self.fn)

    def name(self):
        return 'EPSG:2180' if self.zone is None else 'UTM %dN' % self.zone


def plugin_frame(ring):
    """The frame the plugin works in for a ring of lon/lat positions."""
    if all(POLAND[0] <= x <= POLAND[2] and POLAND[1] <= y <= POLAND[3] for x, y in ring):
        return Frame()
    lons, lats = [p[0] for p in ring], [p[1] for p in ring]
    return Frame(int(math.floor((0.5 * (min(lons) + max(lons)) + 180) / 6)) % 60 + 1)


def square(frame, lon, lat, side_m):
    """A square of `side_m` centred at (lon, lat), with straight edges in `frame`."""
    cx, cy = frame.fwd(lon, lat)
    h = side_m / 2
    pts = [(-h, -h), (h, -h), (h, h), (-h, h), (-h, -h)]
    return [list(frame.inv(cx + x, cy + y)) for x, y in pts]


# ---------- independent areas ----------

GEOD_SCRIPT = r'''
import json, math, sys
from pyproj import Geod, Transformer
out = []
for job in json.load(sys.stdin):
    fwd = Transformer.from_crs('EPSG:4326', job['proj'], always_xy=True)
    inv = Transformer.from_crs(job['proj'], 'EPSG:4326', always_xy=True)
    geod = Geod(ellps=job['ellps'])
    total = 0.0
    for k, ring in enumerate(job['rings']):
        xy = [fwd.transform(*c) for c in ring]
        per = sum(math.hypot(b[0] - a[0], b[1] - a[1]) for a, b in zip(xy, xy[1:]))
        step = max(1.0, per / 20000)
        lons, lats = [], []
        for a, b in zip(xy, xy[1:]):
            n = max(1, math.ceil(math.hypot(b[0] - a[0], b[1] - a[1]) / step))
            for i in range(n):
                lon, lat = inv.transform(a[0] + (b[0] - a[0]) * i / n, a[1] + (b[1] - a[1]) * i / n)
                lons.append(lon)
                lats.append(lat)
        a = abs(geod.polygon_area_perimeter(lons, lats)[0])
        total += a if k == 0 else -a
    out.append(total)
print(json.dumps(out))
'''

_PY = {}


def geod_python():
    """A Python with pyproj (AE_SHAPELY_PYTHON first), or None."""
    if 'py' not in _PY:
        _PY['py'] = None
        for py in [os.environ.get('AE_SHAPELY_PYTHON'), sys.executable]:
            if py and subprocess.run([py, '-I', '-c', 'import pyproj'], capture_output=True).returncode == 0:
                _PY['py'] = py
                break
    return _PY['py']


def rings_of(geom):
    polys = [geom['coordinates']] if geom['type'] == 'Polygon' else geom['coordinates']
    return [(k, r) for p in polys for k, r in enumerate(p)]


def true_areas(geoms, frame, size_m):
    """Ellipsoidal areas of `geoms` whose edges are straight lines of `frame`."""
    py = geod_python()
    if py:
        jobs = [{'proj': frame.proj, 'ellps': frame.ellps, 'rings': [r for _, r in rings_of(g)]} for g in geoms]
        r = subprocess.run([py, '-I', '-c', GEOD_SCRIPT], input=json.dumps(jobs), capture_output=True,
                           text=True, timeout=600)
        if r.returncode != 0:
            raise RuntimeError('pyproj helper failed: ' + r.stderr[-2000:])
        return json.loads(r.stdout)
    # densified chords on the equal-area projection: the error shrinks with the step squared
    step = max(2.0, size_m / 4000)
    return [sum((1 if k == 0 else -1) * ring_area_ell(r, frame.tm, step) for k, r in rings_of(g)) for g in geoms]


# ---------- checks ----------

def check_division(name, geoms, reported, max_error, frame, size_m, shares):
    """Each part within the tolerance of its share, measured independently; the reported
    areas agree with that measure and the reported largest error is not understated."""
    true = true_areas(geoms, frame, size_m)
    total = sum(true)
    dev = diff = 0.0
    for i, (a, rep, sh) in enumerate(zip(true, reported, shares)):
        want = total * sh
        dev = max(dev, abs(a - want))
        diff = max(diff, abs(rep - a))
        check(abs(a - want) <= tol(want), '%s part %d: %.4f m², its share is %.4f (tolerance %.4f)'
              % (name, i, a, want, tol(want)))
        check(abs(rep - a) <= 0.01 * tol(a), '%s part %d: reported %.4f m², independently %.4f'
              % (name, i, rep, a))
    check(dev <= max_error + 0.01 * tol(total * min(shares)),
          '%s: largest deviation %.4f m², reported %.4f' % (name, dev, max_error))
    print('      %s in %s: parts of %.4g m², largest deviation %.3g m² (%.1e), reported %.3g m²; '
          'reported areas within %.2g m²' % (name, frame.name(), total * min(shares), dev,
                                              dev / (total * min(shares)), max_error, diff))


# Squares centred far from the central meridian of their frame: EPSG:2180 near Poland's
# eastern border (19°E + 4.3°), UTM 34N 2.5° and 2° east of 21°E.
SQUARES = [
    ('pl', Frame(), 23.3, 52.2, [1000, 10000, 50000]),
    ('pl', Frame(), 21.8, 51.9, [300000]),
    ('pl', Frame(), 19.1, 51.95, [600000]),
    ('utm', Frame(34), 23.5, 45.0, [1000, 10000, 50000]),
    ('utm', Frame(34), 23.0, 45.0, [999000]),
    ('utm', Frame(34), 23.0, 0.5, [999000]),
]


def test_divide_equal_large_squares():
    for tag, frame, lon, lat, sizes in SQUARES:
        feats = []
        for s in sizes:
            ring = square(frame, lon, lat, s)
            check(plugin_frame(ring).name() == frame.name(), (tag, s, plugin_frame(ring).name()))
            feats.append(feature(polygon(ring), name='%s%d' % (tag, s)))
        with Session(write_layer('sq', feats)) as ses:
            ses.activate('sq')
            for row, s in enumerate(sizes):
                ses.select('sq', [row])
                r = ses.plugin('divide-equal', {'parts': 4, 'angle': 0})
                check(r['divided'] == 1 and r['created'] == 3, r)
                ids = set(ses.selected_ids('sq'))
                parts = [f['geometry'] for f in ses.features('sq') if f['id'] in ids]
                check(len(parts) == 4, r)
                check_division('%s %g km' % (tag, s / 1000), parts, r['areas'][0], r['maxError'], frame, s, [0.25] * 4)


def test_divide_percent_and_angle_utm():
    frame = Frame(34)
    ring = square(frame, 23.5, 45.0, 50000)
    with Session(write_layer('sq', [feature(polygon(ring), name='a')])) as s:
        s.activate('sq')
        s.select('sq', [0])
        r = s.plugin('divide-percent', {'percent': 30, 'angle': 30})
        parts = [f['geometry'] for f in s.features('sq')]
        # the original keeps the remainder (the last part), the cut-off share is the new feature
        check_division('cut off 30 %', [parts[1], parts[0]], r['areas'][0], r['maxError'], frame, 50000, [0.3, 0.7])


def test_tool_large_squares():
    frame = Frame(34)
    feats = [feature(polygon(square(frame, 23.0, 45.0, s)), name='s%d' % s) for s in (50000, 999000)]
    with Session(write_layer('sq', feats)) as s:
        r = s.run('gp.run', {'tool': TOOL, 'params': {'input': 'sq', 'mode': 'Equal parts', 'count': 3, 'angle': 60}})
        summary = r['notices'][0]
        err = float(summary.split('largest error ')[1].split(' m')[0])
        out = s.features('sq divided')
        for size in (50000, 999000):
            parts = [f for f in out if f['properties']['name'] == 's%d' % size]
            check(len(parts) == 3, out)
            check_division('tool %g km' % (size / 1000), [f['geometry'] for f in parts],
                           [f['properties']['area_m2'] for f in parts], err, frame, size, [1 / 3] * 3)


def test_refuses_above_the_extent():
    frame = Frame(34)
    ring = square(frame, 23.0, 45.0, 1010000)
    check(plugin_frame(ring).name() == 'UTM 34N', plugin_frame(ring).name())
    with Session(write_layer('sq', [feature(polygon(ring), name='big')])) as s:
        s.activate('sq')
        s.select('sq', [0])
        before = s.features('sq')
        for cmd, params in [('divide-equal', {'parts': 2}), ('divide-percent', {'percent': 50})]:
            e = s.error_of(cmd, params)
            check('the parcel is 1010 km across in UTM 34N' in e and 'up to %d km' % MAX_EXTENT_KM in e
                  and 'areaMode "Planar"' in e, e)
        check(s.features('sq') == before, 'a refused division changed the data')
        try:
            s.run('gp.run', {'tool': TOOL, 'params': {'input': 'sq', 'mode': 'Equal parts', 'count': 2}})
            raise AssertionError('the tool divided a 1010 km parcel by ellipsoidal area')
        except CommandError as e:
            check('the parcel is 1010 km across' in str(e) and 'areaMode "Planar"' in str(e), str(e))
        # planar area has no such limit
        r = s.run('gp.run', {'tool': TOOL, 'params': {'input': 'sq', 'mode': 'Equal parts', 'count': 2,
                                                      'areaMode': 'Planar'}})
        check('planar area' in r['notices'][0], r)
        check(len(s.features('sq divided')) == 2, r)
