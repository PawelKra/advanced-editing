"""What shared boundaries cost on adversarial input: the snapping grid of many long cutter
segments, and topological editing of many neighbours along one edge. Each ends in a result or
a clean message, not in running out of fuel or memory."""

import time

from aelib import Session, check, feature, write_layer
from test_topo import line, poly, sq, split_all


def split_session(feats, tools, select):
    pl = write_layer('pl', feats)
    tl = write_layer('tl', tools)
    s = Session(pl, tl)
    s.select('pl', select)
    s.activate('tl')
    s.select('tl', range(len(tools)), add=True)
    return s


def test_coincident_long_cutters_split_once():
    # 8000 identical 100 km parts: one cut, however many copies the tool repeats
    part = line((-50_000, 50), (50_000, 50))['coordinates']
    tool = feature({'type': 'MultiLineString', 'coordinates': [part] * 8000}, name='t')
    feats = [feature(poly(sq(0, 0)), name='A'), feature(poly(sq(100, 0)), name='B')]
    t = time.time()
    with split_session(feats, [tool], [0, 1]) as s:
        r = s.plugin('split-with-line', {'keepTools': True})
        after = s.features('pl')
    check(r['split'] == 2 and r['created'] == 2, r)
    check(len(after) == 4, after)
    print('      %s (%.1fs)' % (r['message'], time.time() - t))


def test_dense_parallel_long_cutters_are_refused():
    # 6000 distinct 100 km parts 1 cm apart: every grid cell along them holds them all
    parts = [line((-50_000, 20 + 0.01 * i), (50_000, 20 + 0.01 * i))['coordinates'] for i in range(6000)]
    tool = feature({'type': 'MultiLineString', 'coordinates': parts}, name='t')
    feats = [feature(poly(sq(0, 0)), name='A')]
    with split_session(feats, [tool], [0]) as s:
        before = s.features('pl')
        e = s.error_of('split-with-line', {'keepTools': True})
        check('too many long segments close together' in e and 'Select fewer tool features' in e, e)
        check(s.features('pl') == before, 'a refused command changed the data')


def test_many_long_cutters_close_together_are_refused_before_indexing():
    # 400 parts of 100 km 1 cm apart: their index fits the memory cap, but building it three
    # times and searching it does not fit one command
    parts = [line((-50_000, 20 + 0.01 * i), (50_000, 20 + 0.01 * i))['coordinates'] for i in range(400)]
    tool = feature({'type': 'MultiLineString', 'coordinates': parts}, name='t')
    with split_session([feature(poly(sq(0, 0)), name='A')], [tool], [0]) as s:
        before = s.features('pl')
        e = s.error_of('split-with-line', {'keepTools': True})
        check('too many long segments close together' in e and 'ran too long' not in e, e)
        check(s.features('pl') == before, 'a refused command changed the data')


def comb(teeth, x=100.0, h=100.0):
    """A polygon east of x = `x` whose teeth reach 1 m across it: 2 * `teeth` crossings."""
    w = h / teeth
    pts = [(x + 5, 0.0)]
    for i in range(teeth):
        y0, y1 = (i + 0.25) * w, (i + 0.75) * w
        pts += [(x + 1, y0), (x - 1, y0), (x - 1, y1), (x + 1, y1)]
    pts += [(x + 5, h), (x + 5, 0.0)]
    return poly(ll_ring(pts))


def ll_ring(pts):
    return line(*pts)['coordinates']


def fan_out(teeth, neighbours, refused=False):
    """Cuts a comb out of A's east edge, which `neighbours` triangles share (the message of a
    refused cut instead of the result)."""
    tri = poly(ll_ring([(100, 0), (101, 50), (100, 100), (100, 0)]))
    feats = [feature(poly(sq(0, 0)), name='A')] + [feature(tri, name='n%d' % i) for i in range(neighbours)]
    t = time.time()
    with split_session(feats, [feature(comb(teeth), name='comb')], [0]) as s:
        before = s.features('pl')
        if refused:
            r = s.error_of('cut-with-polygon', {'keepTools': True})
        else:
            r = s.plugin('cut-with-polygon', {'keepTools': True})
        after = s.features('pl')
    return r, before, after, time.time() - t


def test_dense_shared_edge_fan_out_skips_the_layer():
    # 100 new vertices on one edge, 3000 neighbours along it: 300 000 inserts, where about
    # 3000 use up a command
    r, before, after, dt = fan_out(50, 3000)
    check(r['changed'] == 1 and r['sharedVertices'] == 0, r)
    check("shared vertices not added in 'pl': the layer is too large to search" in r['message'], r['message'])
    check([f['geometry'] for f in after[1:]] == [f['geometry'] for f in before[1:]], 'a neighbour changed')
    print('      %s (%.1fs)' % (r['message'], dt))


def test_a_modest_shared_edge_fan_out_is_shared():
    # 40 new vertices on one edge, 20 neighbours along it: 800 inserts fit
    r, before, after, _ = fan_out(20, 20)
    check(r['changed'] == 1 and r['sharedVertices'] == 800 and r['sharedFeatures'] == 20, r)
    check('not added' not in r['message'], r['message'])
    check(all(len(a['geometry']['coordinates'][0]) == 4 + 40 for a in after[1:]), 'a neighbour missed vertices')


def test_a_comb_too_costly_to_cut_is_refused_up_front():
    # 750 teeth: i_overlay compares every two of their edges, which overlap along x
    e, before, after, dt = fan_out(750, 0, refused=True)
    check('Cutting the targets with these polygons is more than one command can do' in e and 'ran too long' not in e, e)
    check(after == before, 'a refused command changed the data')
    print('      750 teeth refused in %.1fs' % dt)


def test_shared_vertices_fit_in_what_the_cut_left():
    # 400 teeth, 2 neighbours: the cut and its writes take most of the command, so the 1600
    # inserts are skipped; counting only the reading admitted them and ran out of budget
    r, before, after, dt = fan_out(400, 2)
    check(r['changed'] == 1 and r['sharedVertices'] == 0, r)
    check("shared vertices not added in 'pl'" in r['message'], r['message'])
    check([f['geometry'] for f in after[1:]] == [f['geometry'] for f in before[1:]], 'a neighbour changed')
    print('      %s (%.1fs)' % (r['message'], dt))
    # 200 teeth leave room for their 800 inserts
    r, _, after, _ = fan_out(200, 2)
    check(r['sharedVertices'] == 800 and 'not added' not in r['message'], r)
