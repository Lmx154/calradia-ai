"""CalradiaAI mod (vanilla + mods/calradia_ai/overlay): builds, and changes only what it should.

Everything outside the new camp-menu options and script_game_receive_url_response must stay
vanilla: other files byte-identical, and in files that embed quick strings the only
permitted difference is a renumbered quick-string operand (tag_quick_string).
"""

from __future__ import annotations

import pytest
from golden_compare import GOLDEN_EXPORT, REPO, SOURCE
from support import read_output

from modsys.build import build

OVERLAY = REPO / "mods" / "calradia_ai" / "overlay"
TAG_QUICK_STRING = 22
NEW_VARIABLE = b"calradia_ai_request_pending"
URL = b"http://127.0.0.1:8766/ping"
# Files whose only permitted differences are renumbered quick-string operands.
QSTR_ONLY = ["conversation.txt", "mission_templates.txt", "simple_triggers.txt", "triggers.txt"]
CHANGED = {"menus.txt", "scripts.txt", "quick_strings.txt", "variables.txt", "variable_uses.txt"}


@pytest.fixture(scope="module")
def mod(tmp_path_factory: pytest.TempPathFactory) -> dict[str, bytes]:
    out = tmp_path_factory.mktemp("calradia_ai") / "CalradiaAI"
    result = build(SOURCE, out, overlay=OVERLAY, quiet=True)
    assert result.passes == 1
    assert not result.warnings
    return read_output(out)


def golden(name: str) -> bytes:
    return (GOLDEN_EXPORT / name).read_bytes()


def only_quick_string_diffs(a: bytes, b: bytes) -> bool:
    ta, tb = a.split(), b.split()
    if len(ta) != len(tb):
        return False
    return all(
        x == y or (int(x) >> 56 == int(y) >> 56 == TAG_QUICK_STRING)
        for x, y in zip(ta, tb, strict=True)
    )


def test_untouched_files_are_vanilla(mod: dict[str, bytes]) -> None:
    assert set(mod) == {p.name for p in GOLDEN_EXPORT.iterdir()}
    for name in sorted(set(mod) - CHANGED - set(QSTR_ONLY)):
        assert mod[name] == golden(name), name


@pytest.mark.parametrize("name", QSTR_ONLY)
def test_only_quick_string_indices_move(mod: dict[str, bytes], name: str) -> None:
    assert only_quick_string_diffs(golden(name), mod[name])


def test_vanilla_global_variables_keep_their_indices(mod: dict[str, bytes]) -> None:
    assert mod["variables.txt"] == golden("variables.txt") + NEW_VARIABLE + b"\n"


def test_only_the_url_response_script_changes(mod: dict[str, bytes]) -> None:
    a, b = golden("scripts.txt").split(b"\n"), mod["scripts.txt"].split(b"\n")
    assert len(a) == len(b)
    changed = [i for i, (x, y) in enumerate(zip(a, b, strict=True)) if x != y]
    real = [i for i in changed if not only_quick_string_diffs(a[i], b[i])]
    assert [b[i - 1] for i in real] == [b"game_receive_url_response -1"]


def test_camp_menu_sends_the_request(mod: dict[str, bytes]) -> None:
    menus = mod["menus.txt"]
    assert b" mno_calradia_ai_contact " in menus
    assert b" mno_calradia_ai_abandon " in menus
    assert URL in mod["quick_strings.txt"]
    contact = menus.split(b" mno_calradia_ai_contact ")[1].split(b" mno_")[0]
    assert b" 380 2 " in contact  # (send_message_to_url, s0, 0): opcode 380, two operands


def test_overlay_files_match_vanilla_outside_the_mod_blocks() -> None:
    for name in ("module_game_menus.py", "module_scripts.py"):
        base = (SOURCE / name).read_text(encoding="cp1254").splitlines()
        mine = (OVERLAY / name).read_text(encoding="cp1254").splitlines()
        added = [line for line in mine if line not in base]
        assert added, name
        assert set(base) - set(mine) <= _allowed_removed(name), name


def _allowed_removed(name: str) -> set[str]:
    if name != "module_scripts.py":
        return set()
    # The upstream commented-out example body of game_receive_url_response.
    lines = (SOURCE / name).read_text(encoding="cp1254").splitlines()
    start = lines.index('  ("game_receive_url_response",')
    end = lines.index('  ("game_get_cheat_mode",')
    return set(lines[start:end])
