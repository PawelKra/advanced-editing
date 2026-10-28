"""plugin.json against the code: every declared command is dispatched, the tab and panel
name only existing commands."""

import re

from aelib import REPO, check, manifest


def lib_commands():
    src = (REPO / 'src' / 'lib.rs').read_text()
    table = src.split('pub const COMMANDS', 1)[1].split('];', 1)[0]
    return re.findall(r'\("([a-z0-9-]+)",', table)


def test_commands_match_dispatch_table():
    m = manifest()
    declared = [c['id'] for c in m['commands']]
    check(sorted(declared) == sorted(lib_commands()), 'plugin.json %s vs lib.rs %s' % (declared, lib_commands()))
    for c in m['commands']:
        check(c.get('label') and c.get('help') and c.get('icon'), 'incomplete command %s' % c['id'])


def test_tab_and_panel_references():
    m = manifest()
    commands = {c['id'] for c in m['commands']}
    tools = {'tool:' + t['id'] for t in m['tools']}
    valid = commands | tools | {'panel'}
    groups = m['tab']['groups']
    check(1 <= len(groups) <= 8, 'groups')
    for g in groups:
        check(1 <= len(g['items']) <= 16, 'group %s' % g['label'])
        for it in g['items']:
            for x in (it['items'] if isinstance(it, dict) else [it]):
                check(x in valid, 'tab item %s' % x)
    p = m['panel']
    check(p['state'] in commands, 'state command')
    check(len(p['items']) <= 32, 'panel items')
    for it in p['items']:
        if 'command' in it:
            check(it['command'] in commands, 'panel command %s' % it['command'])


def src(name):
    return (REPO / 'src' / name).read_text()


def test_every_command_has_help_and_params():
    entries = {c['id']: c for c in manifest()['commands']}
    for cid in lib_commands():
        c = entries.get(cid)
        check(c is not None, 'no plugin.json entry for %s' % cid)
        check(len(c.get('help', '')) >= 20, 'no help for %s' % cid)
        check(c.get('params', '').startswith('{'), 'no params description for %s' % cid)


def test_panel_widgets_read_by_the_code_exist():
    ids = {it.get('id', it.get('command')) for it in manifest()['panel']['items']}
    used = set()
    for f in (REPO / 'src').glob('*.rs'):
        used |= set(re.findall(r'panel_(?:i64|bool)\("([a-z0-9-]+)"\)', f.read_text()))
    check(used, 'no panel values found in the code')
    check(used <= ids, 'panel values read but not declared: %s' % sorted(used - ids))


def test_script_params_are_documented():
    """Every override Settings::from_request reads appears in some command's params text."""
    body = src('lib.rs').split('pub fn from_request', 1)[1].split('\n    }\n}', 1)[0]
    keys = set(re.findall(r'req\.(?:f64|bool)\("([A-Za-z]+)"\)', body))
    keys |= set(re.findall(r'\("([A-Za-z]+)", &mut s\.', body))
    check(len(keys) >= 10, keys)
    documented = ' '.join(c['params'] for c in manifest()['commands'])
    missing = [k for k in keys if '"%s"' % k not in documented]
    check(not missing, 'params not described in plugin.json: %s' % missing)


def test_tool_params_match_the_code():
    body = src('division.rs').split('fn tool_plan', 1)[1]
    read = set(re.findall(r'req\.(?:str|f64|bool|features)\("([A-Za-z_]+)"\)', body))
    read |= set(re.findall(r'num\("([A-Za-z_]+)"\)', body))
    tool = next(t for t in manifest()['tools'] if t['id'] == 'divide-parcel')
    declared = {p['name'] for p in tool['params']}
    check(read == declared, 'divide-parcel reads %s, plugin.json declares %s' % (sorted(read), sorted(declared)))
    for p in tool['params']:
        if p['kind'] == 'choice':
            check(p.get('default') in p['choices'], 'default of %s' % p['name'])


def setting_keys():
    """Settings field -> the script parameter that overrides it (from Settings::from_request)."""
    body = src('lib.rs').split('pub fn from_request', 1)[1].split('\n    }\n}', 1)[0]
    keys = dict((f, k) for k, f in re.findall(r'req\.f64\("([A-Za-z]+)"\)[^\n]*\{\s*s\.([a-z_]+) =', body))
    keys.update((f, k) for k, f in re.findall(r'\("([A-Za-z]+)", &mut s\.([a-z_]+)\)', body))
    return keys


def functions():
    """Every `fn` of src/*.rs by module::name, as (signature, body) (brace-matched)."""
    out = {}
    for f in (REPO / 'src').glob('*.rs'):
        text = f.read_text()
        for m in re.finditer(r'\bfn ([a-z_0-9]+)\b[^{;]*\{', text):
            depth, i = 1, m.end()
            while depth and i < len(text):
                depth += {'{': 1, '}': -1}.get(text[i], 0)
                i += 1
            out.setdefault('%s::%s' % (f.stem, m.group(1)), []).append((m.group(0), text[m.end():i]))
    return out


def test_every_command_documents_the_params_it_reads():
    """Per command: the script parameters its code reads (its settings and direct `req` reads,
    followed through the functions taking a `&Ctx` it calls) appear in its params text."""
    keys = setting_keys()
    check(len(keys) >= 10, keys)
    fns = functions()
    table = src('lib.rs').split('pub const COMMANDS', 1)[1].split('];', 1)[0]
    entries = {c['id']: c for c in manifest()['commands']}
    checked = 0
    for cid, path in re.findall(r'\("([a-z0-9-]+)", ([a-z_:]+)\)', table):
        if cid == 'panel-state':  # shows the settings, takes no parameters
            continue
        name = path if '::' in path else 'lib::' + path
        todo, seen, read = [name], set(), set()
        while todo:
            n = todo.pop()
            if n in seen or n not in fns:
                continue
            seen.add(n)
            module = n.split('::')[0]
            for _, body in fns[n]:
                aliases = re.findall(r'let ([a-z_]+) = &?ctx\.settings;', body)
                for a in ['settings'] + aliases:
                    read |= {keys[f] for f in re.findall(r'\b%s\.([a-z_]+)' % a, body) if f in keys}
                read |= set(re.findall(r'req\.(?:f64|bool|str|i64)\("([A-Za-z]+)"\)', body))
                for q, callee in re.findall(r'\b(?:([a-z_]+)::)?([a-z_0-9]+)\(', body):
                    c = '%s::%s' % (q or module, callee)
                    if any('&Ctx' in sig for sig, _ in fns.get(c, [])):
                        todo.append(c)
        check(len(seen) >= 1, 'no body found for %s' % cid)
        params = entries[cid]['params']
        missing = sorted(k for k in read if '"%s"' % k not in params)
        check(not missing, '%s reads %s but its params text lacks them: %s' % (cid, missing, params))
        checked += bool(read)
    check(checked >= 15, 'too few commands read a parameter: %d' % checked)
