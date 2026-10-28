"""Edge cases for Divide Into Equal Parts, Cut Off Percent and the Divide Parcel tool.

Areas are ellipsoidal (test_division.area_ell, independent of the plugin); validity and
overlaps come from shapely (see test_edge_lines.py)."""

import math

from aelib import ABROAD, PREFIX, CommandError, Session, check, feature, write_layer
from test_division import area_ell, check_cover, tol
from test_edge_lines import (HOLE, O, SQ, Problems, closed, cw, fingerprint, gmap, metric, mpoly, poly,
                             shapely_job, short, sq)

TOOL = PREFIX + 'divide-parcel'

COMB = [(0, 0), (100, 0), (100, 80), (90, 80), (90, 20), (70, 20), (70, 80), (60, 80), (60, 20), (40, 20),
        (40, 80), (30, 80), (30, 20), (10, 20), (10, 80), (0, 80)]
L_SHAPE = [(0, 0), (200, 0), (200, 80), (80, 80), (80, 240), (0, 240)]
U_SHAPE = [(0, 0), (300, 0), (300, 300), (200, 300), (200, 100), (100, 100), (100, 300), (0, 300)]
HOUSE = [(0, 0), (100, 0), (100, 100), (50, 150), (0, 100)]
DIAMOND = [(50, 0), (100, 50), (50, 100), (0, 50)]


def P_(*rings, origin=O):
    return poly(origin, *rings)


def valid_no_overlap(P, label, pieces, ref):
    m = metric(ref)
    r = shapely_job(geoms=[gmap(g, m) for g in pieces])
    bad = [(i, w) for i, (v, w) in enumerate(zip(r['valid'], r['why'])) if not v]
    P.expect(label, not bad, 'invalid pieces: %s' % bad[:3])
    ov = [o for o in r['overlaps'] if abs(o[2]) > 0.01]
    P.expect(label, not ov, 'overlapping pieces: %s' % ov[:3])
    tiny = [a for a in r['area'] if a < 0.01]
    P.expect(label, not tiny, 'sliver pieces: %s' % tiny)


def divide_case(P, label, geom, command, params, want_shares):
    """Runs `command` on the one feature; the pieces (first = original feature) must have
    `want_shares` of the ellipsoidal area, in that order."""
    data = write_layer('dv', [feature(geom, name='p', n=3)])
    with Session(data) as s:
        before = s.features('dv')
        s.activate('dv')
        s.select('dv', [0])
        try:
            r = s.plugin(command, params)
        except CommandError as e:
            P.add(label, 'refused: %s' % short(e))
            P.expect(label, fingerprint(s.features('dv')) == fingerprint(before), 'a refusal changed data')
            return None
        after = s.features('dv')
        s.undo()
        P.expect(label, fingerprint(s.features('dv')) == fingerprint(before), 'undo did not restore')
    total = area_ell(geom)
    pieces = [f['geometry'] for f in after]
    P.expect(label, len(pieces) == len(want_shares), '%d pieces, expected %d' % (len(pieces), len(want_shares)))
    P.expect(label, after[0]['id'] == before[0]['id'], 'the first piece is not the original feature')
    P.expect(label, all(f['properties'] == {'name': 'p', 'n': 3} for f in after), 'attributes not copied')
    got = [area_ell(g) for g in pieces]
    P.expect(label, abs(sum(got) - total) <= tol(total), 'sum %.4f, original %.4f' % (sum(got), total))
    if len(pieces) == len(want_shares):
        errs = [(i, round(a, 4), round(w * total, 4)) for i, (a, w) in enumerate(zip(got, want_shares))
                if abs(a - w * total) > tol(w * total)]
        P.expect(label, not errs, 'piece areas off (i, got, want): %s' % errs[:4])
    valid_no_overlap(P, label, pieces, geom)
    return r, after


def equal_case(P, label, geom, parts, angle, reverse=False):
    out = divide_case(P, label, geom, 'divide-equal', {'parts': parts, 'angle': angle, 'reverse': reverse},
                      [1.0 / parts] * parts)
    if out:
        try:
            # lines at a and a + 180 are the same lines; parts are numbered as for 0-179
            check_cover(geom, [f['geometry'] for f in out[1]], angle % 180, reverse)
        except AssertionError as e:
            P.add(label, 'order/cover: %s' % e)
    return out


def test_divide_concave_shapes():
    P = Problems()
    for name, ring in (('comb', COMB), ('L', L_SHAPE), ('U', U_SHAPE)):
        for parts, angle in ((4, 0), (5, 90), (7, 45), (3, 135)):
            equal_case(P, '%s %d parts at %d' % (name, parts, angle), P_(ring), parts, angle)
    P.done()


def test_divide_holed_into_many():
    P = Problems()
    g = P_(SQ, HOLE)
    for parts, angle in ((10, 0), (12, 90), (9, 30), (2, 45)):
        equal_case(P, 'holed %d parts at %d' % (parts, angle), g, parts, angle)
    # a hole exactly on the cut line of 2 parts (x = 50 splits the hole in half)
    equal_case(P, 'holed cut through the hole centre', P_(SQ, cw(sq(40, 20, 20, 60))), 2, 0)
    P.done()


def test_divide_angles():
    P = Problems()
    for angle in (0, 45, 89.999, 90, 90.001, 179, 179.999, 180, 270, -30, 359.9):
        equal_case(P, 'L 3 parts at %g' % angle, P_(L_SHAPE), 3, angle)
    P.done()


def test_cut_off_percent_extremes():
    P = Problems()
    for pct in (1, 99, 0.5, 50):
        divide_case(P, 'percent %g' % pct, P_(L_SHAPE), 'divide-percent', {'percent': pct, 'angle': 0},
                    [1 - pct / 100, pct / 100])
    for pct, angle in ((1, 90), (99, 45), (33.3, 135)):
        divide_case(P, 'comb percent %g at %d' % (pct, angle), P_(COMB), 'divide-percent',
                    {'percent': pct, 'angle': angle}, [1 - pct / 100, pct / 100])
    P.done()


def test_cut_off_percent_refuses_0_and_100():
    P = Problems()
    data = write_layer('dv', [feature(P_(SQ), name='p')])
    with Session(data) as s:
        before = s.features('dv')
        s.activate('dv')
        for pct in (0, 100, -5, 150):
            s.select('dv', [0])
            try:
                r = s.plugin('divide-percent', {'percent': pct})
                P.add('percent %g' % pct, 'accepted: %s' % r.get('message'))
                s.undo()
            except CommandError as e:
                P.expect('percent %g' % pct, 'percent' in str(e).lower() or 'share' in str(e).lower(),
                         'message does not name the setting: %s' % short(e))
            P.expect('percent %g' % pct, fingerprint(s.features('dv')) == fingerprint(before), 'data changed')
    P.done()


def test_divide_many_parts_small_polygon():
    P = Problems()
    equal_case(P, '10 x 10 m into 50', P_(sq(0, 0, 10)), 50, 0)
    equal_case(P, '10 x 10 m into 50 at 37', P_(sq(0, 0, 10)), 50, 37)
    equal_case(P, '1 x 2000 m strip into 100 along it', P_(sq(0, 0, 2000, 1)), 100, 0)
    equal_case(P, '1 x 2000 m strip into 3 across', P_(sq(0, 0, 2000, 1)), 3, 90)
    P.done()


def test_divide_multipolygon():
    P = Problems()
    mp = mpoly(O, [sq(0, 0, 100)], [sq(150, 10, 120, 80)], [sq(400, 0, 10)])
    for parts, angle in ((3, 0), (4, 90), (6, 45)):
        equal_case(P, 'multipolygon %d at %d' % (parts, angle), mp, parts, angle)
    divide_case(P, 'multipolygon percent 10', mp, 'divide-percent', {'percent': 10, 'angle': 0}, [0.9, 0.1])
    P.done()


def test_divide_reverse():
    P = Problems()
    for angle in (0, 90, 45):
        equal_case(P, 'comb reverse at %d' % angle, P_(COMB), 4, angle, reverse=True)
    out = divide_case(P, 'percent reverse', P_(sq(0, 0, 300, 100)), 'divide-percent',
                      {'percent': 10, 'angle': 0, 'reverse': True}, [0.9, 0.1])
    if out:
        m = metric(P_(sq(0, 0, 300, 100)))
        xs = [m(c)[0] for c in out[1][1]['geometry']['coordinates'][0]]
        P.expect('percent reverse', min(xs) > 269, 'the 10 %% share is not on the east: x from %.2f' % min(xs))
    P.done()


def test_divide_cut_through_vertices():
    P = Problems()
    equal_case(P, 'house apex on the cut', P_(HOUSE), 2, 0)
    equal_case(P, 'diamond corners on the cut', P_(DIAMOND), 2, 0)
    equal_case(P, 'diamond corners on the cut at 90', P_(DIAMOND), 2, 90)
    equal_case(P, 'square diagonal cut', P_(SQ), 2, 45)
    # a vertex where the 4-part cuts fall: every cut at a multiple of 25 m on a ring with
    # vertices there
    ring = [(0, 0), (25, 0), (50, 0), (75, 0), (100, 0), (100, 100), (75, 100), (50, 100), (25, 100), (0, 100)]
    equal_case(P, 'cuts through collinear vertices', P_(ring), 4, 0)
    # duplicate consecutive vertices
    dup = [(0, 0), (100, 0), (100, 0), (100, 100), (0, 100), (0, 100)]
    equal_case(P, 'duplicate vertices', P_(dup), 3, 0)
    P.done()


def test_divide_far_frames():
    P = Problems()
    for origin in ((14.2, 52.0), (24.1, 50.5), ABROAD):
        equal_case(P, 'L 5 parts at %s' % (origin,), poly(origin, L_SHAPE), 5, 30)
    P.done()


# ---------- the Divide Parcel tool ----------

def tool_case(P, label, geom, params, want_shares, refuse=False):
    data = write_layer('shapes', [feature(geom, name='p')])
    with Session(data) as s:
        before = s.features('shapes')
        p = dict(params, input='shapes')
        try:
            s.run('gp.run', {'tool': TOOL, 'params': p})
        except CommandError as e:
            P.expect(label, refuse, 'refused: %s' % short(e))
            P.expect(label, fingerprint(s.features('shapes')) == fingerprint(before), 'the input changed')
            return None
        P.expect(label, not refuse, 'expected a refusal')
        out = s.features('shapes divided')
        P.expect(label, fingerprint(s.features('shapes')) == fingerprint(before), 'the input changed')
    total = area_ell(geom)
    if callable(want_shares):
        want_shares = want_shares(total)
    pieces = [f['geometry'] for f in out]
    got = [area_ell(g) for g in pieces]
    P.expect(label, len(pieces) == len(want_shares), '%d parts, expected %d (areas %s)'
             % (len(pieces), len(want_shares), [round(a, 3) for a in got]))
    P.expect(label, abs(sum(got) - total) <= tol(total), 'sum %.4f vs %.4f' % (sum(got), total))
    if len(pieces) == len(want_shares):
        errs = [(i, round(a, 4), round(w * total, 4)) for i, (a, w) in enumerate(zip(got, want_shares))
                if abs(a - w * total) > tol(w * total)]
        P.expect(label, not errs, 'part areas off: %s' % errs[:4])
    for f, a in zip(out, got):
        if params.get('areaMode') == 'Planar':
            continue
        P.expect(label, abs(f['properties']['area_m2'] - a) <= tol(a),
                 'reported area %.4f vs %.4f' % (f['properties']['area_m2'], a))
    valid_no_overlap(P, label, pieces, geom)
    return out


def test_tool_area_and_width_edges():
    P = Problems()
    g = P_(sq(0, 0, 300, 100))
    total = area_ell(g)
    # nothing to divide: refused with a message naming the parcel
    tool_case(P, 'area larger than the parcel', g, {'mode': 'Parts of given area', 'area': 2 * total}, [1.0],
              refuse=True)
    tool_case(P, 'area = whole parcel', g, {'mode': 'Parts of given area', 'area': total}, [1.0], refuse=True)
    tool_case(P, 'area = a third', g, {'mode': 'Parts of given area', 'area': total / 3}, [1 / 3] * 3)
    tool_case(P, 'area a hair under a third', g, {'mode': 'Parts of given area', 'area': total / 3 - 0.001},
              [1 / 3] * 3)  # a 0.003 m² remainder is a sliver: it goes with the last part
    tool_case(P, 'width 100 on a 300 m wide rect', g, {'mode': 'Strips of given width', 'width': 100, 'areaMode': 'Planar'},
              [1 / 3] * 3)
    tool_case(P, 'width wider than the parcel', g, {'mode': 'Strips of given width', 'width': 1000}, [1.0],
              refuse=True)
    tool_case(P, 'equal parts count 1', g, {'mode': 'Equal parts', 'count': 1}, [1.0], refuse=True)
    P.done()


def test_tool_holed_and_comb():
    P = Problems()
    tool_case(P, 'comb 6 equal at 90', P_(COMB), {'mode': 'Equal parts', 'count': 6, 'angle': 90}, [1 / 6] * 6)
    tool_case(P, 'holed area 1500', P_(SQ, HOLE), {'mode': 'Parts of given area', 'area': 1500, 'angle': 0},
              lambda t: [1500 / t] * 5 + [(t - 7500) / t])
    P.done()
