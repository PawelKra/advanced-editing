"""Budget tests for the segment scans of src/lines.rs: Split With Line, Reshape and
Trim/Extend compare only the segments near each other, charge that work before writing, and
refuse cleanly (never "it ran too long") when it would not fit in one command."""

import math
import time

from aelib import PL, Session, check, feature, linestring, write_layer

N = 10_000


def run(s, command, params=None):
    """The command's result, or its error message."""
    try:
        return s.plugin(command, params or {})
    except Exception as e:  # noqa: BLE001 - the message is what is checked
        return str(e)


def dense_pair(name, tool, target):
    lon, lat = PL
    return (write_layer(name + '_tool', [feature(linestring(lon, lat, tool))]),
            write_layer(name + '_target', [feature(linestring(lon, lat, target))]))


def crossing_lines():
    # two wavy lines of N segments each crossing once in the middle: every segment's box
    # meets only a few of the other line's
    tool = [(i * 0.5, 0.2 * math.sin(i)) for i in range(N + 1)]
    target = [(N * 0.25 + 0.2 * math.sin(i), i * 0.5 - N * 0.25) for i in range(N + 1)]
    return tool, target


def overlapping_zigzags():
    # two zigzags of N segments 20 m high running over each other: every segment crosses
    # its neighbours on the other line, tens of thousands of crossings
    tool = [(i * 0.5, 20.0 * (i % 2)) for i in range(N + 1)]
    target = [(i * 0.5 + 0.25, 20.0 * (i % 2) + 0.1) for i in range(N + 1)]
    return tool, target


def apply(command, files, name, params=None):
    with Session(*files) as s:
        s.select(name + '_target', [0])
        s.activate(name + '_tool')
        s.select(name + '_tool', [0], add=True)
        before = s.features(name + '_target')
        t = time.time()
        r = run(s, command, params)
        return r, time.time() - t, before, s.features(name + '_target')


def test_split_two_dense_crossing_lines():
    r, dt, _, after = apply('split-with-line', dense_pair('xs', *crossing_lines()), 'xs')
    check(isinstance(r, dict) and r['split'] == 1 and r['created'] == 1, r)
    check(len(after) == 2, len(after))
    print('      split of two crossing lines of %d segments: %.2fs' % (N, dt))


def test_trim_with_a_dense_boundary():
    tool, target = crossing_lines()
    # the target crosses the boundary 500 m before its end
    target = target[:int(0.6 * N)]
    r, dt, _, after = apply('trim-extend', dense_pair('xt', tool, target), 'xt')
    check(isinstance(r, dict) and r['trimmed'] == 1, r)
    print('      trim of a line of %d segments to a boundary of %d: %.2fs' % (len(target) - 1, N, dt))


def test_dense_overlapping_lines_are_refused_up_front():
    # about 30 000 crossing segment pairs: without the estimate Trim/Extend runs out of budget,
    # and Split With Line already in snapping the line to the target
    files = dense_pair('zz', *overlapping_zigzags())
    for command in ('split-with-line', 'reshape', 'trim-extend'):
        r, dt, before, after = apply(command, files, 'zz')
        check(isinstance(r, str) and 'more than one command can do' in r and 'ran too long' not in r, (command, r))
        check(after == before, command + ': a refusal changed data')
        print('      %s refused in %.2fs' % (command, dt))


def stubs_layer(name, n):
    """A boundary line at x = 0 and `n` short lines stopping 0.5 m short of it."""
    lon, lat = PL
    feats = [feature(linestring(lon, lat, [(0, -1), (0, n * 0.2 + 1)]))]
    feats += [feature(linestring(lon, lat, [(-5, i * 0.2), (-0.5, i * 0.2)])) for i in range(n)]
    return write_layer(name, feats)


def trim_in_active_layer(name, n):
    with Session(stubs_layer(name, n)) as s:
        s.activate(name)
        s.select(name, [0])
        before = s.features(name)
        return run(s, 'trim-extend'), before, s.features(name)


def test_trim_in_the_active_layer_counts_the_search_and_the_edits():
    # with nothing selected elsewhere every line of the active layer is a candidate: the read,
    # the search and the edits of the lines found are checked before anything is written
    # (3000 lines ran out of budget when only the lines touching the boundary were counted)
    r, _, after = trim_in_active_layer('stubs_ok', 2000)
    check(isinstance(r, dict) and r['extended'] == 2000, r)
    check(all(abs(f['geometry']['coordinates'][-1][0] - after[0]['geometry']['coordinates'][0][0]) < 1e-9
              for f in after[1:]), 'a line does not end on the boundary')
    r, before, after = trim_in_active_layer('stubs_big', 3000)
    check(isinstance(r, str) and 'Select the lines to trim or extend in another layer' in r
          and 'ran too long' not in r, r)
    check(after == before, 'a refusal changed data')


def many_pieces(name, n, polygon):
    """A zigzag of `n` segments across a strip (or along a line) `n` metres long: about `n`
    pieces."""
    lon, lat = PL
    tool = [(i * 1.0, -1.0 if i % 2 == 0 else 11.0) for i in range(n + 1)]
    if polygon:
        strip = [(0.5, 0), (n - 0.5, 0), (n - 0.5, 10), (0.5, 10), (0.5, 0)]
        target = feature({'type': 'Polygon', 'coordinates': [linestring(lon, lat, strip)['coordinates']]})
    else:
        target = feature(linestring(lon, lat, [(0.5, 5), (n - 0.5, 5)]))
    return (write_layer(name + '_tool', [feature(linestring(lon, lat, tool))]),
            write_layer(name + '_target', [target]))


def test_splits_into_thousands_of_pieces_fit_or_are_refused_up_front():
    # snapping, slicing, placing the new vertices and writing the pieces are all counted
    # before the first write: a split either fits or changes nothing
    for polygon, n, fits in ((True, 1000, True), (True, 4000, False), (False, 2000, True), (False, 4000, False)):
        name = 'pc%d%s' % (n, 'p' if polygon else 'l')
        r, dt, before, after = apply('split-with-line', many_pieces(name, n, polygon), name)
        if fits:
            check(isinstance(r, dict) and r['split'] == 1 and r['created'] >= n - 2, (name, r))
        else:
            check(isinstance(r, str) and 'more than one command can' in r and 'ran too long' not in r, (name, r))
            check(after == before, name + ': a refusal changed data')
        print('      %s into about %d pieces: %s (%.2fs)' % ('polygon' if polygon else 'line', n,
                                                     'split' if fits else 'refused', dt))
