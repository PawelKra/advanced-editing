"""Fill Ring with islands: the new polygon fills only the free part of the hole.

Areas and overlaps are checked with shapely in the plugin's metric frame (see test_edge_lines.py)."""

from aelib import Session, check, feature, write_layer
from test_edge_lines import O, SQ, cw, fingerprint, flat, gmap, metric, mpoly, poly, shapely_job, sq
from test_limits import blob

HOLE = cw(sq(20, 20, 60))
FREE = 3600.0


def rings(g):
    polys = [g['coordinates']] if g['type'] == 'Polygon' else g['coordinates']
    return [r for p in polys for r in p]


def vertex_set(*geoms):
    return {tuple(c) for g in geoms for c in flat(g)}


def fill(feats, select=(0,), params=None, locators=None):
    """Runs fill-ring on layer 'parcels' (rows `select` selected), with `locators` (geometries)
    selected in layer 'loc'. Returns (answer, before, after, features after undo)."""
    data = [write_layer('parcels', [feature(g, name='p%d' % i) for i, g in enumerate(feats)])]
    if locators:
        data.append(write_layer('loc', [feature(g) for g in locators]))
    with Session(*data) as s:
        before = s.features('parcels')
        if locators:
            s.select('loc', range(len(locators)))
        s.activate('parcels')
        s.select('parcels', list(select), add=bool(locators))
        r = s.plugin('fill-ring', params or {})
        after = s.features('parcels')
        s.undo()
        return r, before, after, s.features('parcels')


def check_fill(r, before, after, restored, want_areas, want_holes=None, want_parts=None):
    """The new features (after the old ones) have `want_areas`, overlap nothing, share every
    vertex bit-exactly with the parcels read before, and undo restores the layer."""
    new = after[len(before):]
    check(r['created'] == len(want_areas) == len(new), (r, len(new)))
    check(fingerprint(after[:len(before)]) == fingerprint(before), 'the parcels changed')
    m = metric(before[0]['geometry'])
    geoms = [f['geometry'] for f in after]
    rep = shapely_job(geoms=[gmap(g, m) for g in geoms])
    check(all(rep['valid']), rep['why'])
    got = sorted(rep['area'][len(before):])
    check(all(abs(a - b) < 1e-4 for a, b in zip(got, sorted(want_areas))), (got, want_areas))
    ov = [o for o in rep['overlaps'] if abs(o[2]) > 1e-6]
    check(not ov, 'overlaps: %s' % ov)
    old = vertex_set(*[f['geometry'] for f in before])
    for f in new:
        g = f['geometry']
        check(vertex_set(g) <= old, 'new vertices: %s' % (vertex_set(g) - old))
        check(f['properties']['name'] == 'p0', f['properties'])
        if want_holes is not None:
            check(len(rings(g)) - (len(g['coordinates']) if g['type'] == 'MultiPolygon' else 1) == want_holes, g)
        if want_parts is not None:
            check((len(g['coordinates']) if g['type'] == 'MultiPolygon' else 1) == want_parts, g)
    check(fingerprint(restored) == fingerprint(before), 'undo did not restore')


def test_island_part_of_the_same_feature():
    r, before, after, restored = fill([mpoly(O, [SQ, HOLE], [sq(40, 40, 20)])])
    check_fill(r, before, after, restored, [FREE - 400], want_holes=1)
    check(r['islands'] == 1 and '1 island(s) left out' in r['message'], r)
    # the outer ring is the hole's, the hole the island's
    new = after[-1]['geometry']
    parent = before[0]['geometry']['coordinates']
    check(set(map(tuple, rings(new)[0])) == set(map(tuple, parent[0][1])), 'outer ring is not the hole')
    check(set(map(tuple, rings(new)[1])) == set(map(tuple, parent[1][0])), 'hole is not the island')


def test_island_of_another_feature():
    r, before, after, restored = fill([poly(O, SQ, HOLE), poly(O, sq(30, 50, 20, 10))])
    check_fill(r, before, after, restored, [FREE - 200], want_holes=1)


def test_island_touching_the_hole_at_a_point():
    # a diamond whose bottom vertex lies on the hole's bottom edge
    diamond = [(50, 20), (60, 30), (50, 40), (40, 30)]
    r, before, after, restored = fill([poly(O, SQ, HOLE), poly(O, diamond)])
    check_fill(r, before, after, restored, [FREE - 200])
    # and one touching a corner of the hole
    r, before, after, restored = fill([poly(O, SQ, HOLE), poly(O, sq(20, 20, 10))])
    check_fill(r, before, after, restored, [FREE - 100])
    # two islands touching each other at a corner
    r, before, after, restored = fill([mpoly(O, [SQ, HOLE], [sq(30, 30, 10)]), poly(O, sq(40, 40, 10))])
    check_fill(r, before, after, restored, [FREE - 200])
    # an island touching the hole's ring at four points cuts the free part in four
    r, before, after, restored = fill([poly(O, SQ, HOLE), poly(O, [(20, 50), (50, 20), (80, 50), (50, 80)])])
    check_fill(r, before, after, restored, [FREE - 1800], want_parts=4)


def test_island_splitting_the_hole():
    # a band from side to side leaves two pieces: one multi-part feature
    r, before, after, restored = fill([poly(O, SQ, HOLE), poly(O, sq(20, 45, 60, 10))])
    check_fill(r, before, after, restored, [FREE - 600], want_parts=2)


def test_two_islands():
    r, before, after, restored = fill([mpoly(O, [SQ, HOLE], [sq(25, 25, 10)]), poly(O, sq(60, 60, 10))])
    check_fill(r, before, after, restored, [FREE - 200], want_holes=2)
    check(r['islands'] == 2, r)


def test_nested_islands():
    # an island with its own hole holding another island: both holes are filled around their islands
    outer_island = [sq(30, 30, 40), cw(sq(40, 40, 20))]
    r, before, after, restored = fill([mpoly(O, [SQ, HOLE], outer_island, [sq(45, 45, 10)])])
    check_fill(r, before, after, restored, [FREE - 1600, 400 - 100], want_holes=1)
    check(r['created'] == 2 and r['islands'] == 3, r)


def test_islands_covering_the_hole():
    data = write_layer('parcels', [feature(poly(O, SQ, HOLE), name='p0'), feature(poly(O, sq(20, 20, 60)), name='f'),
                                   feature(poly(O, [(x + 200, y) for x, y in SQ], [(x + 200, y) for x, y in HOLE]),
                                           name='p2')])
    with Session(data) as s:
        before = s.features('parcels')
        s.activate('parcels')
        s.select('parcels', [0])
        e = s.error_of('fill-ring')
        check('islands cover each of the 1 hole(s) entirely' in e, e)
        check(fingerprint(s.features('parcels')) == fingerprint(before), 'a refused command changed the data')
        # with another hole that is free, that one is filled and the covered one reported
        s.select('parcels', [0, 2])
        r = s.plugin('fill-ring')
        check(r['created'] == 1 and r['covered'] == 1 and '1 hole(s) not filled: islands cover them entirely'
              in r['message'], r)
        check(vertex_set(s.features('parcels')[-1]['geometry']) <= vertex_set(before[2]['geometry']), 'wrong hole')


def test_no_islands_copies_the_ring():
    r, before, after, restored = fill([poly(O, SQ, HOLE), poly(O, sq(200, 0, 50))])
    check_fill(r, before, after, restored, [FREE], want_holes=0)
    check(r['islands'] == 0 and 'island' not in r['message'], r)
    hole = before[0]['geometry']['coordinates'][1]
    new = after[-1]['geometry']
    check(new['type'] == 'Polygon' and len(new['coordinates']) == 1, new)
    ring = new['coordinates'][0]
    check(ring == hole or ring == hole[::-1], 'not the hole ring vertex for vertex: %s vs %s' % (ring, hole))


def test_locator_picks_one_of_two_holes():
    h1, h2 = cw(sq(10, 10, 30)), cw(sq(60, 60, 30))
    g = mpoly(O, [SQ, h1, h2], [sq(20, 20, 10)], [sq(70, 70, 5)])
    locator = poly(O, [(12, 12), (15, 12), (15, 15)])
    r, before, after, restored = fill([g], locators=[locator])
    check_fill(r, before, after, restored, [900 - 100], want_holes=1)


def test_layer_too_large_still_subtracts_own_parts():
    # A far-away polygon of ~1.4 MB: the layer is not read, so only the selected features are
    # searched for islands. The fill then overlaps the unselected 5 x 5 polygon in the hole:
    # that is the documented behaviour, and the message must say so plainly.
    big = {'type': 'Polygon', 'coordinates': [blob(5000, 0, 100, 35000)]}
    feats = [mpoly(O, [SQ, HOLE], [sq(40, 40, 20)]), poly(O, sq(25, 25, 5)), big]
    r, before, after, restored = fill(feats)
    check(r['created'] == 1 and r['islands'] == 1 and not r['layerChecked'], r)
    check("the layer is too large to check its unselected polygons for islands: the new polygon may overlap "
          "unselected polygons of 'parcels' lying in the hole. Select those polygons too, or undo" in r['message'],
          r['message'])
    check(fingerprint(restored) == fingerprint(before), 'undo did not restore')
    m = metric(before[0]['geometry'])
    a = shapely_job(geoms=[gmap(after[-1]['geometry'], m)])['area'][0]
    check(abs(a - (FREE - 400)) < 1e-4, a)
    # selecting the small polygon too keeps it out, as the message advises
    r, before, after, restored = fill(feats, select=(0, 1))
    check(r['created'] == 1 and r['islands'] == 2 and not r['layerChecked'], r)
    check_fill(r, before, after, restored, [FREE - 425], want_holes=2)
