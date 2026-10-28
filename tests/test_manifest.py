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
