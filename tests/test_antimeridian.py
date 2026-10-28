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
