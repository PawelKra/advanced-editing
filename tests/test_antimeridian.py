"""Features at and across the antimeridian: the frame takes longitudes the short way round
(179.9° to -179.9° spans 0.2°), results come back within [-180, 180], and vertices that are
kept stay bit-exact."""

from aelib import Session, check, feature, write_layer
from test_division import area_ell, tol

LAT = -17.0


def ring(lon0, lon1, lat0=LAT, lat1=LAT + 0.1):
    return [[lon0, lat0], [lon1, lat0], [lon1, lat1], [lon0, lat1], [lon0, lat0]]


def across(lon=179.9, h=0.1):
    """From `lon` east to -`lon`: 0.2° wide by default."""
    return {'type': 'Polygon', 'coordinates': [ring(lon, -lon, LAT, LAT + h)]}


def unwrapped(g):
    """The geometry with longitudes on [0, 360), to measure it in one piece."""
    def rec(c):
        if c and isinstance(c[0], (int, float)):
            return [c[0] + 360.0 if c[0] < 0 else c[0], c[1]]
        return [rec(x) for x in c]
    return {'type': g['type'], 'coordinates': rec(g['coordinates'])}


def positions(g):
    def rec(c):
        if c and isinstance(c[0], (int, float)):
            yield tuple(c)
        else:
            for x in c:
                yield from rec(x)
    return list(rec(g['coordinates']))


def check_pieces(before, pieces, parts):
    """`pieces` cover `before` (by ellipsoidal area), have longitudes within [-180, 180] and
    keep every original corner bit-exact."""
    check(len(pieces) == parts, '%d pieces' % len(pieces))
    lons = [p[0] for g in pieces for p in positions(g)]
    check(all(-180.0 <= x <= 180.0 for x in lons), 'longitudes outside [-180, 180]: %s' % [x for x in lons if abs(x) > 180][:3])
    total = sum(area_ell(unwrapped(g)) for g in pieces)
    whole = sum(area_ell(unwrapped(g)) for g in before)
    check(abs(total - whole) <= tol(whole), 'pieces %.3f m², original %.3f m²' % (total, whole))
    out = set(p for g in pieces for p in positions(g))
    check(all(p in out for g in before for p in positions(g)), 'an original corner moved')
    return total


def test_split_across_the_antimeridian():
    # 2 km: on a 20 km polygon the boolean ops' integer grid moves corners beyond the snapping
    # distance, wherever it lies
    pl = write_layer('am', [feature(across(179.99, 0.01), name='a')])
    # a line from 179.985°E to 179.985°W, itself across
    tl = write_layer('amt', [feature({'type': 'LineString', 'coordinates': [[179.985, LAT + 0.002], [-179.985, LAT + 0.007]]}, name='t')])
    with Session(pl, tl) as s:
        before = s.features('am')
        s.select('am', [0])
        s.activate('amt')
        s.select('amt', [0], add=True)
        r = s.plugin('split-with-line', {'keepTools': True})
        after = s.features('am')
    check(r['split'] == 1 and r['created'] == 1, r)
    check_pieces([before[0]['geometry']], [f['geometry'] for f in after], 2)
    # the new vertices on the west edge are west of the antimeridian, on the east edge east
    new = [p for f in after for p in positions(f['geometry']) if p not in positions(before[0]['geometry'])]
    check(sorted(round(p[0], 7) for p in set(new)) == [-179.99, 179.99], new)


def test_divide_across_the_antimeridian():
    pl = write_layer('am', [feature(across(), name='a')])
    with Session(pl) as s:
        before = s.features('am')
        s.activate('am')
        s.select('am', [0])
        r = s.plugin('divide-equal', {'parts': 4, 'angle': 0})
        after = s.features('am')
    check(r['divided'] == 1, r)
    pieces = [f['geometry'] for f in after]
    total = check_pieces([before[0]['geometry']], pieces, 4)
    for g in pieces:
        check(abs(area_ell(unwrapped(g)) - total / 4) <= tol(total), 'unequal parts')
    # cut from north to south: the outer pieces on either side, the middle cut close to 180°
    sides = sorted((min(p[0] for p in positions(g)) > 0, max(p[0] for p in positions(g)) < 0) for g in pieces)
    check(sides == [(False, False), (False, False), (False, True), (True, False)], sides)
    lons = [p[0] for g in pieces for p in positions(g)]
    check(min(abs(abs(x) - 180) for x in lons) < 0.01, 'no cut near the antimeridian')


def test_divide_parcels_on_both_sides_of_the_antimeridian():
    # one parcel just east, one just west: one frame for both, each divided where it is
    feats = [feature({'type': 'Polygon', 'coordinates': [ring(179.8, 179.95)]}, name='e'),
             feature({'type': 'Polygon', 'coordinates': [ring(-179.95, -179.8)]}, name='w')]
    pl = write_layer('am2', feats)
    with Session(pl) as s:
        before = s.features('am2')
        s.activate('am2')
        s.select('am2', [0, 1])
        r = s.plugin('divide-equal', {'parts': 2, 'angle': 0})
        after = s.features('am2')
    check(r['divided'] == 2, r)
    for name, sign in (('e', 1), ('w', -1)):
        pieces = [f['geometry'] for f in after if f['properties']['name'] == name]
        orig = [f['geometry'] for f in before if f['properties']['name'] == name]
        check_pieces(orig, pieces, 2)
        check(all(p[0] * sign > 0 for g in pieces for p in positions(g)), '%s: a piece crossed sides' % name)


def test_wide_polygon_across_the_antimeridian_is_refused():
    # 170°E to 170°W the short way is 20°; 165°E to 165°W is 30°
    ok = write_layer('amw', [feature({'type': 'Polygon', 'coordinates': [ring(165.0, -165.0, LAT, LAT + 1)]}, name='w')])
    with Session(ok) as s:
        before = s.features('amw')
        s.activate('amw')
        s.select('amw', [0])
        e = s.error_of('divide-equal', {'parts': 2, 'angle': 90})
        check('span 30.0° of longitude' in e, e)
        check(s.features('amw') == before, 'a refusal changed data')


def test_implicit_target_across_the_antimeridian():
    # the targets come from the active layer itself: a tool at 179.95°E finds the polygon
    # running from 179.9°E to 179.9°W although their lon/lat boxes don't meet
    tool = {'type': 'Polygon', 'coordinates': [ring(179.94, 179.96, LAT + 0.02, LAT + 0.04)]}
    far = {'type': 'Polygon', 'coordinates': [ring(-179.5, -179.4)]}
    pl = write_layer('amc', [feature(across(), name='a'), feature(far, name='far'), feature(tool, name='t')])
    with Session(pl) as s:
        before = s.features('amc')
        s.activate('amc')
        s.select('amc', [2])
        r = s.plugin('cut-with-polygon', {'keepTools': True})
        after = s.features('amc')
    check(r['changed'] == 1, r)
    by_name = {f['properties']['name']: f['geometry'] for f in after}
    old = {f['properties']['name']: f['geometry'] for f in before}
    check(by_name['far'] == old['far'] and by_name['t'] == old['t'], 'a feature away from the cut changed')
    cut = by_name['a']
    check(cut['type'] == 'Polygon' and len(cut['coordinates']) == 2, 'no hole cut: %s' % cut)
    want = area_ell(unwrapped(old['a'])) - area_ell(tool)
    got = area_ell(unwrapped(cut))
    check(abs(got - want) <= 1e-4 * want, 'area %.1f m², want %.1f m²' % (got, want))


def test_implicit_target_away_from_the_tool_is_not_found():
    # a tool just west of the polygon across the antimeridian, and one far east of it
    for lon0, lon1 in ((179.8, 179.85), (-179.5, -179.45)):
        tool = {'type': 'Polygon', 'coordinates': [ring(lon0, lon1, LAT + 0.02, LAT + 0.04)]}
        pl = write_layer('amn', [feature(across(), name='a'), feature(tool, name='t')])
        with Session(pl) as s:
            before = s.features('amn')
            s.activate('amn')
            s.select('amn', [1])
            e = s.error_of('cut-with-polygon', {'keepTools': True})
            check("none of the polygons of 'amn' touch the selected tool features" in e, e)
            check(s.features('amn') == before, 'a refusal changed data')


def dateline_parts():
    """A: 0.001° by 1° across the antimeridian; B: 0.1° by 0.02° across it further north,
    twice the area of A although its lon/lat box is the narrower one."""
    a = ring(179.9995, -179.9995, LAT, LAT + 1)
    b = ring(179.95, -179.95, LAT + 2, LAT + 2.02)
    return a, b


def test_parts_without_locators_keep_the_largest_across_the_antimeridian():
    a, b = dateline_parts()
    mp = {'type': 'MultiPolygon', 'coordinates': [[a], [b]]}
    for command in ('delete-part', 'split-off-part'):
        with Session(write_layer('amp', [feature(mp, name='m')])) as s:
            s.activate('amp')
            s.select('amp', [0])
            r = s.plugin(command)
            after = s.features('amp')
        check(r.get('deleted', r.get('created')) == 1, (command, r))
        kept = after[0]['geometry']
        check(kept['type'] == 'Polygon' and set(positions(kept)) == set(map(tuple, b)), (command, kept))
        if command == 'split-off-part':
            check(len(after) == 2 and set(positions(after[1]['geometry'])) == set(map(tuple, a)), after)


def test_parts_without_locators_keep_the_largest_when_no_frame_fits_the_feature():
    # A: 1° by 1° across the antimeridian at the equator, about 12 400 km². B: 2° by 1° at
    # 80°N, about 4 100 km² in its own frame; C: 10° by 1° at 85°N, beyond any frame, about
    # 9 700 km² on the ellipsoid. In square degrees B and C would be the larger.
    a = ring(179.5, -179.5, -0.5, 0.5)
    for other in (ring(10.0, 12.0, 80.0, 81.0), ring(10.0, 20.0, 85.0, 86.0)):
        mp = {'type': 'MultiPolygon', 'coordinates': [[other], [a]]}
        for command in ('delete-part', 'split-off-part'):
            with Session(write_layer('amp', [feature(mp, name='m')])) as s:
                s.activate('amp')
                s.select('amp', [0])
                r = s.plugin(command)
                after = s.features('amp')
            check(r.get('deleted', r.get('created')) == 1, (command, r))
            kept = after[0]['geometry']
            check(kept['type'] == 'Polygon' and set(positions(kept)) == set(map(tuple, a)), (command, kept))
            if command == 'split-off-part':
                check(len(after) == 2 and set(positions(after[1]['geometry'])) == set(map(tuple, other)), after)


def point_at(lon, lat):
    return {'type': 'Point', 'coordinates': [lon, lat]}


def test_part_locators_across_the_antimeridian():
    # A across the antimeridian, C a plain part west of it; locators just east and just west
    # of 180° pick A, one near 0° (inside A's lon/lat box, far from it) picks nothing
    a, _ = dateline_parts()
    c = ring(179.0, 179.1)
    mp = {'type': 'MultiPolygon', 'coordinates': [[a], [c]]}
    locs = write_layer('amloc', [feature(point_at(-179.9998, LAT + 0.5), k=1),
                                 feature(point_at(179.9998, LAT + 0.5), k=2),
                                 feature(point_at(0.0, LAT + 0.5), k=3)])
    for command in ('delete-part', 'split-off-part'):
        for row in (0, 1):
            with Session(write_layer('amp', [feature(mp, name='m')]), locs) as s:
                s.select('amloc', [row])
                s.activate('amp')
                s.select('amp', [0], add=True)
                r = s.plugin(command)
                after = s.features('amp')
            check(r.get('deleted', r.get('created')) == 1, (command, row, r))
            check(set(positions(after[0]['geometry'])) == set(map(tuple, c)), (command, row, after[0]))
        with Session(write_layer('amp', [feature(mp, name='m')]), locs) as s:
            before = s.features('amp')
            s.select('amloc', [2])
            s.activate('amp')
            s.select('amp', [0], add=True)
            e = s.error_of(command)
            check('none of the parts of the 1 selected polygon(s) touch' in e, (command, e))
            check(s.features('amp') == before, 'a refusal changed data')


def test_hole_locators_across_the_antimeridian():
    # a polygon from 179.5°E to 179.5°W with one hole across the antimeridian and one east of it
    across_hole = list(reversed(ring(179.99, -179.99, LAT + 0.4, LAT + 0.6)))
    east_hole = list(reversed(ring(179.6, 179.7, LAT + 0.4, LAT + 0.6)))
    holed = {'type': 'Polygon', 'coordinates': [ring(179.5, -179.5, LAT, LAT + 1), across_hole, east_hole]}
    locs = write_layer('amloc', [feature(point_at(-179.995, LAT + 0.5), k=1),
                                 feature(point_at(179.995, LAT + 0.5), k=2),
                                 feature(point_at(0.0, LAT + 0.5), k=3)])
    for row in (0, 1):
        with Session(write_layer('amh', [feature(holed, name='h')]), locs) as s:
            s.select('amloc', [row])
            s.activate('amh')
            s.select('amh', [0], add=True)
            r = s.plugin('delete-ring')
            g = s.features('amh')[0]['geometry']
        check(r['rings'] == 1, (row, r))
        check(len(g['coordinates']) == 2 and set(positions({'type': 'LineString', 'coordinates': g['coordinates'][1]}))
              == set(map(tuple, east_hole)), (row, g))
        with Session(write_layer('amh', [feature(holed, name='h')]), locs) as s:
            s.select('amloc', [row])
            s.activate('amh')
            s.select('amh', [0], add=True)
            r = s.plugin('fill-ring')
            after = s.features('amh')
        check(r['created'] == 1 and len(after) == 2, (row, r))
        want = area_ell(unwrapped({'type': 'Polygon', 'coordinates': [across_hole]}))
        got = area_ell(unwrapped(after[1]['geometry']))
        check(abs(got - want) <= 1e-6 * want, 'fill %.1f m², hole %.1f m²' % (got, want))
    for command in ('delete-ring', 'fill-ring'):
        with Session(write_layer('amh', [feature(holed, name='h')]), locs) as s:
            before = s.features('amh')
            s.select('amloc', [2])
            s.activate('amh')
            s.select('amh', [0], add=True)
            e = s.error_of(command)
            check('none of the holes of the 1 selected polygon(s) touch' in e, (command, e))
            check(s.features('amh') == before, 'a refusal changed data')


def test_fill_ring_islands_across_the_antimeridian():
    # a hole across the antimeridian with an island west of 180°, in a layer that also holds
    # 10 polygons near 0° with 28 000 vertices: taken raw, the hole's box spans the world and
    # each of them would be projected and measured as an island, more than the command can do
    from test_limits import blob
    across_hole = list(reversed(ring(179.99, -179.99, LAT + 0.4, LAT + 0.6)))
    holed = {'type': 'Polygon', 'coordinates': [ring(179.5, -179.5, LAT, LAT + 1), across_hole]}
    island = {'type': 'Polygon', 'coordinates': [ring(-179.998, -179.992, LAT + 0.45, LAT + 0.55)]}
    far = [feature({'type': 'Polygon', 'coordinates': [blob(300 * i, 0, 100, 2800, (0.5, LAT + 0.5))]}, name='far')
           for i in range(10)]
    with Session(write_layer('amh', [feature(holed, name='h'), feature(island, name='i')] + far)) as s:
        s.activate('amh')
        s.select('amh', [0])
        r = s.plugin('fill-ring')
        after = s.features('amh')
    check(r['created'] == 1 and r['islands'] == 1 and r['layerChecked'], r)
    want = area_ell(unwrapped({'type': 'Polygon', 'coordinates': [across_hole]})) - area_ell(unwrapped(island))
    got = area_ell(unwrapped(after[-1]['geometry']))
    check(abs(got - want) <= 1e-6 * want, 'fill %.1f m², hole less the island %.1f m²' % (got, want))
    lons = [p[0] for p in positions(after[-1]['geometry'])]
    check(all(-180.0 <= x <= 180.0 for x in lons), 'longitudes outside [-180, 180]')
