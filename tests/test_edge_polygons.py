"""Edge cases for cut/clip with polygon, rings, parts and fill gap.

Expected areas are computed with shapely from the same inputs (see test_edge_lines.py)."""

from aelib import CommandError, Session, check, feature, write_layer
from test_edge_lines import (HOLE, O, SQ, Problems, T, closed, cw, fingerprint, flat, gmap, metric, mpoly,
                             nparts, poly, run_two_layers, same_geom, shapely_job, short, sq)


def geom_report(geoms, ref):
    m = metric(ref)
    return shapely_job(geoms=[gmap(g, m) for g in geoms])


def area_of(g, ref=None):
    return geom_report([g], ref or g)['area'][0]


def nholes(g):
    polys = [g['coordinates']] if g['type'] == 'Polygon' else g['coordinates']
    return sum(len(p) - 1 for p in polys)


def overlay_case(P, label, command, target, tools, want_area=None, want_deleted=False, unchanged=False,
                 want_parts=None, want_holes=None, refuse_ok=False):
    x = run_two_layers(command, [target], tools, {'keepTools': True})
    if x['err'] is not None:
        P.expect(label, unchanged or refuse_ok, 'refused: %s' % short(x['err']))
        P.expect(label, fingerprint(x['after']) == fingerprint(x['before']), 'a refused command changed the data')
        P.expect(label, bool(x['err'].strip()), 'empty message')
        return x
    if want_deleted:
        P.expect(label, len(x['after']) == 0, 'target not deleted: %s' % [f['geometry']['type'] for f in x['after']])
        return x
    if not P.expect(label, len(x['after']) == 1, '%d features' % len(x['after'])):
        return x
    g = x['after'][0]['geometry']
    if unchanged:
        P.expect(label, same_geom(g, target, 1e-12), 'the target changed (area %.4f)' % area_of(g, target))
        return x
    r = geom_report([g], target)
    P.expect(label, r['valid'][0], 'invalid: %s' % r['why'][0])
    if want_area is not None:
        P.expect(label, abs(r['area'][0] - want_area) <= max(1e-4, 1e-7 * want_area),
                 'area %.6f, expected %.6f' % (r['area'][0], want_area))
    if want_parts is not None:
        P.expect(label, nparts(g) == want_parts, '%d parts, expected %d' % (nparts(g), want_parts))
    if want_holes is not None:
        P.expect(label, nholes(g) == want_holes, '%d holes, expected %d' % (nholes(g), want_holes))
    rest = x['restored']
    P.expect(label, rest is not None and fingerprint(rest[0]) == fingerprint(x['before']), 'undo did not restore')
    return x


# ---------- cut / clip ----------

def test_cut_clip_tool_equals_target():
    P = Problems()
    overlay_case(P, 'cut: same square', 'cut-with-polygon', T(SQ), [T(SQ)], want_deleted=True)
    overlay_case(P, 'clip: same square', 'clip-with-polygon', T(SQ), [T(SQ)], unchanged=True)
    # the same square drawn from another start vertex and the other way round
    other = T([(100, 100), (100, 0), (0, 0), (0, 100)])
    overlay_case(P, 'cut: same square, other ring order', 'cut-with-polygon', T(SQ), [other], want_deleted=True)
    overlay_case(P, 'clip: same square, other ring order', 'clip-with-polygon', T(SQ), [other], unchanged=True)
    P.done()


def test_cut_clip_tool_sharing_an_edge_only():
    P = Problems()
    nb = T(sq(100, 0, 100))
    overlay_case(P, 'cut: edge neighbour', 'cut-with-polygon', T(SQ), [nb], unchanged=True)
    # clip: per the help, a selected target entirely outside the tools is deleted
    overlay_case(P, 'clip: edge neighbour', 'clip-with-polygon', T(SQ), [nb], want_deleted=True)
    corner = T(sq(100, 100, 50))
    overlay_case(P, 'cut: corner neighbour', 'cut-with-polygon', T(SQ), [corner], unchanged=True)
    # sharing half an edge
    half = T(sq(100, 50, 50))
    overlay_case(P, 'cut: half-edge neighbour', 'cut-with-polygon', T(SQ), [half], unchanged=True)
    P.done()


def test_cut_clip_tool_with_a_hole():
    P = Problems()
    donut = T(sq(20, 20, 60), cw(sq(40, 40, 20)))
    overlay_case(P, 'cut: donut', 'cut-with-polygon', T(SQ), [donut], want_area=10000 - 3600 + 400,
                 want_parts=2, want_holes=1)
    overlay_case(P, 'clip: donut', 'clip-with-polygon', T(SQ), [donut], want_area=3600 - 400, want_parts=1, want_holes=1)
    # the target lies entirely in the tool's hole
    small = T(sq(45, 45, 10))
    overlay_case(P, 'cut: target in the hole', 'cut-with-polygon', small, [donut], unchanged=True)
    overlay_case(P, 'clip: target in the hole', 'clip-with-polygon', small, [donut], want_deleted=True)
    P.done()


def test_cut_clip_multipart_tool_and_three_pieces():
    P = Problems()
    two = mpoly(O, [sq(-10, 20, 30, 10)], [sq(80, 60, 30, 10)])
    overlay_case(P, 'cut: multipart tool', 'cut-with-polygon', T(SQ), [two], want_area=10000 - 200 - 200)
    overlay_case(P, 'clip: multipart tool', 'clip-with-polygon', T(SQ), [two], want_area=400, want_parts=2)
    # an upside-down U: its two legs cut the square into three disjoint pieces
    fork = T([(30, -10), (40, -10), (40, 110), (60, 110), (60, -10), (70, -10), (70, 120), (30, 120)])
    overlay_case(P, 'cut: three pieces', 'cut-with-polygon', T(SQ), [fork], want_area=10000 - 2000, want_parts=3)
    overlay_case(P, 'clip: two legs', 'clip-with-polygon', T(SQ), [fork], want_area=2000, want_parts=2)
    # a tool that leaves pieces touching at a single point
    bow = T([(50, -10), (110, 50), (50, 110), (-10, 50)])  # a diamond whose corners stick out
    x = overlay_case(P, 'cut: four corner triangles', 'cut-with-polygon', T(SQ), [bow])
    if x['err'] is None and x['after']:
        P.expect('cut: four corner triangles', nparts(x['after'][0]['geometry']) == 4, 'parts %d' % nparts(x['after'][0]['geometry']))
    P.done()


def test_cut_in_active_layer_touching_targets_untouched():
    P = Problems()
    data = write_layer('lay', [feature(T(sq(0, 0, 100)), name='a'), feature(T(sq(100, 0, 100)), name='b'),
                               feature(T(sq(100, 100, 100)), name='c'),
                               feature(T(sq(200, 0, 50)), name='tool')])
    # the tool touches b along an edge and c at a corner only: nothing overlaps
    with Session(data) as s:
        before = s.features('lay')
        s.activate('lay')
        s.select('lay', [3])
        for cmd in ('cut-with-polygon', 'clip-with-polygon'):
            try:
                r = s.plugin(cmd, {'keepTools': True})
                P.add(cmd, 'succeeded on touching neighbours: %s' % r)
                s.undo()
            except CommandError as e:
                P.expect(cmd, bool(str(e).strip()), 'empty message')
            P.expect(cmd, fingerprint(s.features('lay')) == fingerprint(before), 'touching neighbours changed')
    P.done()


def test_cut_keeps_adjacent_targets_topology():
    """Two adjacent targets cut by one tool across their shared edge: the new boundary vertices
    on the shared edge must be the same in both pieces (no gap, no overlap)."""
    P = Problems()
    tool = T([(50, 30), (150, 37), (150, 70), (50, 63)])
    x = run_two_layers('cut-with-polygon', [T(sq(0, 0, 100)), T(sq(100, 0, 100))], [tool], {'keepTools': True})
    if P.expect('adjacent', x['err'] is None, 'refused: %s' % short(x['err'])):
        r = geom_report([f['geometry'] for f in x['after']] + [tool], tool)
        P.expect('adjacent', all(r['valid']), r['why'])
        ov = [o for o in r['overlaps'] if abs(o[2]) > 1e-6]
        P.expect('adjacent', not ov, 'overlaps: %s' % ov)
        P.expect('adjacent', abs(r['union'] - 20000) < 1e-3, 'pieces + tool cover %.6f, not 20000' % r['union'])
    P.done()


# ---------- rings ----------

def ring_case(P, label, command, target, tools, want_holes=None, want_area=None, refuse=False):
    x = run_two_layers(command, [target], tools, {'keepTools': True})
    if x['err'] is not None:
        P.expect(label, refuse, 'refused: %s' % short(x['err']))
        P.expect(label, fingerprint(x['after']) == fingerprint(x['before']), 'a refused command changed the data')
        return x
    P.expect(label, not refuse, 'expected a refusal: %s' % x['r'])
    g = x['after'][0]['geometry']
    r = geom_report([g], target)
    P.expect(label, r['valid'][0], 'invalid: %s' % r['why'][0])
    if want_holes is not None:
        P.expect(label, nholes(g) == want_holes, '%d holes, expected %d' % (nholes(g), want_holes))
    if want_area is not None:
        P.expect(label, abs(r['area'][0] - want_area) <= 1e-4, 'area %.6f, expected %.6f' % (r['area'][0], want_area))
    return x


def test_add_ring_touching_and_nested():
    P = Problems()
    # touching the shell or a hole at single points is a valid hole (OGC)
    ring_case(P, 'touches the shell at a point', 'add-ring', T(SQ), [T([(0, 50), (30, 40), (30, 60)])],
              want_holes=1, want_area=10000 - 300)
    ring_case(P, 'touches the shell and a hole', 'add-ring', T(SQ, HOLE), [T([(0, 20), (30, 30), (10, 30)])],
              want_holes=2, want_area=10000 - 1600 - 100)
    # inside an existing hole: not inside the polygon at all
    ring_case(P, 'inside an existing hole', 'add-ring', T(SQ, HOLE), [T(sq(45, 45, 10))], refuse=True)
    # around an existing hole: the result must stay valid (no hole inside a hole)
    ring_case(P, 'around an existing hole', 'add-ring', T(SQ, HOLE), [T(sq(20, 20, 60))], refuse=True)
    ring_case(P, 'touches a hole corner', 'add-ring', T(SQ, HOLE), [T(sq(10, 10, 20))], want_holes=2,
              want_area=10000 - 1600 - 400)
    # still skipped: along the shell, across it, or overlapping a hole
    ring_case(P, 'shares an edge with the shell', 'add-ring', T(SQ), [T(sq(0, 10, 20))], refuse=True)
    ring_case(P, 'crosses the shell', 'add-ring', T(SQ), [T(sq(-10, 10, 20))], refuse=True)
    ring_case(P, 'shares an edge with a hole', 'add-ring', T(SQ, HOLE), [T(sq(10, 40, 20))], refuse=True)
    ring_case(P, 'overlaps a hole', 'add-ring', T(SQ, HOLE), [T(sq(20, 40, 20))], refuse=True)
    # touching the shell at two points cuts the polygon in two
    ring_case(P, 'cuts the polygon in two', 'add-ring', T(SQ), [T([(0, 50), (50, 40), (100, 50), (50, 60)])],
              refuse=True)
    # two tools that overlap each other
    ring_case(P, 'two overlapping tools', 'add-ring', T(SQ), [T(sq(10, 10, 20)), T(sq(20, 20, 20))])
    P.done()


def test_add_ring_inside_a_multipart_part():
    P = Problems()
    mp = mpoly(O, [sq(0, 0, 100)], [sq(200, 0, 100)])
    ring_case(P, 'hole in the second part', 'add-ring', mp, [T(sq(240, 40, 20))], want_holes=1,
              want_area=20000 - 400)
    P.done()


def parts_case(P, label, command, geom, want_parts, want_area=None, want_features=1):
    data = write_layer('mp', [feature(geom, name='m', n=1)])
    with Session(data) as s:
        before = s.features('mp')
        s.activate('mp')
        s.select('mp', [0])
        try:
            r = s.plugin(command)
        except CommandError as e:
            P.add(label, 'refused: %s' % short(e))
            P.expect(label, fingerprint(s.features('mp')) == fingerprint(before), 'refusal changed data')
            return
        after = s.features('mp')
        P.expect(label, len(after) == want_features, '%d features, expected %d' % (len(after), want_features))
        g = after[0]['geometry']
        P.expect(label, nparts(g) == want_parts, '%d parts, expected %d' % (nparts(g), want_parts))
        if want_area is not None:
            a = area_of(g)
            P.expect(label, abs(a - want_area) < 1e-3, 'area %.4f, expected %.4f' % (a, want_area))
        s.undo()
        P.expect(label, fingerprint(s.features('mp')) == fingerprint(before), 'undo')


def test_delete_and_split_off_equal_parts():
    P = Problems()
    eq = mpoly(O, [sq(0, 0, 50)], [sq(100, 0, 50)], [sq(200, 0, 50)])
    parts_case(P, 'delete-part: three equal parts', 'delete-part', eq, 1, 2500)
    parts_case(P, 'split-off-part: three equal parts', 'split-off-part', eq, 1, 2500, want_features=3)
    # parts touching at a corner (a valid multipolygon)
    touch = mpoly(O, [sq(0, 0, 50)], [sq(50, 50, 30)])
    parts_case(P, 'delete-part: corner-touching parts', 'delete-part', touch, 1, 2500)
    P.done()


# ---------- fill gap ----------

def cells(skip=(), size=100):
    return [T(sq(size * i, size * j, size)) for j in range(3) for i in range(3) if (i, j) != (1, 1) and (i, j) not in skip]


def gap_case(P, label, geoms, want_created, want_area=None):
    data = write_layer('cells', [feature(g, name='c%d' % i) for i, g in enumerate(geoms)])
    with Session(data) as s:
        before = s.features('cells')
        s.activate('cells')
        s.select('cells', range(len(geoms)))
        try:
            r = s.plugin('fill-gap')
        except CommandError as e:
            P.expect(label, want_created == 0, 'refused: %s' % short(e))
            P.expect(label, fingerprint(s.features('cells')) == fingerprint(before), 'refusal changed data')
            return
        after = s.features('cells')
        new = after[len(before):]
        P.expect(label, len(new) == want_created, '%d gaps filled, expected %d (%s)' % (len(new), want_created, r))
        P.expect(label, fingerprint(after[:len(before)]) == fingerprint(before), 'the polygons around changed')
        if new and want_area is not None:
            r2 = geom_report([f['geometry'] for f in new] + [f['geometry'] for f in before], geoms[0])
            P.expect(label, all(r2['valid']), 'invalid gap polygon: %s' % r2['why'][:len(new)])
            got = sorted(r2['area'][:len(new)])
            P.expect(label, all(abs(a - w) < 1e-3 for a, w in zip(got, sorted(want_area))),
                     'gap areas %s, expected %s' % (got, want_area))
            ov = [o for o in r2['overlaps'] if abs(o[2]) > 1e-6]
            P.expect(label, not ov, 'the gap overlaps its neighbours: %s' % ov)


def test_fill_gap_touching_outside_at_a_point():
    P = Problems()
    # ring of 8 cells minus the NE corner: the gap touches the outside at the point (200, 200)
    gap_case(P, 'gap touching the outside at a point', cells(skip=[(2, 2)]), 1, [10000])
    # and a gap whose own corners touch: two diagonal gaps meeting at a point
    g = [T(sq(0, 0, 300, 100)), T(sq(0, 200, 300, 100)), T(sq(0, 100, 100)), T(sq(200, 100, 100)),
         T([(100, 100), (150, 100), (150, 150), (100, 150)]), T([(150, 150), (200, 150), (200, 200), (150, 200)])]
    gap_case(P, 'two gaps meeting at a point', g, 2, [2500, 2500])
    P.done()


def test_fill_gap_slivers_and_thin_gaps():
    P = Problems()

    def slot(w):
        return [T(sq(0, 0, 100)), T(sq(100 + w, 0, 100)), T(sq(0, -10, 200 + w, 10)), T(sq(0, 100, 200 + w, 10))]
    gap_case(P, '5 mm wide slot (0.5 m²)', slot(0.005), 0)
    gap_case(P, '2 cm wide slot (2 m²)', slot(0.02), 1, [2.0])
    gap_case(P, 'zero-width slot', slot(0.0), 0)
    # a gap enclosed by a single multipolygon's parts and another feature
    P.done()


def test_fill_gap_inside_a_hole_with_an_island():
    """A hole of one polygon that holds an island polygon: the ring between them is a gap
    enclosed by two features."""
    P = Problems()
    g = [T(SQ, HOLE), T(sq(40, 40, 20))]
    gap_case(P, 'ring between hole and island', g, 1, [1600 - 400])
    P.done()


# ---------- collinear and nearly collinear edges ----------

def lin(*rings):
    """A polygon built linearly in lon/lat (see test_edge_lines.lonlat_linear): its edges are
    within a fraction of a millimetre of those of the same shape built in the frame."""
    from test_edge_lines import lonlat_linear
    return {'type': 'Polygon', 'coordinates': [lonlat_linear(O, closed(r)) for r in rings]}


def test_cut_clip_collinear_edges():
    P = Problems()
    tool = T(sq(50, 0, 100, 100))
    overlay_case(P, 'cut: shares two edges', 'cut-with-polygon', T(SQ), [tool], want_area=5000, want_parts=1)
    overlay_case(P, 'clip: shares two edges', 'clip-with-polygon', T(SQ), [tool], want_area=5000, want_parts=1)
    tool2 = T(sq(50, -10, 100, 50))
    overlay_case(P, 'cut: shares part of the right edge', 'cut-with-polygon', T(SQ), [T(sq(100, 20, 50, 50)), tool2],
                 want_area=10000 - 50 * 40, want_parts=1)
    P.done()


def test_cut_clip_nearly_collinear_edges():
    P = Problems()
    tgt = lin(SQ)
    for label, cmd, want in (('cut', 'cut-with-polygon', 5000), ('clip', 'clip-with-polygon', 5000)):
        x = overlay_case(P, '%s: sub-mm collinear' % label, cmd, tgt, [lin(sq(50, 0, 100, 100))])
        if x['err'] is None and x['after']:
            g = x['after'][0]['geometry']
            r = shapely_job(geoms=[gmap(g, metric(g))], orig=None)
            P.expect('%s: sub-mm collinear' % label, nparts(g) == 1 and abs(r['area'][0] - 5000) < 1,
                     '%d parts, area %.6f (slivers along the shared edges)' % (nparts(g), r['area'][0]))
            if g['type'] == 'MultiPolygon':
                from test_edge_lines import shapely_job as sj
                parts = [{'type': 'Polygon', 'coordinates': p} for p in g['coordinates']]
                rr = sj(geoms=[gmap(p, metric(g)) for p in parts])
                P.add('%s: sub-mm collinear' % label, 'part areas %s' % rr['area'])
    P.done()


def test_add_part_touching_edge_merges():
    P = Problems()
    x = run_two_layers('add-part', [T(SQ)], [T(sq(100, 0, 50))], {'keepTools': True})
    if P.expect('touching edge', x['err'] is None, 'refused: %s' % short(x['err'])):
        g = x['after'][0]['geometry']
        r = shapely_job(geoms=[gmap(g, metric(g))])
        P.expect('touching edge', nparts(g) == 1 and r['valid'][0] and abs(r['area'][0] - 12500) < 1e-3,
                 '%d parts, valid %s, area %.4f' % (nparts(g), r['valid'][0], r['area'][0]))
    x = run_two_layers('add-part', [lin(SQ)], [lin(sq(100, 0, 50))], {'keepTools': True})
    if P.expect('touching edge sub-mm', x['err'] is None, 'refused: %s' % short(x['err'])):
        g = x['after'][0]['geometry']
        r = shapely_job(geoms=[gmap(g, metric(g))])
        P.expect('touching edge sub-mm', r['valid'][0], 'invalid: %s' % r['why'][0])
    P.done()
