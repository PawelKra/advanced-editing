"""Attributes of features made from another: every creating command copies the fields except
the identifiers `id`, `fid` and `ogc_fid` (any case)."""

from aelib import (PL, PREFIX, Session, check, feature, linestring, multipolygon, polygon, rect,
                   write_layer)

LON, LAT = PL
KEYS = {'id': 7, 'fid': 8, 'OGC_FID': 9}
KEPT = {'name': 'a', 'n': 3}


def props():
    return dict(KEYS, **KEPT)


def new_features(s, layer, before):
    """The features of `layer` added since `before` (feature ids not seen before)."""
    old = {f['id'] for f in before}
    return [f for f in s.features(layer) if f['id'] not in old]


def check_inherited(command, feats, count=None):
    check(feats and (count is None or len(feats) == count), '%s created %d feature(s)' % (command, len(feats)))
    for f in feats:
        p = f['properties']
        for k in KEYS:
            check(p.get(k) is None, '%s copied %s: %s' % (command, k, p))
        for k, v in KEPT.items():
            check(p.get(k) == v, '%s lost %s: %s' % (command, k, p))


def run(data, active, rows, command, params=None, others=()):
    """Selects `others` ((layer, rows), …) and `rows` of the active layer, runs `command`,
    returns the features it added to `active` or, with `others`, to their first layer."""
    with Session(*data) as s:
        target = others[0][0] if others else active
        before = s.features(target)
        for name, sel in others:
            s.select(name, sel, add=True)
        s.activate(active)
        s.select(active, rows, add=bool(others))
        s.plugin(command, params or {})
        return new_features(s, target, before)


def square_layer(name='parcels'):
    return write_layer(name, [feature(polygon(rect(LON, LAT, 100, 100)), **props())])


def test_the_layer_has_the_identifier_fields():
    with Session(square_layer()) as s:
        names = {f['name'] for f in s.layers()['parcels']['fields']}
        check(names >= set(KEYS) | set(KEPT), names)
        check(s.features('parcels')[0]['properties'] == props(), s.features('parcels'))


def test_polygon_commands_leave_out_identifiers():
    holed = polygon(rect(LON, LAT, 100, 100), list(reversed(rect(LON, LAT, 40, 40, 30, 30))))
    feats = run([write_layer('holed', [feature(holed, **props())])], 'holed', [0], 'fill-ring')
    check_inherited('fill-ring', feats, 1)
    mp = multipolygon(polygon(rect(LON, LAT, 100, 100)), polygon(rect(LON, LAT, 30, 30, 200, 0)))
    feats = run([write_layer('multi', [feature(mp, **props())])], 'multi', [0], 'split-off-part')
    check_inherited('split-off-part', feats, 1)


def test_line_and_copy_commands_leave_out_identifiers():
    cut = write_layer('cut', [feature(linestring(LON, LAT, [(50, -10), (50, 110)]))])
    feats = run([square_layer(), cut], 'cut', [0], 'split-with-line', others=[('parcels', [0])])
    check_inherited('split-with-line', feats, 1)
    road = write_layer('roads', [feature(linestring(LON, LAT, [(0, 0), (100, 0)]), **props())])
    feats = run([road], 'roads', [0], 'offset', {'distance': 5, 'move': False})
    check_inherited('offset', feats, 1)
    feats = run([square_layer()], 'parcels', [0], 'copy-move', {'distance': 25, 'bearing': 90, 'move': False})
    check_inherited('copy-move', feats, 1)


def test_division_leaves_out_identifiers():
    feats = run([square_layer()], 'parcels', [0], 'divide-equal', {'parts': 3})
    check_inherited('divide-equal', feats, 2)
    feats = run([square_layer()], 'parcels', [0], 'divide-percent', {'percent': 30})
    check_inherited('divide-percent', feats, 1)
    with Session(square_layer()) as s:
        s.run('gp.run', {'tool': PREFIX + 'divide-parcel',
                         'params': {'input': 'parcels', 'mode': 'Equal parts', 'count': 2}})
        out = s.features('parcels divided')
        check_inherited('divide-parcel', out, 2)
        check(sorted(f['properties']['part'] for f in out) == [1, 2], out)
