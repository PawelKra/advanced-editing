"""Edge cases for merge/exchange attributes, wrong selections for every command, and real
parcels (AE_REAL_DATA)."""

import json
import math
import os

from aelib import PREFIX, CommandError, Session, Skip, check, command_ids, feature, tmpdir, write_layer
from test_division import area_ell, tol
from test_edge_lines import (HOLE, O, SQ, L, Problems, T, actionable, check_partition, cw, faces_of, fingerprint,
                             flat, gmap, metric, nparts, run_two_layers, shapely_job, short, sq)


# ---------- merge / exchange ----------

def merge_case(P, label, geoms, props=None, params=None):
    props = props or [{'name': 'f%d' % i} for i in range(len(geoms))]
    data = write_layer('mg', [feature(g, **p) for g, p in zip(geoms, props)])
    with Session(data) as s:
        before = s.features('mg')
        s.activate('mg')
        s.select('mg', range(len(geoms)))
        try:
            r = s.plugin('merge-with-attributes', params or {})
        except CommandError as e:
            P.add(label, 'refused: %s' % short(e))
            P.expect(label, fingerprint(s.features('mg')) == fingerprint(before), 'a refusal changed data')
            return None, before
        after = s.features('mg')
        s.undo()
        P.expect(label, fingerprint(s.features('mg')) == fingerprint(before), 'undo did not restore')
    P.expect(label, len(after) == 1, '%d features after merging' % len(after))
    return after[0], before


def test_merge_polygons_touching_at_a_point():
    P = Problems()
    g, _ = merge_case(P, 'corner touch', [T(sq(0, 0, 100)), T(sq(100, 100, 100))])
    if g:
        r = shapely_job(geoms=[gmap(g['geometry'], metric(g['geometry']))])
        P.expect('corner touch', r['valid'][0], 'invalid: %s' % r['why'][0])
        P.expect('corner touch', abs(r['area'][0] - 20000) < 1e-3, 'area %.4f' % r['area'][0])
        P.expect('corner touch', nparts(g['geometry']) == 2, '%d parts' % nparts(g['geometry']))
    g, _ = merge_case(P, 'hole and its island', [T(SQ, HOLE), T(sq(30, 30, 40))])
    if g:
        r = shapely_job(geoms=[gmap(g['geometry'], metric(g['geometry']))])
        P.expect('hole and its island', r['valid'][0], 'invalid: %s' % r['why'][0])
        P.expect('hole and its island', abs(r['area'][0] - 10000) < 1e-3, 'area %.4f' % r['area'][0])
        P.expect('hole and its island', g['geometry']['type'] == 'Polygon' and len(g['geometry']['coordinates']) == 1,
                 'the filled hole is still there: %s rings' % [len(p) for p in g['geometry']['coordinates']])
    g, _ = merge_case(P, 'overlapping squares', [T(sq(0, 0, 100)), T(sq(50, 50, 100))])
    if g:
        r = shapely_job(geoms=[gmap(g['geometry'], metric(g['geometry']))])
        P.expect('overlapping squares', r['valid'][0] and abs(r['area'][0] - 17500) < 1e-3,
                 'valid %s area %.4f' % (r['valid'][0], r['area'][0]))
    P.done()


def test_merge_overlapping_and_t_lines():
    P = Problems()
    g, _ = merge_case(P, 'collinear overlap', [L((0, 0), (100, 0)), L((50, 0), (150, 0))])
    if g:
        r = shapely_job(geoms=[gmap(g['geometry'], metric(g['geometry']))])
        P.expect('collinear overlap', abs(r['length'][0] - 150) < 1e-3,
                 'merged length %.3f, the lines cover 150 m (%s)' % (r['length'][0], g['geometry']['type']))
        P.expect('collinear overlap', g['geometry']['type'] == 'LineString', 'not one line: %s' % g['geometry']['type'])
    g, before = merge_case(P, 'contained line', [L((0, 0), (40, 0), (100, 0)), L((100, 0), (20, 0))])
    if g:
        r = shapely_job(geoms=[gmap(g['geometry'], metric(g['geometry']))])
        P.expect('contained line', abs(r['length'][0] - 100) < 1e-3, 'merged length %.3f' % r['length'][0])
        orig = [c for f in before for c in flat(f['geometry'])]
        P.expect('contained line', all(any(abs(c[0] - o[0]) < 1e-12 and abs(c[1] - o[1]) < 1e-12 for o in orig)
                                       for c in flat(g['geometry'])), 'vertices moved')
    g, _ = merge_case(P, 'T-junction', [L((0, 0), (100, 0)), L((50, 0), (50, 50))])
    if g:
        r = shapely_job(geoms=[gmap(g['geometry'], metric(g['geometry']))])
        P.expect('T-junction', abs(r['length'][0] - 150) < 1e-3, 'merged length %.3f' % r['length'][0])
        P.expect('T-junction', nparts(g['geometry']) == 2, '%d parts' % nparts(g['geometry']))
    g, _ = merge_case(P, 'reversed ends meet', [L((0, 0), (100, 0)), L((200, 0), (100, 0))])
    if g:
        P.expect('reversed ends meet', g['geometry']['type'] == 'LineString' and len(g['geometry']['coordinates']) == 3,
                 'not joined into one line: %s %d' % (g['geometry']['type'], len(flat(g['geometry']))))
    g, _ = merge_case(P, 'closed loop', [L((0, 0), (100, 0)), L((100, 0), (100, 100)), L((100, 100), (0, 0))])
    if g:
        c = g['geometry']['coordinates']
        P.expect('closed loop', g['geometry']['type'] == 'LineString' and c[0] == c[-1] and len(c) == 4,
                 'loop not joined: %s %s' % (g['geometry']['type'], len(flat(g['geometry']))))
    P.done()


def test_merge_sum_with_nulls_and_text_numbers():
    P = Problems()
    props = [{'name': 'a', 'n': None, 'x': 1.5, 's': '7'}, {'name': 'b', 'n': None, 'x': None, 's': '8'},
             {'name': 'c', 'n': None, 'x': 2, 's': None}]
    g, _ = merge_case(P, 'sum nulls', [T(sq(0, 0, 100)), T(sq(100, 0, 50)), T(sq(150, 0, 20))], props, {'sum': True})
    if g:
        p = g['properties']
        P.expect('sum nulls', p['name'] == 'a', 'kept %s, expected the largest (a)' % p['name'])
        P.expect('sum nulls', p['n'] is None, 'all-null field became %r' % p['n'])
        P.expect('sum nulls', p['x'] == 3.5, 'x = %r, expected 3.5' % p['x'])
        P.expect('sum nulls', p['s'] == '7', 'text field changed: %r' % p['s'])
    P.done()


def test_exchange_across_layers_refused():
    P = Problems()
    a = write_layer('ea', [feature(T(sq(0, 0, 100)), name='a', n=1), feature(T(sq(100, 0, 100)), name='b', n=2)])
    b = write_layer('eb', [feature(T(sq(0, 200, 100)), name='z', n=9)])
    with Session(a, b) as s:
        before = (s.features('ea'), s.features('eb'))
        s.select('eb', [0])
        s.activate('ea')
        s.select('ea', [0], add=True)
        try:
            r = s.plugin('exchange-attributes')
            P.add('two layers', 'succeeded: %s' % r)
        except CommandError as e:
            P.expect('two layers', 'two' in str(e) and actionable(str(e)), 'message: %s' % short(e))
        P.expect('two layers', (s.features('ea'), s.features('eb')) == before, 'data changed')
    # nulls swap as nulls
    c = write_layer('ec', [feature(T(sq(0, 0, 100)), name='a', n=None), feature(T(sq(100, 0, 100)), name=None, n=2)])
    with Session(c) as s:
        s.activate('ec')
        s.select('ec', [0, 1])
        s.plugin('exchange-attributes')
        f = s.features('ec')
        P.expect('nulls', f[0]['properties'] == {'name': None, 'n': 2} and f[1]['properties'] == {'name': 'a', 'n': None},
                 [x['properties'] for x in f])
    P.done()


# ---------- wrong selections for every command ----------

def clear(s):
    try:
        s.clear_selection()
    except CommandError:
        pass  # nothing was selected


COMMANDS = [c for c in command_ids() if c != 'panel-state']


def fixture_layers():
    return [write_layer('polys', [feature(T(sq(0, 0, 100)), name='p0'), feature(T(sq(100, 0, 100)), name='p1')]),
            write_layer('lines', [feature(L((-10, 50), (210, 50)), name='l0')]),
            write_layer('pts', [feature({'type': 'Point', 'coordinates': flat(L((50, 50), (51, 51)))[0]}, name='q')])]


def snapshot(s):
    return fingerprint(s.features('polys') + s.features('lines') + s.features('pts'))


def test_every_command_refuses_an_empty_selection():
    P = Problems()
    with Session(*fixture_layers()) as s:
        before = snapshot(s)
        for active in ('polys', 'lines'):
            for c in COMMANDS:
                clear(s)
                s.activate(active)
                try:
                    r = s.plugin(c)
                    P.add('%s/%s' % (c, active), 'ran with nothing selected: %s' % short(json.dumps(r)))
                    s.undo()
                except CommandError as e:
                    P.expect('%s/%s' % (c, active), actionable(str(e)), 'message not actionable: %s' % short(e))
                if snapshot(s) != before:
                    P.add('%s/%s' % (c, active), 'data changed')
                    break
    P.done()


def test_every_command_refuses_a_selection_only_in_another_layer():
    P = Problems()
    with Session(*fixture_layers()) as s:
        before = snapshot(s)
        for c in COMMANDS:
            clear(s)
            s.select('lines', [0])
            s.activate('polys')
            try:
                r = s.plugin(c)
                P.add(c, 'ran with nothing selected in the active layer: %s' % short(json.dumps(r)))
                s.undo()
            except CommandError as e:
                P.expect(c, actionable(str(e)), 'message not actionable: %s' % short(e))
            if snapshot(s) != before:
                P.add(c, 'data changed')
                break
    P.done()


def test_points_active_layer():
    """Points are neither tools nor targets for the geometry commands: a clear refusal, no
    change (copy-move works on points)."""
    P = Problems()
    with Session(*fixture_layers()) as s:
        before = snapshot(s)
        for c in COMMANDS:
            clear(s)
            s.select('polys', [0])
            s.activate('pts')
            s.select('pts', [0], add=True)
            try:
                r = s.plugin(c)
                if c not in ('copy-move', 'delete-ring', 'fill-ring', 'delete-part', 'split-off-part'):
                    P.add(c, 'ran with a point as the tool: %s' % short(json.dumps(r)))
                s.undo()
            except CommandError as e:
                P.expect(c, bool(str(e).strip()) and 'panicked' not in str(e) and 'unreachable' not in str(e),
                         'message: %s' % short(e))
            if snapshot(s) != before:
                P.add(c, 'data changed')
                break
    P.done()


def test_raster_active_layer():
    asc = tmpdir() / 'data' / 'grid.asc'
    asc.parent.mkdir(exist_ok=True)
    lon, lat = 19.0, 52.0
    asc.write_text('ncols 4\nnrows 4\nxllcorner %r\nyllcorner %r\ncellsize 0.0001\nNODATA_value -9999\n' % (lon, lat)
                   + '\n'.join(' '.join(str(i * 4 + j) for j in range(4)) for i in range(4)) + '\n')
    P = Problems()
    with Session(*fixture_layers(), asc) as s:
        try:
            s.activate('grid')
        except CommandError as e:
            raise Skip('no raster layer: %s' % short(e))
        before = snapshot(s)
        s.select('polys', [0])
        for c in COMMANDS:
            s.activate('grid')
            try:
                r = s.plugin(c)
                P.add(c, 'ran with a raster active: %s' % short(json.dumps(r)))
                s.undo()
            except CommandError as e:
                P.expect(c, bool(str(e).strip()) and 'panicked' not in str(e), 'message: %s' % short(e))
            if snapshot(s) != before:
                P.add(c, 'data changed')
                break
    P.done()


def test_mixed_geometry_selection():
    """Polygons and lines selected together as tools: commands for one kind use those and
    refuse or ignore the other, never corrupt it."""
    P = Problems()
    a = write_layer('tg', [feature(T(sq(0, 0, 100)), name='t')])
    tools = write_layer('mix', [feature(L((-10, 50), (110, 50)), name='l'), feature(T(sq(20, 20, 10)), name='p')])
    with Session(a, tools) as s:
        before = fingerprint(s.features('tg') + s.features('mix'))
        for c in ('split-with-line', 'cut-with-polygon', 'clip-with-polygon', 'add-ring', 'reshape', 'add-part',
                  'trim-extend'):
            clear(s)
            s.select('tg', [0])
            s.activate('mix')
            s.select('mix', [0, 1], add=True)
            try:
                r = s.plugin(c)
                after = s.features('tg')
                m = metric(after[0]['geometry'])
                rep = shapely_job(geoms=[gmap(f['geometry'], m) for f in after if f['geometry']['type'].endswith('Polygon')])
                P.expect(c, all(rep['valid']), 'invalid output: %s' % rep['why'])
                s.undo()
            except CommandError as e:
                P.expect(c, actionable(str(e)), 'message not actionable: %s' % short(e))
            if fingerprint(s.features('tg') + s.features('mix')) != before:
                P.add(c, 'undo/refusal left changes')
                break
    P.done()


def test_divide_parts_out_of_range():
    P = Problems()
    with Session(write_layer('dv', [feature(T(sq(0, 0, 10)), name='p')])) as s:
        before = s.features('dv')
        s.activate('dv')
        for parts in (1, 0, -3, 2.5, 10000):
            s.select('dv', [0])
            try:
                r = s.plugin('divide-equal', {'parts': parts})
                n = len(s.features('dv'))
                P.add('parts %r' % parts, 'accepted: %d features (%s)' % (n, short(r.get('message', ''))))
                s.undo()
            except CommandError as e:
                P.expect('parts %r' % parts, 'part' in str(e).lower(), 'message: %s' % short(e))
            P.expect('parts %r' % parts, fingerprint(s.features('dv')) == fingerprint(before), 'data changed')
    P.done()


# ---------- real parcels ----------

def real():
    path = os.environ.get('AE_REAL_DATA')
    if not path:
        raise Skip('AE_REAL_DATA is not set')
    return path


def bbox(g):
    cs = flat(g)
    return min(c[0] for c in cs), min(c[1] for c in cs), max(c[0] for c in cs), max(c[1] for c in cs)


def real_layer(s):
    return [l['name'] for l in s.layers().values() if l['dim'] == 'polygon' and l['name'] != 'tl'][0]


def test_real_zigzag_split_across_parcels():
    path = real()
    with Session(path) as s:
        name = real_layer(s)
        feats = s.features(name)
    # a mid-sized parcel; a zigzag across its bounding box (and into its neighbours)
    areas = sorted(range(len(feats)), key=lambda i: area_ell(feats[i]['geometry']))
    k = areas[len(areas) // 2]
    x0, y0, x1, y1 = bbox(feats[k]['geometry'])
    dx, dy = x1 - x0, y1 - y0
    zig = {'type': 'LineString', 'coordinates': [[x0 - 0.3 * dx + i * 1.6 * dx / 6, (y0 - 0.2 * dy) if i % 2 else (y1 + 0.2 * dy)]
                                                  for i in range(7)]}
    hit = [i for i, f in enumerate(feats) if not (bbox(f['geometry'])[2] < x0 - 0.3 * dx or bbox(f['geometry'])[0] > x1 + 0.7 * dx
                                                 or bbox(f['geometry'])[3] < y0 - 0.2 * dy or bbox(f['geometry'])[1] > y1 + 0.2 * dy)]
    tl = write_layer('tl', [feature(zig, name='zig')])
    P = Problems()
    with Session(path, tl) as s:
        s.select(name, hit)
        s.activate('tl')
        s.select('tl', [0], add=True)
        try:
            r = s.plugin('split-with-line', {'keepTools': True})
        except CommandError as e:
            raise AssertionError('refused: %s' % short(e, 300))
        after = s.features(name)
        s.undo()
        P.expect('undo', fingerprint(s.features(name)) == fingerprint(feats), 'undo did not restore')
    ids0 = {f['id'] for f in feats}
    m = metric(feats[k]['geometry'])
    split = 0
    for i in hit:
        fid = feats[i]['id']
        same = [f for f in after if f['id'] == fid]
        new = [f for f in after if f['id'] not in ids0 and f['properties'] == feats[i]['properties']]
        pieces = [f['geometry'] for f in same + new]
        want = faces_of(feats[i]['geometry'], [zig], m)
        n = sum(len(p) for p in want if len(p) > 1) or 1
        if n == 1:
            P.expect('parcel %s' % fid, len(pieces) == 1, '%d pieces, the line does not cross it' % len(pieces))
            continue
        split += 1
        check_partition(P, 'parcel %s' % fid, feats[i]['geometry'], pieces, m, want=n,
                        faces_from=[zig] if feats[i]['geometry']['type'] == 'Polygon' else None)
    untouched = [f for i, f in enumerate(feats) if i not in hit]
    after_by_id = {f['id']: f for f in after}
    P.expect('others', all(fingerprint([after_by_id.get(f['id'], {'id': None, 'properties': None, 'geometry': None})])
                           == fingerprint([f]) for f in untouched), 'unselected parcels changed')
    allp = [gmap(f['geometry'], m) for f in after if f['id'] in {feats[i]['id'] for i in hit} or f['id'] not in ids0]
    rep = shapely_job(geoms=allp)
    ov = [o for o in rep['overlaps'] if abs(o[2]) > 0.01]
    P.expect('all pieces', not ov, 'pieces of neighbouring parcels overlap: %s' % ov[:3])
    print('      real zigzag: %d parcels selected, %d split, %d features after' % (len(hit), split, len(after)))
    P.done()


def test_real_divide_many_angles():
    path = real()
    with Session(path) as s:
        name = real_layer(s)
        feats = s.features(name)
    pick = [i for i in range(0, len(feats), max(1, len(feats) // 8))][:8]
    P = Problems()
    with Session(path) as s:
        s.activate(name)
        for j, i in enumerate(pick):
            parts, angle = 3 + j % 5, (37 * j + 11) % 180
            s.select(name, [i])
            before = s.features(name)
            try:
                r = s.plugin('divide-equal', {'parts': parts, 'angle': angle})
            except CommandError as e:
                P.add('parcel %d' % i, 'refused: %s' % short(e))
                continue
            after = s.features(name)
            old = {f['id'] for f in before}
            pieces = [f['geometry'] for f in after if f['id'] == before[i]['id'] or f['id'] not in old]
            total = area_ell(feats[i]['geometry'])
            P.expect('parcel %d' % i, len(pieces) == parts, '%d pieces, expected %d' % (len(pieces), parts))
            errs = [round(area_ell(g) - total / parts, 4) for g in pieces if abs(area_ell(g) - total / parts) > tol(total / parts)]
            P.expect('parcel %d (%d parts at %d)' % (i, parts, angle), not errs, 'area errors (m²): %s' % errs)
            m = metric(feats[i]['geometry'])
            rep = shapely_job(geoms=[gmap(g, m) for g in pieces])
            P.expect('parcel %d' % i, all(rep['valid']), 'invalid: %s' % [w for w in rep['why'] if w != 'Valid Geometry'])
            ov = [o for o in rep['overlaps'] if abs(o[2]) > 0.01]
            P.expect('parcel %d' % i, not ov, 'overlaps %s' % ov)
            s.undo()
            P.expect('parcel %d' % i, fingerprint(s.features(name)) == fingerprint(before), 'undo')
    P.done()


# ---------- simplify / smooth / densify keep polygons valid ----------

def edit_one(P, label, command, geom, params):
    data = write_layer('ed', [feature(geom, name='e')])
    with Session(data) as s:
        before = s.features('ed')
        s.activate('ed')
        s.select('ed', [0])
        try:
            r = s.plugin(command, params)
        except CommandError as e:
            P.expect(label, bool(str(e).strip()), 'empty message')
            P.expect(label, fingerprint(s.features('ed')) == fingerprint(before), 'a refusal changed data')
            return None
        g = s.features('ed')[0]['geometry']
        s.undo()
        P.expect(label, fingerprint(s.features('ed')) == fingerprint(before), 'undo did not restore')
    if g['type'].endswith('Polygon'):
        rep = shapely_job(geoms=[gmap(g, metric(geom))])
        P.expect(label, rep['valid'][0], 'invalid result: %s' % rep['why'][0])
    return g


def test_simplify_smooth_hole_near_the_shell():
    P = Problems()
    bump = T([(0, 0), (40, 0), (50, -5), (60, 0), (100, 0), (100, 100), (0, 100)], [(48, -3), (50, -1), (52, -3)])
    edit_one(P, 'simplify: hole in a bump', 'simplify', bump, {'tolerance': 6})
    corner = T(SQ, cw(sq(2, 2, 4)))
    edit_one(P, 'smooth: hole near a corner', 'smooth', corner, {'iterations': 1})
    edit_one(P, 'smooth: hole near a corner x3', 'smooth', corner, {'iterations': 3})
    # two holes that a corner cut would make touch
    two = T(SQ, cw(sq(30, 30, 10)), cw(sq(40.2, 40.2, 10)))
    edit_one(P, 'smooth: corner-touching holes', 'smooth', two, {'iterations': 2})
    edit_one(P, 'simplify: corner-touching holes', 'simplify', two, {'tolerance': 15})
    P.done()


def test_simplify_self_touching_after_removal():
    """A narrow inlet whose removal makes the shell cross itself."""
    P = Problems()
    inlet = T([(0, 0), (100, 0), (100, 100), (52, 100), (52, 3), (50, 1), (48, 3), (48, 100), (0, 100)])
    edit_one(P, 'simplify: narrow inlet', 'simplify', inlet, {'tolerance': 3})
    edit_one(P, 'smooth: narrow inlet', 'smooth', inlet, {'iterations': 3})
    P.done()


def test_densify_degenerate_lines():
    P = Problems()
    g = edit_one(P, 'densify: zero-length segments', 'densify', L((0, 0), (0, 0), (10, 0), (10, 0)), {'distance': 1})
    if g:
        m = metric(g)
        pts = [m(c) for c in g['coordinates']]
        segs = [math.dist(a, b) for a, b in zip(pts, pts[1:])]
        P.expect('densify: zero-length segments', max(segs) <= 1 + 1e-6, 'longest segment %.4f' % max(segs))
    P.done()
