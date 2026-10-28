"""Optional checks on real data: set AE_REAL_DATA to a GeoPackage (or any file RustGIS
opens) of adjacent parcels in Poland. Nothing from it is stored in the repository."""

import os

from aelib import Session, Skip, check


def real_session():
    path = os.environ.get('AE_REAL_DATA')
    if not path:
        raise Skip('AE_REAL_DATA is not set')
    s = Session(path)
    layers = [l for l in s.layers().values() if l['dim'] == 'polygon']
    if not layers:
        s.close()
        raise Skip('no polygon layer in %s' % path)
    return s, layers[0]


def test_real_neighbours_and_frame():
    s, layer = real_session()
    with s:
        s.activate(layer['name'])
        s.select(layer['name'], [0])
        r = s.plugin('diag-selection', {'toolDims': ['polygon'], 'targetDims': ['polygon']})
        check(r['frame'] == 'EPSG:2180', r)
        check(r['fromActive'] and r['targets'] and r['targets'][0]['ids'], 'no neighbours found: %s' % r)
        if layer['features'] <= 1000:
            r = s.plugin('diag-selection', {'mode': 'edit', 'dims': ['polygon'], 'whole': True})
            check(len(r['ids']) == layer['features'], 'read %d of %d' % (len(r['ids']), layer['features']))


def test_real_vertex_snap_is_exact():
    s, layer = real_session()
    with s:
        for f in s.features(layer['name'])[:20]:
            r = s.plugin('diag-snap', {'geometry': f['geometry']})
            check(r['exact'] == r['n'], r)
