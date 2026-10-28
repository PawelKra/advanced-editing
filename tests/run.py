"""Runs the plugin tests: every `test_*` function of every `tests/test_*.py`.

    python -I tests/run.py                 # everything
    python -I tests/run.py polygons        # tests/test_polygons.py only
    python -I tests/run.py -k delete       # tests whose name contains "delete"

Build first (../build.sh): the tests load pkg-diag/plugin.wasm.
"""

import importlib.util
import sys
import time
import traceback
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

import aelib  # noqa: E402


def main(argv):
    pattern = None
    modules = []
    it = iter(argv)
    for a in it:
        if a == '-k':
            pattern = next(it, None)
        else:
            modules.append(a.removeprefix('test_').removesuffix('.py'))
    files = sorted(HERE.glob('test_*.py'))
    if modules:
        files = [f for f in files if f.stem.removeprefix('test_') in modules]
    passed, skipped, failed = 0, 0, []
    t0 = time.time()
    try:
        for f in files:
            spec = importlib.util.spec_from_file_location(f.stem, f)
            mod = importlib.util.module_from_spec(spec)
            spec.loader.exec_module(mod)
            tests = [(n, getattr(mod, n)) for n in dir(mod) if n.startswith('test_') and callable(getattr(mod, n))]
            tests.sort(key=lambda t: t[1].__code__.co_firstlineno)
            for name, fn in tests:
                if pattern and pattern not in name:
                    continue
                t = time.time()
                try:
                    fn()
                    passed += 1
                    print('ok    %-48s %5.1fs' % ('%s.%s' % (f.stem, name), time.time() - t), flush=True)
                except aelib.Skip as e:
                    skipped += 1
                    print('skip  %s.%s: %s' % (f.stem, name, e), flush=True)
                except Exception as e:  # noqa: BLE001 - report every failure and go on
                    failed.append('%s.%s' % (f.stem, name))
                    print('FAIL  %s.%s: %s' % (f.stem, name, e), flush=True)
                    if not isinstance(e, AssertionError):
                        traceback.print_exc()
    finally:
        aelib.cleanup()
    print('\n%d passed, %d skipped, %d failed in %.1fs' % (passed, skipped, len(failed), time.time() - t0))
    for n in failed:
        print('  failed:', n)
    return 1 if failed or not passed else 0


if __name__ == '__main__':
    sys.exit(main(sys.argv[1:]))
