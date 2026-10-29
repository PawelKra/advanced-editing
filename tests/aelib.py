"""Test harness for the Advanced Editing plugin (standard library only).

Runs the plugin in a private, headless RustGIS (`rustgis-cli mcp --yes` over stdio, with its
own in-memory project and its own plugin folder), never in the desktop app.

The plugin under test is `pkg-diag/plugin.wasm` (built by ../build.sh with the diagnostic
commands) with plugin.json plus those commands' declarations, installed into a temporary
plugin folder.
"""

import json
import math
import os
import shutil
import subprocess
import tempfile
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
PLUGIN_ID = 'advanced-editing'
PREFIX = 'plugin:%s/' % PLUGIN_ID
CLI = os.environ.get('RUSTGIS_CLI', 'rustgis-cli')

# Commands compiled in with `--features diagnostics` (src/diagnostics.rs).
DIAG_COMMANDS = ['diag-proj', 'diag-geojson', 'diag-snap', 'diag-features', 'diag-selection',
                 'diag-delete', 'diag-add', 'diag-settings', 'diag-layers', 'diag-bench', 'diag-divide-parcel']


class CommandError(Exception):
    """A command answered an error (its edits were rolled back)."""


class Check(AssertionError):
    pass


def check(cond, msg='check failed'):
    if not cond:
        raise Check(msg)


def manifest():
    return json.loads((REPO / 'plugin.json').read_text())


def command_ids():
    return [c['id'] for c in manifest()['commands']]


# ---------- temporary files and the plugin folder ----------

_ENV = {}


def tmpdir():
    """One temporary directory per test run (removed by run.py at the end)."""
    if 'tmp' not in _ENV:
        _ENV['tmp'] = Path(tempfile.mkdtemp(prefix='ae-tests-', dir=os.environ.get('AE_TMP')))
    return _ENV['tmp']


def cleanup():
    t = _ENV.pop('tmp', None)
    if t and not os.environ.get('AE_KEEP_TMP'):
        shutil.rmtree(t, ignore_errors=True)
    _ENV.clear()


def plugins_dir():
    """A plugin folder holding only the diagnostic build of this plugin."""
    if 'plugins' not in _ENV:
        wasm = REPO / 'pkg-diag' / 'plugin.wasm'
        if not wasm.exists():
            raise RuntimeError('pkg-diag/plugin.wasm is missing: run ./build.sh first')
        pkg = tmpdir() / 'pkg-diag'
        pkg.mkdir(exist_ok=True)
        m = manifest()
        m['commands'] = m['commands'] + [{'id': c, 'label': c, 'params': '{}'} for c in DIAG_COMMANDS]
        (pkg / 'plugin.json').write_text(json.dumps(m, indent=1))
        shutil.copy(wasm, pkg / 'plugin.wasm')
        pdir = tmpdir() / 'plugins'
        pdir.mkdir(exist_ok=True)
        env = dict(os.environ, RUSTGIS_PLUGINS_DIR=str(pdir))
        r = subprocess.run([CLI, 'plugins', 'install', str(pkg)], env=env, capture_output=True, text=True)
        if r.returncode != 0:
            raise RuntimeError('installing the test plugin failed: ' + r.stderr)
        _ENV['plugins'] = pdir
    return _ENV['plugins']


# ---------- synthetic data ----------

M_PER_DEG_LAT = 111_320.0


def offset_m(lon, lat, dx, dy):
    """(lon, lat) moved dx metres east and dy north (spherical approximation; plenty for
    building test shapes)."""
    return [lon + dx / (M_PER_DEG_LAT * math.cos(math.radians(lat))), lat + dy / M_PER_DEG_LAT]


def rect(lon, lat, w, h, x0=0.0, y0=0.0):
    """A closed counter-clockwise ring: w x h metres, lower-left corner (x0, y0) metres from
    (lon, lat)."""
    pts = [(x0, y0), (x0 + w, y0), (x0 + w, y0 + h), (x0, y0 + h), (x0, y0)]
    return [offset_m(lon, lat, x, y) for x, y in pts]


def polygon(*rings):
    return {'type': 'Polygon', 'coordinates': list(rings)}


def multipolygon(*polys):
    return {'type': 'MultiPolygon', 'coordinates': [p['coordinates'] for p in polys]}


def linestring(lon, lat, pts_m):
    return {'type': 'LineString', 'coordinates': [offset_m(lon, lat, x, y) for x, y in pts_m]}


def point(lon, lat, x=0.0, y=0.0):
    return {'type': 'Point', 'coordinates': offset_m(lon, lat, x, y)}


def feature(geometry, **props):
    return {'type': 'Feature', 'properties': props, 'geometry': geometry}


def write_layer(name, features):
    """Writes a GeoJSON file `<name>.geojson` (RustGIS names the layer `name`)."""
    p = tmpdir() / 'data'
    p.mkdir(exist_ok=True)
    f = p / (name + '.geojson')
    f.write_text(json.dumps({'type': 'FeatureCollection', 'features': features}))
    return f


# Near Warsaw: EPSG:2180 is used. Outside Poland: UTM.
PL = (19.0, 52.0)
ABROAD = (10.5, 45.5)


def grid_layer(name, origin=PL, nx=3, ny=1, size=100.0):
    """nx x ny adjacent squares of `size` metres, attributes name='c<i>_<j>' and n=i."""
    lon, lat = origin
    feats = []
    for j in range(ny):
        for i in range(nx):
            feats.append(feature(polygon(rect(lon, lat, size, size, i * size, j * size)),
                                 name='c%d_%d' % (i, j), n=j * nx + i))
    return write_layer(name, feats)


def holed_layer(name, origin=PL):
    """One 100 m square with a 40 m square hole in the middle."""
    lon, lat = origin
    outer = rect(lon, lat, 100, 100)
    hole = list(reversed(rect(lon, lat, 40, 40, 30, 30)))
    return write_layer(name, [feature(polygon(outer, hole), name='holed', n=0)])


# ---------- a private RustGIS ----------

class Session:
    """A headless RustGIS with the test plugin, over MCP on stdio. Use as a context manager."""

    def __init__(self, *data):
        env = dict(os.environ, RUSTGIS_PLUGINS_DIR=str(plugins_dir()))
        args = [CLI, 'mcp', '--yes']
        for d in data:
            args += ['--data', str(d)]
        self.p = subprocess.Popen(args, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                  stderr=subprocess.DEVNULL, text=True, env=env)
        self.i = 0
        self._req('initialize', {'protocolVersion': '2024-11-05', 'capabilities': {},
                                 'clientInfo': {'name': 'advanced-editing-tests', 'version': '0'}})
        self._send({'jsonrpc': '2.0', 'method': 'notifications/initialized'})

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()

    def close(self):
        if self.p.poll() is None:
            self.p.stdin.close()
            try:
                self.p.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.p.kill()

    def _send(self, m):
        self.p.stdin.write(json.dumps(m) + '\n')
        self.p.stdin.flush()

    def _req(self, method, params):
        self.i += 1
        self._send({'jsonrpc': '2.0', 'id': self.i, 'method': method, 'params': params})
        while True:
            line = self.p.stdout.readline()
            if not line:
                raise RuntimeError('rustgis-cli mcp exited')
            r = json.loads(line)
            if r.get('id') == self.i:
                return r

    def tool(self, name, args):
        """Calls an MCP tool; returns its text, raises CommandError on an error result."""
        r = self._req('tools/call', {'name': name, 'arguments': args})
        if 'error' in r:
            raise CommandError(json.dumps(r['error']))
        res = r['result']
        text = ''.join(c.get('text', '') for c in res.get('content', []))
        if res.get('isError'):
            raise CommandError(text)
        return text

    def run(self, command, params=None):
        """Runs any RustGIS command; returns its answer parsed as JSON when it is JSON."""
        text = self.tool('run_command', {'command': command, 'params': params if params is not None else {}})
        try:
            return json.loads(text)
        except ValueError:
            return text

    def plugin(self, command, params=None):
        """Runs one of this plugin's commands (`cut-with-polygon`, `diag-layers`…)."""
        return self.run(PREFIX + command, params)

    def error_of(self, command, params=None):
        """Runs a plugin command that must fail; returns the error text."""
        try:
            r = self.plugin(command, params)
        except CommandError as e:
            return str(e)
        raise Check('%s succeeded, expected an error: %s' % (command, json.dumps(r)[:300]))

    # --- map state ---

    def layers(self):
        return {l['name']: l for l in self.plugin('diag-layers')}

    def layer_id(self, name):
        return self.layers()[name]['id']

    def activate(self, name):
        self.run('layer.select', {'layer': name})

    def select(self, name, rows, add=False):
        p = {'layer': name, 'rows': list(rows)}
        if add:
            p['mode'] = 'add'
        self.run('select.rows', p)

    def clear_selection(self):
        self.run('select.clear')

    def features(self, name):
        """The layer as a GeoJSON FeatureCollection (feature index = row)."""
        out = tmpdir() / ('export-%d.geojson' % self.i)
        self.run('data.convert', {'layer': name, 'format': 'geojson', 'path': str(out)})
        fc = json.loads(out.read_text())
        out.unlink()
        return fc['features']

    def selected_ids(self, name):
        return self.plugin('diag-features', {'layer': self.layer_id(name), 'selected': True})['ids']

    def undo(self):
        return self.run('edit.undo')

    def redo(self):
        return self.run('edit.redo')


def run_once(cmds, data=()):
    """One `rustgis-cli run` with the test plugin: `cmds` are (command, params-or-None);
    None passes no parameters at all, as the ribbon does. Returns (exit code, stdout+stderr)."""
    env = dict(os.environ, RUSTGIS_PLUGINS_DIR=str(plugins_dir()))
    args = [CLI, 'run']
    for d in data:
        args += ['--data', str(d)]
    for c, p in cmds:
        args += ['--cmd', c if p is None else c + '=' + json.dumps(p)]
    r = subprocess.run(args, capture_output=True, text=True, env=env)
    return r.returncode, r.stdout + r.stderr


# ---------- geometry helpers for assertions ----------

def ring_area_m2(ring, lat0=None):
    """Planar area of a lon/lat ring in m² (local equirectangular; fine for small shapes)."""
    lat0 = lat0 if lat0 is not None else ring[0][1]
    k = M_PER_DEG_LAT * math.cos(math.radians(lat0))
    pts = [(x * k, y * M_PER_DEG_LAT) for x, y in ring]
    return abs(sum(a[0] * b[1] - b[0] * a[1] for a, b in zip(pts, pts[1:]))) / 2


def polygon_area_m2(geom):
    """Area of a Polygon/MultiPolygon GeoJSON geometry in m² (holes subtracted)."""
    polys = [geom['coordinates']] if geom['type'] == 'Polygon' else geom['coordinates']
    return sum(ring_area_m2(p[0]) - sum(ring_area_m2(h) for h in p[1:]) for p in polys)


def close(a, b, tol):
    return abs(a - b) <= tol


class Skip(Exception):
    """Raised by a test that cannot run here (e.g. optional real data not configured)."""


# ---------- stub smoke tests ----------

def stub_fixture():
    """Three adjacent squares ('parcels'), a polygon over two of them ('cutter') and a line
    across all three ('lines'); returns the data files."""
    lon, lat = PL
    return [
        grid_layer('parcels', nx=3),
        write_layer('cutter', [feature(polygon(rect(lon, lat, 120, 50, 40, 25)), name='cut')]),
        write_layer('lines', [feature(linestring(lon, lat, [(-10, 50), (310, 50)]), name='l')]),
    ]


def assert_stubs(commands, tools=()):
    """Every command in `commands` (and tool in `tools`) must fail with "not implemented
    yet" and leave the data unchanged. Run with a plausible selection in place."""
    with Session(*stub_fixture()) as s:
        s.select('parcels', [0, 1])
        s.activate('cutter')
        s.select('cutter', [0], add=True)
        before = s.features('parcels')
        for c in commands:
            e = s.error_of(c)
            check('not implemented yet' in e, '%s: %s' % (c, e))
        for t in tools:
            try:
                s.run('gp.run', {'tool': PREFIX + t, 'params': {'input': 'parcels'}})
                raise Check('tool %s succeeded' % t)
            except CommandError as e:
                check('not implemented yet' in str(e), '%s: %s' % (t, e))
        check(s.features('parcels') == before, 'a stub changed the data')
