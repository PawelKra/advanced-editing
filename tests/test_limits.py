"""What one command can afford (host_io::budget) and the fast shared paths it relies on:
the projection, VertexSnap, and reading/writing GeoJSON text without a Value tree."""

import math
from decimal import Decimal, getcontext

from aelib import (ABROAD, PL, Session, check, feature, linestring, offset_m, point, polygon, rect,
                   write_layer)


def blob(cx, cy, r, n, origin=PL):
    """A closed wavy ring of n vertices around (cx, cy) metres from `origin`."""
    lon, lat = origin
    ring = []
    for i in range(n):
        t = i * math.tau / n
        rr = r * (1 + 0.02 * math.sin(7 * t))
        ring.append(offset_m(lon, lat, cx + rr * math.cos(t), cy + rr * math.sin(t)))
    ring.append(ring[0])
    return ring


def blob_layer(name, k, n):
    return write_layer(name, [feature(polygon(blob(300 * i, 0, 100, n)), n=i) for i in range(k)])


def cutter_layer(k):
    lon, lat = PL
    return write_layer('cutter', [feature(polygon(rect(lon, lat, 300 * k, 60, -150, -30)), name='cut')])


def test_edit_guard_refuses_just_over_and_changes_nothing():
    # 5 x 4201 = 21 005 vertices: just over the ~20 800 a command edits
    with Session(blob_layer('blobs', 5, 4200), cutter_layer(5)) as s:
        s.activate('blobs')
        s.select('blobs', range(5))
        before = s.features('blobs')
        e = s.error_of('simplify', {'tolerance': 0.5})
        check('This would edit 5 features with 21 005 vertices; one command handles about 20 000 vertices. '
              'Select fewer features' in e, e)
        # tool commands count the tools too
        s.activate('cutter')
        s.select('cutter', [0], add=True)
        e = s.error_of('cut-with-polygon', {'keepTools': True})
        check('This would edit 5 features with 21 010 vertices' in e, e)
        check(s.features('blobs') == before, 'a refused command changed the data')


def test_big_selection_within_the_guard_succeeds():
    # 4 x 5001 vertices: read, projected, cut and written in one command
    with Session(blob_layer('blobs', 4, 5000), cutter_layer(4)) as s:
        s.select('blobs', range(4))
        s.activate('cutter')
        s.select('cutter', [0], add=True)
        r = s.plugin('cut-with-polygon', {'keepTools': True})
        check('Cut 4' in r['message'], r)
        check(all(f['geometry']['type'] == 'MultiPolygon' for f in s.features('blobs')))
        s.activate('blobs')
        s.select('blobs', range(4))
        r = s.plugin('smooth', {'iterations': 1})
        check('Smoothed 4' in r['message'], r)


def test_read_guard_refuses_a_large_reply():
    # 35 x 1001 vertices ~ 1.4 MB of GeoJSON: more than a command parses
    with Session(blob_layer('blobs', 35, 1000)) as s:
        s.activate('blobs')
        s.select('blobs', [0])
        e = s.error_of('diag-selection', {'mode': 'edit', 'dims': ['polygon'], 'whole': True})
        check("Reading the 35 features of 'blobs' means 1.4 MB of data; one command reads about 1.2 MB. "
              'Select fewer features.' in e, e)
        # the selection alone is fine
        r = s.plugin('diag-selection', {'mode': 'edit', 'dims': ['polygon']})
        check(r['ids'] == [1], r)


def test_read_guard_refuses_before_copying_the_reply():
    # the reply's length alone refuses it: the module's memory doesn't grow by the 1.4 MB, and
    # the next host call gets its own reply
    with Session(blob_layer('blobs', 35, 1000)) as s:
        lid = s.layer_id('blobs')
        first = s.features('blobs')[0]['id']
        r = s.plugin('diag-features', {'layer': lid, 'readLeft': 1_200_000})
        check(r['error'] == 'Reading the test features means 1.4 MB of data; one command reads about 1.2 MB. '
                            'Select fewer features.', r)
        check(r['count'] is None and r['grownBytes'] < 500_000, r)
        check(r['nextIds'] == [first], r)
        # a reply within what is left is read and counted
        r = s.plugin('diag-features', {'layer': lid, 'readLeft': 1_200_000, 'limit': 5})
        check(r['error'] is None and r['count'] == 5 and r['nextIds'] == [first], r)
        r = s.plugin('diag-features', {'layer': lid, 'readLeft': 1000, 'limit': 5})
        check('means 0.2 MB of data on top of the 1.2 MB already read;' in r['error'], r)
        check(r['grownBytes'] < 100_000 and r['nextIds'] == [first], r)


def fat_layer(name, k, x0, kb=45):
    """k small squares carrying `kb` kB of text each: a large reply with few vertices."""
    lon, lat = PL
    return write_layer(name, [feature(polygon(rect(lon, lat, 50, 50, x0 + 100 * i, 0)), note='x' * (kb * 1000), n=i)
                              for i in range(k)])


def test_read_guard_counts_every_layer_a_command_reads():
    # three target layers of about 0.45 MB each: each fits the read guard, together they don't
    lon, lat = PL
    cutter = write_layer('cutter', [feature(polygon(rect(lon, lat, 3000, 30, -50, 10)), name='cut')])
    with Session(fat_layer('f1', 10, 0), fat_layer('f2', 10, 1000), fat_layer('f3', 10, 2000), cutter) as s:
        for name in ['f1', 'f2', 'f3']:
            s.select(name, range(10), add=True)
        s.activate('cutter')
        s.select('cutter', [0], add=True)
        before = [s.features(n) for n in ['f1', 'f2', 'f3']]
        e = s.error_of('cut-with-polygon', {'keepTools': True})
        check('Reading the 10 selected features of ' in e and 'MB already read; one command reads about 1.2 MB. '
              'Select fewer features.' in e, e)
        check([s.features(n) for n in ['f1', 'f2', 'f3']] == before, 'a refused command changed the data')
        # two of them fit
        s.clear_selection()
        for name in ['f1', 'f2']:
            s.select(name, range(10), add=True)
        s.activate('cutter')
        s.select('cutter', [0], add=True)
        r = s.plugin('cut-with-polygon', {'keepTools': True})
        check('Cut 20' in r['message'], r)
    # locators of Delete Ring count on top of the polygons the command edits
    holed = write_layer('holed', [feature(polygon(rect(lon, lat, 100, 100, -500, 0),
                                                  list(reversed(rect(lon, lat, 40, 40, -470, 30)))),
                                          note='x' * 300_000)])
    with Session(holed, fat_layer('f1', 10, 0), fat_layer('f2', 10, 1000)) as s:
        s.activate('holed')
        s.select('holed', [0])
        for name in ['f1', 'f2']:
            s.select(name, range(10), add=True)
        e = s.error_of('delete-ring')
        check('Reading the 10 selected features of ' in e and 'MB already read; one command reads about 1.2 MB' in e, e)


def test_fill_ring_island_search_counts_on_top_of_the_command_reads():
    # the holed polygon (0.5 MB) and its locators (0.45 MB) are read; the layer (0.6 MB) fits
    # on top of the polygon, not on top of both, so the island search is skipped with its note
    lon, lat = PL
    holed = feature(polygon(rect(lon, lat, 400, 400, -150, -150), list(reversed(rect(lon, lat, 300, 300, -100, -100)))),
                    note='x' * 500_000)
    island = feature(polygon(rect(lon, lat, 20, 20, 60, 60)), note='y' * 100_000)
    with Session(write_layer('holed', [holed, island]), fat_layer('f1', 10, 0)) as s:
        s.select('f1', range(10))
        s.activate('holed')
        s.select('holed', [0], add=True)
        r = s.plugin('fill-ring')
        check(r['created'] == 1 and r['islands'] == 0 and not r['layerChecked'], r)
        check('the layer is too large to check its unselected polygons for islands' in r['message'], r['message'])
        s.undo()
        # without the locators the layer fits and the island is left out
        s.clear_selection()
        s.activate('holed')
        s.select('holed', [0])
        r = s.plugin('fill-ring')
        check(r['created'] == 1 and r['islands'] == 1 and r['layerChecked'], r)


def test_trim_extend_layer_search_counts_on_top_of_the_tool():
    # the tool line (0.6 MB) is read first: the layer (0.85 MB) fits the read guard alone,
    # not on top of it
    lon, lat = PL
    tool = feature(linestring(lon, lat, [(0, -100), (0, 100)]), note='x' * 600_000)
    stubs = [feature(linestring(lon, lat, [(-50, 10 * i), (-5, 10 * i)]), note='y' * 50_000) for i in range(5)]
    with Session(write_layer('tl', [tool] + stubs)) as s:
        s.activate('tl')
        s.select('tl', [0])
        before = s.features('tl')
        e = s.error_of('trim-extend')
        check("Reading the 6 features of 'tl' means 0.9 MB of data on top of the 0.6 MB already read" in e, e)
        check(s.features('tl') == before, 'a refused command changed the data')


def test_topological_editing_counts_on_top_of_the_command_reads():
    # 10 of 20 fat squares (0.45 MB) are cut; their layer (0.9 MB) fits the read guard alone,
    # not on top of them, so their neighbours are not searched and the message says so
    lon, lat = PL
    cutter = write_layer('cutter', [feature(polygon(rect(lon, lat, 1000, 30, -50, 10)), name='cut')])
    with Session(fat_layer('f1', 20, 0), cutter) as s:
        s.select('f1', range(10))
        s.activate('cutter')
        s.select('cutter', [0], add=True)
        r = s.plugin('cut-with-polygon', {'keepTools': True})
        check('Cut 10' in r['message'], r)
        check("shared vertices not added in 'f1': the layer is too large to search" in r['message'], r['message'])


# ---------- projection ----------

getcontext().prec = 45
_PI = Decimal('3.14159265358979323846264338327950288419716939937510582097494459')
_TINY = Decimal('1e-55')


def _exp(x):
    k = 0
    while abs(x) > Decimal('0.5'):
        x /= 2
        k += 1
    s = t = Decimal(1)
    i = 1
    while abs(t) > _TINY:
        t = t * x / i
        i += 1
        s += t
    for _ in range(k):
        s *= s
    return s


def _ln(x):
    y = Decimal(repr(math.log(float(x))))
    for _ in range(4):
        e = _exp(y)
        y += 2 * (x - e) / (x + e)
    return y


def _sin(x):
    x = x % (2 * _PI)
    s = t = x
    i = 1
    while abs(t) > _TINY:
        t = -t * x * x / ((2 * i) * (2 * i + 1))
        i += 1
        s += t
    return s


def _cos(x):
    return _sin(x + _PI / 2)


def _atan(x):
    k = 0
    while abs(x) > Decimal('0.1'):
        x = x / (1 + (1 + x * x).sqrt())
        k += 1
    s = t = x
    i = 1
    while abs(t) > _TINY:
        t = -t * x * x
        s += t / (2 * i + 1)
        i += 1
    return s * 2 ** k


def tm_exact(inv_f, lon0, k0, fn, lon, lat):
    """The Krueger n^6 series with the exact conformal latitude, in 45-digit arithmetic."""
    f = 1 / Decimal(inv_f)
    n = f / (2 - f)
    alp = [n / 2 - 2 * n**2 / 3 + 5 * n**3 / 16 + 41 * n**4 / 180 - 127 * n**5 / 288 + 7891 * n**6 / 37800,
           13 * n**2 / 48 - 3 * n**3 / 5 + 557 * n**4 / 1440 + 281 * n**5 / 630 - 1983433 * n**6 / 1935360,
           61 * n**3 / 240 - 103 * n**4 / 140 + 15061 * n**5 / 26880 + 167603 * n**6 / 181440,
           49561 * n**4 / 161280 - 179 * n**5 / 168 + 6601661 * n**6 / 7257600,
           34729 * n**5 / 80640 - 3418889 * n**6 / 1995840,
           212378941 * n**6 / 319334400]
    a_rect = Decimal(6378137) / (1 + n) * (1 + n**2 / 4 + n**4 / 64 + n**6 / 256)
    e = (f * (2 - f)).sqrt()
    phi = Decimal(repr(lat)) * _PI / 180
    lam = (Decimal(repr(lon)) - lon0) * _PI / 180
    sp = _sin(phi)
    z = e * sp
    sig = _exp(e * _ln((1 + z) / (1 - z)) / 2)
    sig = (sig - 1 / sig) / 2
    tau = sp / _cos(phi)
    taup = tau * (1 + sig * sig).sqrt() - sig * (1 + tau * tau).sqrt()
    cl, sl = _cos(lam), _sin(lam)
    xip = _atan(taup / cl)
    q = sl / (taup * taup + cl * cl).sqrt()
    etap = _ln(q + (q * q + 1).sqrt())
    xi, eta = xip, etap
    for j, a in enumerate(alp):
        k = 2 * (j + 1)
        ek = _exp(k * etap)
        xi += a * _sin(k * xip) * (ek + 1 / ek) / 2
        eta += a * _cos(k * xip) * (ek - 1 / ek) / 2
    kk = Decimal(k0) * a_rect
    return float(500000 + kk * eta), float(Decimal(fn) + kk * xi)


def test_projection_accuracy():
    with Session() as s:
        # the fast projection against the term-by-term one it replaced, on dense grids
        for lon, lat in [(19, 52), (10.5, 45.5), (-70, -33), (150, 60), (20, 80), (3, 0.5)]:
            r = s.plugin('diag-proj', {'grid': True, 'n': 30, 'lon': lon, 'lat': lat})
            check(max(r['fwd_m'], r['inv_m'], r['rt_m']) < 1e-8, r)
        # and against the series evaluated exactly: within 3 nm (a few ulps of the northing)
        pts = [(14.1, 49.1), (24.1, 54.8), (21.0122, 52.2297), (16.123456789, 53.987654321),
               (10.5, 45.5), (8.9, 45.1), (-70.0, -33.0), (150.0, 60.0), (23.9, 79.0)]
        out = s.plugin('diag-proj', {'points': pts})['points']
        for (lon, lat), o in zip(pts, out):
            if o['frame'] == 'EPSG:2180':
                x, y = tm_exact('298.257222101', 19, '0.9993', -5300000, lon, lat)
            else:
                zone = int(o['frame'].split()[1][:-1])
                x, y = tm_exact('298.257223563', zone * 6 - 183, '0.9996',
                                10000000 if o['frame'].endswith('S') else 0, lon, lat)
            check(math.hypot(o['x'] - x, o['y'] - y) < 3e-9, (lon, lat, o, x, y))
            check(o['rt_err_deg'] < 1e-13, o)


def test_vertex_snap_regressions():
    lon, lat = PL
    with Session() as s:
        # 5001 vertices round-trip in one command, every one bit-exact
        r = s.plugin('diag-snap', {'geometry': polygon(blob(0, 0, 100, 5000))})
        check(r['n'] == 5001 and r['exact'] == 5001, r)
        # holes, several parts, shared vertices between parts, a UTM frame
        outer = rect(lon, lat, 100, 100)
        hole = list(reversed(rect(lon, lat, 40, 40, 30, 30)))
        touching = rect(lon, lat, 100, 100, 100, 0)
        mp = {'type': 'MultiPolygon', 'coordinates': [[outer, hole], [touching]]}
        r = s.plugin('diag-snap', {'geometry': mp})
        check(r['exact'] == r['n'] == 15, r)
        r = s.plugin('diag-snap', {'geometry': polygon(blob(0, 0, 50, 300, origin=ABROAD))})
        check(r['frame'] == 'UTM 32N' and r['exact'] == r['n'] == 301, r)
        # a vertex moved within the tolerance snaps back; beyond it, it stays moved
        for nudge, exact in [(9e-6, 301), (2e-5, 299)]:
            r = s.plugin('diag-snap', {'geometry': polygon(blob(0, 0, 50, 300)), 'nudge': nudge})
            check(r['exact'] == exact, (nudge, r))


def test_number_text_is_shortest_and_exact():
    with Session() as s:
        r = s.plugin('diag-geojson', {'numbers': True, 'count': 1000})
        check(r['n'] == 13000 and r['text_diff'] == 0 and r['parse_diff'] == 0, r)


def test_geojson_text_reader_and_writer():
    lon, lat = PL
    sq = rect(lon, lat, 100, 100)
    hole = list(reversed(rect(lon, lat, 40, 40, 30, 30)))
    polys = write_layer('polys', [
        feature(polygon(sq, hole), name='with "hole" é\\'),
        feature({'type': 'MultiPolygon', 'coordinates': [[blob(400, 0, 50, 97)], [rect(lon, lat, 10, 10, 600, 0)]]},
                name='multi', v=None, x=1.5e-7),
    ])
    lines = write_layer('lines', [
        feature(linestring(lon, lat, [(0, 0), (10.123, 5), (20, -3)]), name='l'),
        feature({'type': 'MultiLineString', 'coordinates': [linestring(lon, lat, [(0, 50), (9, 51)])['coordinates'],
                                                             linestring(lon, lat, [(0, 60), (9, 61)])['coordinates']]},
                name='ml'),
    ])
    pts = write_layer('pts', [
        feature(point(lon, lat, 1, 2), name='p'),
        feature({'type': 'MultiPoint', 'coordinates': [offset_m(lon, lat, 3, 4), offset_m(lon, lat, 5, 6)]}, name='mp'),
    ])
    with Session(polys, lines, pts) as s:
        for name in ['polys', 'lines', 'pts']:
            lid = s.layer_id(name)
            exported = s.features(name)
            r = s.plugin('diag-features', {'layer': lid, 'properties': True})
            check(r['geometries'] == [f['geometry'] for f in exported], name)
            check(r['props'] == [f['properties'] for f in exported], (r['props'], exported))
            # written back unchanged: the host stores the very same numbers
            s.plugin('diag-features', {'layer': lid, 'rewrite': True})
            check(s.features(name) == exported, 'rewriting %s changed it' % name)


# ---------- limits in the README ----------

def budget_constants():
    """The numeric constants of host_io::budget, evaluated (they are products and sums of
    literals and each other)."""
    import re
    from aelib import REPO
    src = (REPO / 'src' / 'host_io.rs').read_text()
    body = src[src.index('pub mod budget'):]
    env = {}
    for name, expr in re.findall(r'pub const (\w+): (?:u64|usize) = ([^;]+);', body):
        expr = re.sub(r'\bas \w+', '', expr).replace('/', '//')
        expr = re.sub(r'(?<=\d)_(?=\d)', '', expr)
        env[name] = eval(re.sub(r'\b[A-Z][A-Z0-9_]+\b', lambda m: str(env[m.group(0)]), expr))
    return env


def test_readme_limits_match_the_budget():
    import re
    from aelib import REPO
    b = budget_constants()
    readme = (REPO / 'README.md').read_text()
    limits = readme[readme.index('## Limits'):readme.index('## Permissions')]

    def about(n):
        m = 1
        while n // m >= 100:
            m *= 10
        return '{:,}'.format(n // m * m).replace(',', ' ')

    check(b['MAX_EDIT_VERTICES'] // 1000 == 20 and 'edits about %s vertices' % about(b['MAX_EDIT_VERTICES']) in limits,
          (b['MAX_EDIT_VERTICES'], limits))
    check(b['MAX_WRITE_VERTICES'] // 1000 == 41 and 'about 40 000 vertices they create' in limits, b['MAX_WRITE_VERTICES'])
    check('about %.1f MB' % (b['MAX_READ_BYTES'] / 1e6) in limits, b['MAX_READ_BYTES'])
    check('%d times' % (b['TOOL_FUEL'] // b['COMMAND_FUEL']) in limits, b['TOOL_FUEL'])
    # every vertex count the README states is one of these
    stated = {int(n.replace(' ', '')) for n in re.findall(r'(\d[\d ]*\d) vertices', limits)}
    check(stated <= {20_000, 30_000, 40_000, 3000, 8000, 20}, stated)
    # no module keeps a vertex cap of its own beside the budget (the quadratic validity check
    # of small outputs is not an edit limit)
    for f in sorted((REPO / 'src').glob('*.rs')):
        for name in re.findall(r'const (\w*VERTICES\w*): usize = \d', f.read_text()):
            check(name == 'MAX_VALIDATE_VERTICES', '%s: %s' % (f.name, name))


# ---------- polygon overlays ----------

def test_add_ring_with_many_tools_and_targets_is_refused_up_front():
    # 1000 small squares in the empty half of 1000 triangles' common box: each tool would be
    # compared with each target, a million boolean ops on 9000 vertices in all
    import time
    lon, lat = PL
    tri = [offset_m(lon, lat, x, y) for x, y in [(0, 0), (1000, 0), (0, 1000), (0, 0)]]
    targets = write_layer('targets', [feature(polygon(tri), n=i) for i in range(1000)])
    tools = write_layer('tools', [feature(polygon(rect(lon, lat, 5, 5, 600 + 11 * (i % 32), 600 + 11 * (i // 32))), n=i)
                                  for i in range(1000)])
    t = time.time()
    with Session(targets, tools) as s:
        s.select('targets', range(1000))
        s.activate('tools')
        s.select('tools', range(1000), add=True)
        before = s.features('targets'), s.features('tools')
        e = s.error_of('add-ring', {'keepTools': False})
        check('Trying these polygons as holes in the targets is more than one command can do' in e
              and 'ran too long' not in e, e)
        check((s.features('targets'), s.features('tools')) == before, 'a refused command changed the data')
    print('      1000 x 1000 refused in %.1fs' % (time.time() - t))


def test_merge_and_fill_gap_of_a_comb_are_refused_up_front():
    # a comb of 1500 teeth beside a square: the union compares every two of the teeth's edges,
    # which overlap along x; 750 teeth still fit
    import time
    from test_topo import poly, sq
    from test_topo_limits import comb
    for teeth, refused in ((750, False), (1500, True)):
        t = time.time()
        with Session(write_layer('pl', [feature(poly(sq(0, 0)), name='A'), feature(comb(teeth), name='comb')])) as s:
            s.activate('pl')
            s.select('pl', [0, 1])
            before = s.features('pl')
            if not refused:
                r = s.plugin('merge-with-attributes')
                check(r['merged'] == 2, r)
                continue
            e = s.error_of('fill-gap')
            check('Finding the gaps between these polygons is more than one command can do' in e
                  and 'ran too long' not in e, e)
            e = s.error_of('merge-with-attributes')
            check('Merging these polygons is more than one command can do' in e and 'ran too long' not in e, e)
            check(s.features('pl') == before, 'a refused command changed the data')
        print('      %d teeth %s in %.1fs' % (teeth, 'refused' if refused else 'merged', time.time() - t))


def test_simplify_of_stacked_holes_is_refused_up_front():
    # narrow holes stacked one above another, all over the same x-range: checking the result
    # for crossing rings compares every two of their segments. 320 holes fit; 3000 (about
    # 15 000 vertices, within the edit guard) are refused before any change.
    import time
    lon, lat = PL

    def stacked(k):
        # the extra vertex on the outer ring's bottom edge is what simplifying removes
        outer = rect(lon, lat, 100, 3 * k + 10, -10, -5)
        outer.insert(1, offset_m(lon, lat, 40, -5.01))
        return polygon(outer, *[rect(lon, lat, 80, 2, 0, 3 * i) for i in range(k)])

    for k, refused in ((320, False), (3000, True)):
        t = time.time()
        with Session(write_layer('holes', [feature(stacked(k), name='h')])) as s:
            s.activate('holes')
            s.select('holes', [0])
            before = s.features('holes')
            if not refused:
                r = s.plugin('simplify', {'tolerance': 0.5})
                check(r['changed'] == 1 and r['verticesAfter'] == r['verticesBefore'] - 1, r)
            else:
                e = s.error_of('simplify', {'tolerance': 0.5})
                check('Checking the results for crossing rings is more than one command can do' in e
                      and 'Select fewer features' in e and 'ran too long' not in e, e)
                check(s.features('holes') == before, 'a refused command changed the data')
        print('      %d holes %s in %.1fs' % (k, 'refused' if refused else 'simplified', time.time() - t))


def test_merge_and_fill_gap_of_a_dense_grid_are_refused_up_front():
    # thin rectangles across thin rectangles, each pair crossing four times: the union resolves
    # every crossing, which the comparisons of x ranges alone do not show (70 x 70 bars take
    # more than the host allows)
    import time
    from test_topo import poly
    from test_topo_limits import ll_ring

    def bar(x0, y0, x1, y1):
        return poly(ll_ring([(x0, y0), (x1, y0), (x1, y1), (x0, y1), (x0, y0)]))

    for k, refused in ((40, False), (70, True)):
        t = time.time()
        n = 10.0 * k
        feats = [feature(bar(-5, 10 * i, n, 10 * i + 1), name='h%d' % i) for i in range(k)]
        feats += [feature(bar(10 * i, -5, 10 * i + 1, n), name='v%d' % i) for i in range(k)]
        with Session(write_layer('pl', feats)) as s:
            s.activate('pl')
            s.select('pl', list(range(2 * k)))
            before = s.features('pl')
            if not refused:
                r = s.plugin('merge-with-attributes')
                check(r['merged'] == 2 * k, r)
            else:
                e = s.error_of('fill-gap')
                check('Finding the gaps between these polygons is more than one command can do' in e
                      and 'ran too long' not in e, e)
                e = s.error_of('merge-with-attributes')
                check('Merging these polygons is more than one command can do' in e and 'ran too long' not in e, e)
                check(s.features('pl') == before, 'a refused command changed the data')
        print('      %d x %d bars %s in %.1fs' % (k, k, 'refused' if refused else 'merged', time.time() - t))
