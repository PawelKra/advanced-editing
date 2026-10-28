"""Merge With Attributes on many lines: joining through points where exactly two ends meet,
in time linear in the lines, and the budget check of the overlap sweep."""

import math

from aelib import PL, CommandError, Session, check, feature, linestring, write_layer


def lines_layer(name, lines):
    lon, lat = PL
    return write_layer(name, [feature(linestring(lon, lat, l), name='l%d' % i, n=i) for i, l in enumerate(lines)])


def merge(name, data, count):
    """Merges all `count` features; returns (result or the refusal text, before, after)."""
    with Session(data) as s:
        before = s.features(name)
        s.activate(name)
        s.select(name, range(count))
        try:
            r = s.plugin('merge-with-attributes')
        except CommandError as e:
            r = str(e)
        after = s.features(name)
    return r, before, after


def positions(fs):
    return {tuple(c) for f in fs for part in parts_of(f['geometry']) for c in part}


def parts_of(g):
    return g['coordinates'] if g['type'] == 'MultiLineString' else [g['coordinates']]


def test_merge_radiating_lines_joins_nothing():
    # every end at the centre meets 300 others: nothing is joined
    n = 301
    star = [[(0, 0), (100 * math.cos(2 * math.pi * i / n), 100 * math.sin(2 * math.pi * i / n))] for i in range(n)]
    r, before, after = merge('star', lines_layer('star', star), n)
    check(isinstance(r, dict) and r['merged'] == n, r)
    check(len(after) == 1 and len(parts_of(after[0]['geometry'])) == n, len(parts_of(after[0]['geometry'])))
    check(positions(after) == positions(before), 'vertices moved')


def test_merge_thousands_of_radiating_lines_is_refused_up_front():
    # about 4.5 million pairs of segments sharing the centre: refused before any change, with
    # what to do instead (the old pairwise merge ran out of budget here)
    n = 3000
    star = [[(0, 0), (100 * math.cos(2 * math.pi * i / n), 100 * math.sin(2 * math.pi * i / n))] for i in range(n)]
    r, before, after = merge('bigstar', lines_layer('bigstar', star), n)
    check(isinstance(r, str) and 'Select fewer lines' in r and 'ran too long' not in r, r)
    check(after == before, 'a refusal changed data')


def test_merge_long_chain_into_one_line():
    # 2000 segments of an arc, end to end, shuffled and half of them reversed: one line with
    # every original vertex, bit-exact, in order
    n = 2000
    pts = [(300 * math.cos(i * 0.002), 300 * math.sin(i * 0.002)) for i in range(n + 1)]
    segs = [[pts[i], pts[i + 1]] if i % 2 else [pts[i + 1], pts[i]] for i in range(n)]
    order = sorted(range(n), key=lambda i: (i * 7919) % n)
    r, before, after = merge('chain', lines_layer('chain', [segs[i] for i in order]), n)
    check(isinstance(r, dict) and r['merged'] == n and r['deleted'] == n - 1, r)
    g = after[0]['geometry']
    check(g['type'] == 'LineString' and len(g['coordinates']) == n + 1, (g['type'], len(parts_of(g))))
    # the chain's positions as written, in order (either direction)
    by_index = {}
    for f in before:
        a, b = f['geometry']['coordinates']
        i = order[f['properties']['n']]
        by_index[i], by_index[i + 1] = (b, a) if i % 2 == 0 else (a, b)
    want = [by_index[i] for i in range(n + 1)]
    got = g['coordinates']
    check(got == want or got == want[::-1], 'the chain is not the original vertices in order')


def test_merge_chain_through_t_junction_stops_there():
    # a-b-c joined; d meets the chain's middle end b too, so b has three ends: no join there
    lines = [[(0, 0), (100, 0)], [(100, 0), (200, 10)], [(100, 0), (100, 80)], [(200, 10), (300, 0)]]
    r, before, after = merge('tj', lines_layer('tj', lines), len(lines))
    g = after[0]['geometry']
    sizes = sorted(len(p) for p in parts_of(g))
    check(g['type'] == 'MultiLineString' and sizes == [2, 2, 3], sizes)
