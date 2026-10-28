"""Tests for src/polygons.rs."""

import json
import os
import time

from aelib import (PL, Session, Skip, assert_stubs, check, close, feature, grid_layer, holed_layer,
                   multipolygon, point, polygon, polygon_area_m2, rect, write_layer)

# Commands of this module that are still stubs: remove each one as it is implemented.
STUBS = []

LON, LAT = PL
EPS_DEG = 1e-12


def test_stubs_answer_not_implemented():
    if STUBS:
        assert_stubs(STUBS)


# ---------- helpers ----------

def coords(geom):
    polys = [geom['coordinates']] if geom['type'] == 'Polygon' else geom['coordinates']
    return [c for p in polys for r in p for c in r]


def holes(geom):
    polys = [geom['coordinates']] if geom['type'] == 'Polygon' else geom['coordinates']
    return sum(len(p) - 1 for p in polys)


def parts(geom):
    return 1 if geom['type'] == 'Polygon' else len(geom['coordinates'])


def same_coord(a, b, eps=EPS_DEG):
    return abs(a[0] - b[0]) <= eps and abs(a[1] - b[1]) <= eps


def same_geometry(a, b, eps=EPS_DEG):
    ca, cb = coords(a), coords(b)
    return a['type'] == b['type'] and len(ca) == len(cb) and all(same_coord(x, y, eps) for x, y in zip(ca, cb))


def same_features(a, b):
    return len(a) == len(b) and all(
        x['properties'] == y['properties'] and same_geometry(x['geometry'], y['geometry']) for x, y in zip(a, b))


def snapshot(s, *layers):
    return {name: s.features(name) for name in layers}


def unchanged(s, before):
    return all(same_features(s.features(name), feats) for name, feats in before.items())


def areas(s, layer):
    return [polygon_area_m2(f['geometry']) for f in s.features(layer)]


def layer(name, *rings_m, **props):
    """One feature per rectangle (w, h, x0, y0) in metres from PL."""
    return write_layer(name, [feature(polygon(rect(LON, LAT, *r)), n=i, **props) for i, r in enumerate(rings_m)])


def tools_then_targets(s, targets, target_rows, tools, tool_rows):
    s.select(targets, target_rows)
    s.activate(tools)
    s.select(tools, tool_rows, add=True)


def undo_restores(s, before, command, params=None):
    s.plugin(command, params)
    check(not unchanged(s, before), '%s changed nothing' % command)
    s.undo()
    check(unchanged(s, before), 'undo of %s did not restore the data' % command)


# ---------- cut / clip ----------

def test_cut_with_polygon():
    with Session(grid_layer('parcels', nx=3), layer('cutter', (120, 50, 40, 25))) as s:
        tools_then_targets(s, 'parcels', [0, 1], 'cutter', [0])
        r = s.plugin('cut-with-polygon', {'keepTools': True})
        check(r['changed'] == 2 and r['deleted'] == 0, r)
        a = areas(s, 'parcels')
        check(close(a[0], 7000, 2) and close(a[1], 7000, 2) and close(a[2], 10000, 2), a)
        check(all(holes(f['geometry']) == 0 for f in s.features('parcels')))
        check(len(s.features('cutter')) == 1)
        check(s.selected_ids('parcels') == [1, 2], s.selected_ids('parcels'))


def test_cut_splits_into_multipart_and_deletes_covered():
    cutter = layer('cutter', (20, 120, 40, -10), (120, 120, 190, -10))
    with Session(grid_layer('parcels', nx=3), cutter) as s:
        tools_then_targets(s, 'parcels', [0, 2], 'cutter', [0, 1])
        r = s.plugin('cut-with-polygon', {'keepTools': False})
        check(r['changed'] == 1 and r['deleted'] == 1 and r['toolsDeleted'] == 2, r)
        f = s.features('parcels')
        check([x['properties']['name'] for x in f] == ['c0_0', 'c1_0'], f)
        check(parts(f[0]['geometry']) == 2 and close(polygon_area_m2(f[0]['geometry']), 8000, 2), f[0])
        check(len(s.features('cutter')) == 0)


def test_cut_keeps_shared_vertices_and_undo():
    with Session(grid_layer('parcels', nx=2), layer('cutter', (30, 30, -10, -10))) as s:
        before = snapshot(s, 'parcels', 'cutter')
        tools_then_targets(s, 'parcels', [0], 'cutter', [0])
        s.plugin('cut-with-polygon')
        f = s.features('parcels')
        check(close(polygon_area_m2(f[0]['geometry']), 10000 - 400, 1), f[0])
        shared = [c for c in coords(f[1]['geometry'])[:-1] if any(same_coord(c, d) for d in coords(f[0]['geometry']))]
        check(len(shared) == 2, 'shared vertices: %s' % shared)
        check(same_features([f[1]], [before['parcels'][1]]), 'the neighbour changed')
        s.undo()
        check(unchanged(s, before), 'undo did not restore')


def test_cut_wrong_selection():
    with Session(grid_layer('parcels', nx=2), layer('cutter', (20, 20, 500, 500))) as s:
        before = snapshot(s, 'parcels', 'cutter')
        s.activate('cutter')
        s.select('parcels', [0])
        e = s.error_of('cut-with-polygon')
        check('nothing is selected in the active layer' in e, e)
        tools_then_targets(s, 'parcels', [0], 'cutter', [0])
        e = s.error_of('cut-with-polygon')
        check('do not overlap' in e and 'Select the cutting polygon' in e, e)
        check(unchanged(s, before))


def test_cut_from_active_layer_ignores_touching_neighbours():
    with Session(grid_layer('parcels', nx=3)) as s:
        before = snapshot(s, 'parcels')
        s.activate('parcels')
        s.select('parcels', [1])
        e = s.error_of('cut-with-polygon')
        check('do not overlap' in e, e)
        check(unchanged(s, before))


def test_clip_with_polygon():
    with Session(grid_layer('parcels', nx=3), layer('cutter', (120, 50, 40, 25))) as s:
        before = snapshot(s, 'parcels', 'cutter')
        tools_then_targets(s, 'parcels', [0, 1, 2], 'cutter', [0])
        r = s.plugin('clip-with-polygon')
        check(r['changed'] == 2 and r['deleted'] == 1, r)
        a = areas(s, 'parcels')
        check(len(a) == 2 and close(a[0], 3000, 2) and close(a[1], 3000, 2), a)
        s.undo()
        check(unchanged(s, before))
        # a target inside the clipper does not change: nothing to do
        tools_then_targets(s, 'cutter', [0], 'parcels', [0, 1, 2])
        e = s.error_of('clip-with-polygon')
        check('already lies inside' in e, e)
        check(unchanged(s, before))


# ---------- rings ----------

def test_add_ring():
    rings = layer('rings', (20, 20, 40, 40), (20, 20, 90, 40), (10, 10, 500, 500))
    with Session(grid_layer('parcels', nx=2), rings) as s:
        before = snapshot(s, 'parcels', 'rings')
        tools_then_targets(s, 'parcels', [0], 'rings', [0, 1, 2])
        r = s.plugin('add-ring', {'keepTools': False})
        check(r['rings'] == 1 and r['skipped'] == 2 and r['toolsDeleted'] == 1, r)
        f = s.features('parcels')
        check(holes(f[0]['geometry']) == 1 and close(polygon_area_m2(f[0]['geometry']), 9600, 1), f[0])
        tool = before['rings'][0]['geometry']['coordinates'][0]
        check(all(any(same_coord(c, d) for d in f[0]['geometry']['coordinates'][1]) for c in tool), 'hole moved')
        check(len(s.features('rings')) == 2)
        s.undo()
        check(unchanged(s, before))
        # only the crossing tool: nothing to do
        tools_then_targets(s, 'parcels', [0], 'rings', [1])
        e = s.error_of('add-ring')
        check('crossing' in e, e)
        check(unchanged(s, before))


def test_add_ring_in_active_layer():
    data = layer('parcels', (100, 100, 0, 0), (20, 20, 30, 30))
    with Session(data) as s:
        s.activate('parcels')
        s.select('parcels', [1])
        s.plugin('add-ring', {'keepTools': False})
        f = s.features('parcels')
        check(len(f) == 1 and holes(f[0]['geometry']) == 1, f)
        check(s.selected_ids('parcels') == [1])


def test_delete_ring():
    two = polygon(rect(LON, LAT, 100, 100), list(reversed(rect(LON, LAT, 10, 10, 10, 10))),
                  list(reversed(rect(LON, LAT, 10, 10, 60, 60))))
    with Session(write_layer('holes', [feature(two, name='h')]), grid_layer('plain', nx=1),
                 write_layer('pts', [feature(point(LON, LAT, 65, 65))])) as s:
        before = snapshot(s, 'holes')
        s.select('pts', [0])
        s.activate('holes')
        s.select('holes', [0], add=True)
        r = s.plugin('delete-ring')
        check(r['rings'] == 1, r)
        g = s.features('holes')[0]['geometry']
        check(holes(g) == 1 and close(polygon_area_m2(g), 9900, 1), g)
        s.undo()
        check(unchanged(s, before))
        s.select('holes', [0])
        undo_restores(s, before, 'delete-ring')
        s.select('holes', [0])
        s.plugin('delete-ring')
        g = s.features('holes')[0]['geometry']
        check(holes(g) == 0 and close(polygon_area_m2(g), 10000, 1), g)
        plain = snapshot(s, 'plain')
        s.activate('plain')
        s.select('plain', [0])
        e = s.error_of('delete-ring')
        check('have no holes' in e, e)
        check(unchanged(s, plain))


def test_fill_ring():
    with Session(holed_layer('holed'), grid_layer('plain', nx=1)) as s:
        before = snapshot(s, 'holed')
        s.activate('holed')
        s.select('holed', [0])
        r = s.plugin('fill-ring')
        check(r['created'] == 1, r)
        f = s.features('holed')
        check(len(f) == 2 and holes(f[0]['geometry']) == 1, f)
        check(close(polygon_area_m2(f[1]['geometry']), 1600, 1), f[1])
        check(f[1]['properties']['name'] == 'holed' and f[1]['properties']['n'] == 0, f[1])
        s.undo()
        check(unchanged(s, before))
        s.redo()
        check(s.selected_ids('holed') == [f[1]['id']], s.selected_ids('holed'))
        plain = snapshot(s, 'plain')
        s.activate('plain')
        s.select('plain', [0])
        e = s.error_of('fill-ring')
        check('have no holes' in e, e)
        check(unchanged(s, plain))


# ---------- parts ----------

def test_add_part():
    tools = layer('new', (50, 50, 0, 150), (40, 40, 80, 80))
    with Session(grid_layer('parcels', nx=2), tools) as s:
        before = snapshot(s, 'parcels', 'new')
        tools_then_targets(s, 'parcels', [0], 'new', [0, 1])
        r = s.plugin('add-part')
        check(r['parts'] == 2, r)
        g = s.features('parcels')[0]['geometry']
        check(parts(g) == 2 and close(polygon_area_m2(g), 10000 + 2500 + 1200, 2), g)
        check(same_features([s.features('parcels')[1]], [before['parcels'][1]]), 'the neighbour changed')
        s.undo()
        check(unchanged(s, before))
        tools_then_targets(s, 'parcels', [0, 1], 'new', [0])
        e = s.error_of('add-part')
        check('2 target polygons' in e, e)
        check(unchanged(s, before))


def multi_layer():
    mp = multipolygon(polygon(rect(LON, LAT, 100, 100)), polygon(rect(LON, LAT, 30, 30, 200, 0)),
                      polygon(rect(LON, LAT, 20, 20, 300, 0)))
    return write_layer('multi', [feature(mp, name='m', n=7)])


def test_delete_part():
    pts = write_layer('pts', [feature(point(LON, LAT, 210, 10))])
    with Session(multi_layer(), grid_layer('plain', nx=1), pts) as s:
        before = snapshot(s, 'multi')
        s.activate('multi')
        s.select('multi', [0])
        r = s.plugin('delete-part')
        check(r['deleted'] == 2, r)
        g = s.features('multi')[0]['geometry']
        check(parts(g) == 1 and close(polygon_area_m2(g), 10000, 1), g)
        s.undo()
        check(unchanged(s, before))
        s.select('pts', [0])
        s.select('multi', [0], add=True)
        s.plugin('delete-part')
        g = s.features('multi')[0]['geometry']
        check(parts(g) == 2 and close(polygon_area_m2(g), 10400, 1), g)
        plain = snapshot(s, 'plain')
        s.activate('plain')
        s.select('plain', [0])
        e = s.error_of('delete-part')
        check('more than one part' in e, e)
        check(unchanged(s, plain))


def test_split_off_part():
    with Session(multi_layer(), grid_layer('plain', nx=1)) as s:
        before = snapshot(s, 'multi')
        s.activate('multi')
        s.select('multi', [0])
        r = s.plugin('split-off-part')
        check(r['created'] == 2, r)
        f = s.features('multi')
        check(len(f) == 3 and all(parts(x['geometry']) == 1 for x in f), f)
        check([round(polygon_area_m2(x['geometry'])) for x in f] == [10000, 900, 400], f)
        check(all(x['properties']['name'] == 'm' and x['properties']['n'] == 7 for x in f), f)
        s.undo()
        check(unchanged(s, before))
        s.redo()
        check(len(s.selected_ids('multi')) == 3, s.selected_ids('multi'))
        plain = snapshot(s, 'plain')
        s.activate('plain')
        s.select('plain', [0])
        e = s.error_of('split-off-part')
        check('more than one part' in e, e)
        check(unchanged(s, plain))


def test_locators_without_geometry_are_refused():
    # A selection elsewhere made only of features without a geometry must not fall back to
    # "no locators", which would act on every hole or part.
    two = polygon(rect(LON, LAT, 100, 100), list(reversed(rect(LON, LAT, 10, 10, 10, 10))),
                  list(reversed(rect(LON, LAT, 10, 10, 60, 60))))
    loc = write_layer('loc', [feature(point(LON, LAT, 65, 65), k=1), feature(None, k=2)])
    with Session(write_layer('holes', [feature(two, name='h')]), multi_layer(), loc) as s:
        before = snapshot(s, 'holes', 'multi')
        for layer_name, commands, what in (('holes', ('delete-ring', 'fill-ring'), 'holes'),
                                           ('multi', ('delete-part', 'split-off-part'), 'parts')):
            for c in commands:
                s.select('loc', [1])
                s.activate(layer_name)
                s.select(layer_name, [0], add=True)
                e = s.error_of(c)
                check("The selected features in 'loc' have no geometry to pick %s with; select features with a "
                      "geometry or clear that selection" % what in e, (c, e))
                check(unchanged(s, before), '%s changed the data' % c)
        # one usable locator next to the null one picks as usual
        s.select('loc', [0, 1])
        s.activate('holes')
        s.select('holes', [0], add=True)
        r = s.plugin('delete-ring')
        check(r['rings'] == 1, r)


# ---------- gaps ----------

def test_fill_gap():
    ring = [(i, j) for j in range(3) for i in range(3) if (i, j) != (1, 1)]
    cells = [feature(polygon(rect(LON, LAT, 100, 100, 100 * i, 100 * j)), name='c%d%d' % (i, j)) for i, j in ring]
    g = 0.05  # a g x g hole between four polygons, below the 0.01 m2 threshold
    sliver = [feature(polygon(rect(LON, LAT, *r))) for r in
              [(100, 50, 400, 0), (50, g, 400, 50), (50, g, 450 + g, 50), (100, 50, 400, 50 + g)]]
    with Session(write_layer('cells', cells), holed_layer('holed'), write_layer('sliv', sliver)) as s:
        before = snapshot(s, 'cells')
        s.activate('cells')
        s.select('cells', list(range(8)))
        r = s.plugin('fill-gap')
        check(r['created'] == 1, r)
        f = s.features('cells')
        check(len(f) == 9 and close(polygon_area_m2(f[8]['geometry']), 10000, 2), f[8])
        check(f[8]['properties'].get('name') is None, f[8])
        s.undo()
        check(unchanged(s, before))
        r = s.plugin('fill-gap', {'wholeLayer': True})
        check(r['created'] == 1, r)
        # a hole of a single feature is not a gap
        others = snapshot(s, 'holed', 'sliv')
        s.activate('holed')
        s.select('holed', [0])
        e = s.error_of('fill-gap')
        check('no enclosed gap' in e, e)
        s.activate('sliv')
        s.select('sliv', [0, 1, 2, 3])
        e = s.error_of('fill-gap')
        check('1 sliver(s) ignored' in e, e)
        check(unchanged(s, others))


# ---------- real data ----------

def real_session(*extra):
    path = os.environ.get('AE_REAL_DATA')
    if not path:
        raise Skip('AE_REAL_DATA is not set')
    s = Session(path, *extra)
    mine = {f.stem for f in extra}
    polys = [l for l in s.layers().values() if l['dim'] == 'polygon' and l['name'] not in mine]
    if not polys:
        s.close()
        raise Skip('no polygon layer in %s' % path)
    return s, polys[0]['name']


def real_features():
    s, name = real_session()
    with s:
        return name, s.features(name)


def timed(s, command, params=None):
    t = time.time()
    r = s.plugin(command, params)
    print('      real %s: %.2fs %s' % (command, time.time() - t, r.get('message')))
    return r


def bbox(geom):
    cs = coords(geom)
    xs, ys = [c[0] for c in cs], [c[1] for c in cs]
    return min(xs), min(ys), max(xs), max(ys)


def inside(pt, ring):
    x, y = pt
    n = False
    for (x1, y1), (x2, y2) in zip(ring, ring[1:]):
        if (y1 > y) != (y2 > y) and x < x1 + (y - y1) * (x2 - x1) / (y2 - y1):
            n = not n
    return n


def only_changed(before, after, rows):
    changed = [i for i in range(len(before)) if not same_geometry(before[i]['geometry'], after[i]['geometry'])]
    check(changed == rows, 'changed rows %s, expected %s' % (changed, rows))


def keeps_vertices(old, new, keep=lambda c: True):
    nc = coords(new)
    check(all(any(same_coord(c, d) for d in nc) for c in coords(old) if keep(c)), 'an untouched vertex moved')


def real_cut_or_clip(command):
    name, feats = real_features()
    k = max(range(60), key=lambda i: len(coords(feats[i]['geometry'])))
    x0, y0, x1, y1 = bbox(feats[k]['geometry'])
    mid = (x0 + x1) / 2
    ring = [[x0 - 1e-4, y0 - 1e-4], [mid, y0 - 1e-4], [mid, y1 + 1e-4], [x0 - 1e-4, y1 + 1e-4], [x0 - 1e-4, y0 - 1e-4]]
    s, name = real_session(write_layer('cutter', [feature(polygon(ring))]))
    with s:
        tools_then_targets(s, name, [k], 'cutter', [0])
        r = timed(s, command)
        after = s.features(name)
        check(r['changed'] == 1 and len(after) == len(feats), r)
        a0, a1 = polygon_area_m2(feats[k]['geometry']), polygon_area_m2(after[k]['geometry'])
        check(0 < a1 < a0, (a0, a1))
        # neighbours change only by the shared vertices topological editing adds
        from test_topo import neighbours_only_gained
        neighbours_only_gained(feats, after, {k}, r, [after[k]['geometry']])
        # the parcel's kept vertices stay those of its neighbours, bit for bit
        kept = (lambda c: c[0] > mid + 1e-7) if command == 'cut-with-polygon' else (lambda c: c[0] < mid - 1e-7)
        keeps_vertices(feats[k]['geometry'], after[k]['geometry'], kept)
        nb = [c for i, f in enumerate(after) if i != k for c in coords(f['geometry'])]
        shared = [c for c in coords(feats[k]['geometry']) if kept(c) and any(same_coord(c, d) for d in nb)]
        check(shared, 'the parcel shares no kept vertex with its neighbours')
        s.undo()
        check(same_features(s.features(name), feats), 'undo did not restore')


def test_real_cut_keeps_neighbours():
    real_cut_or_clip('cut-with-polygon')


def test_real_clip_keeps_neighbours():
    real_cut_or_clip('clip-with-polygon')


def test_real_rings_and_parts():
    name, feats = real_features()
    ext = max(bbox(f['geometry'])[2] for f in feats)
    k = d = None
    for i, f in enumerate(feats):
        g = f['geometry']
        if g['type'] != 'Polygon' or len(g['coordinates']) > 1:
            continue
        x0, y0, x1, y1 = bbox(g)
        cx, cy = (x0 + x1) / 2, (y0 + y1) / 2
        d = 1e-5  # ~1 m
        sq = [[cx - d, cy - d], [cx + d, cy - d], [cx + d, cy + d], [cx - d, cy + d]]
        far = [c for c in coords(g) if abs(c[0] - cx) < 5 * d and abs(c[1] - cy) < 5 * d]
        if all(inside(c, g['coordinates'][0]) for c in sq) and not far:
            k = i
            break
    check(k is not None, 'no parcel to put a hole into')
    tools = write_layer('tools', [feature(polygon(sq + [sq[0]])),
                                  feature(polygon([[ext + 1e-3, cy], [ext + 1e-3 + d, cy], [ext + 1e-3 + d, cy + d],
                                                   [ext + 1e-3, cy + d], [ext + 1e-3, cy]]))])
    orig = feats[k]['geometry']
    s, name = real_session(tools)
    # every command is undone right away: selection changes between commands are undo steps too
    with s:
        def edit_active(command):
            s.activate(name)
            s.select(name, [k])
            return timed(s, command)

        def undo_to(state):
            s.undo()
            check(same_features(s.features(name), state), 'undo did not restore')

        tools_then_targets(s, name, [k], 'tools', [0])
        timed(s, 'add-ring')
        holed = s.features(name)
        only_changed(feats, holed, [k])
        check(holes(holed[k]['geometry']) == 1, holed[k])
        keeps_vertices(orig, holed[k]['geometry'])
        r = edit_active('fill-ring')
        f = s.features(name)
        check(r['created'] == 1 and len(f) == len(feats) + 1, r)
        check(close(polygon_area_m2(f[-1]['geometry']), polygon_area_m2(polygon(sq + [sq[0]])), 1e-3), f[-1])
        parent = {key: v for key, v in feats[k]['properties'].items() if key.lower() not in ('id', 'fid')}
        check(f[-1]['properties'] == parent, (f[-1]['properties'], parent))
        undo_to(holed)
        edit_active('delete-ring')
        check(same_geometry(s.features(name)[k]['geometry'], orig), 'delete-ring did not give the original back')
        undo_to(holed)

        tools_then_targets(s, name, [k], 'tools', [1])
        timed(s, 'add-part')
        multi = s.features(name)
        only_changed(holed, multi, [k])
        check(parts(multi[k]['geometry']) == 2, multi[k])
        r = edit_active('split-off-part')
        f = s.features(name)
        check(r['created'] == 1 and len(f) == len(feats) + 1, r)
        check(same_geometry(f[k]['geometry'], holed[k]['geometry']), 'split-off-part changed the kept part')
        undo_to(multi)
        edit_active('delete-part')
        check(same_geometry(s.features(name)[k]['geometry'], holed[k]['geometry']), 'delete-part changed the kept part')
        undo_to(multi)


def test_real_fill_gap_whole_layer():
    s, name = real_session()
    with s:
        s.activate(name)
        t = time.time()
        try:
            r = s.plugin('fill-gap', {'wholeLayer': True})
            out = json.dumps(r)
        except Exception as e:  # noqa: BLE001 - no gaps is fine, running out of fuel is not
            out = str(e)
            check('no enclosed gap' in out, out)
        print('      real fill-gap (whole layer): %.2fs %s' % (time.time() - t, out[:160]))
