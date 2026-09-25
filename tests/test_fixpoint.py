"""ID fixpoint behaviour on edited copies of the Module System (full builds)."""

from __future__ import annotations

from pathlib import Path

import pytest
from support import copy_source, edit_once, read_output

from modsys.build import BuildError, build

pytestmark = pytest.mark.slow


def _swap_troops(path: Path, first: str, second: str) -> None:
    """Swap two adjacent troop entries (each runs up to the next '  [\"' line)."""
    text = path.read_bytes().decode("latin-1")
    s1 = text.index(f'\n  ["{first}",') + 1
    s2 = text.index(f'\n  ["{second}",') + 1
    e2 = text.index('\n  ["', s2) + 1
    assert text.index('\n  ["', s1) + 1 == s2, "troops must be adjacent"
    path.write_bytes((text[:s1] + text[s2:e2] + text[s1:s2] + text[e2:]).encode("latin-1"))


def test_swapped_troops_converge_in_two_passes(tmp_path: Path) -> None:
    # trp_townsman = 26 and trp_watchman = 27: same name length, same id width
    src = copy_source(tmp_path / "src")
    _swap_troops(src / "module_troops.py", "townsman", "watchman")
    stale_ids = (src / "ID_troops.py").read_bytes()

    first = build(src, tmp_path / "out1", sync_ids=True, quiet=True)
    assert first.passes == 2
    assert first.ids_differ_from_source == ["ID_troops.py"]
    assert first.synced_ids == ["ID_troops.py"]
    ids = (src / "ID_troops.py").read_text()
    assert ids != stale_ids.decode()
    assert "trp_watchman = 26\n" in ids and "trp_townsman = 27\n" in ids

    second = build(src, tmp_path / "out2", quiet=True)
    assert second.passes == 1
    assert second.ids_differ_from_source == []
    assert read_output(tmp_path / "out2") == read_output(tmp_path / "out1")
    assert second.ids == first.ids


def test_new_troop_used_by_new_party(tmp_path: Path) -> None:
    src = copy_source(tmp_path / "src")
    edit_once(
        src / "module_troops.py",
        '\nupgrade(troops,"farmer", "watchman")',
        '\ntroops.append(["new_recruit","New Recruit","New Recruits",tf_guarantee_armor,'
        "no_scene,reserved,fac_commoners,[itm_club],def_attrib|level(4),wp(60),knows_common,"
        "man_face_middle_1,man_face_old_2])"
        '\nupgrade(troops,"farmer", "watchman")',
    )
    edit_once(
        src / "module_parties.py",
        "(0., 0),[(trp_looter,15,0)]),\n  ]",
        "(0., 0),[(trp_looter,15,0)]),\n"
        '  ("new_party","New Party",pf_disabled, no_menu, pt_none, fac_commoners,0,'
        "ai_bhvr_hold,0,(0., 0),[(trp_new_recruit,5,0)]),\n  ]",
    )
    result = build(src, tmp_path / "out", quiet=True)
    assert result.passes == 2
    assert "trp_new_recruit = 1072\n" in result.ids["ID_troops.py"].decode()
    assert "p_new_party = 240\n" in result.ids["ID_parties.py"].decode()
    assert b"p_new_party " in (tmp_path / "out" / "parties.txt").read_bytes()


NEW_ITEM_ANCHOR = '["ccoop_new_items_end", "Items End", [("shield_round_a",0)], 0, 0, 1, 0, 0],\n'
NEW_ITEM = '["new_thing", "New Thing", [("shield_round_a",0)], 0, 0, 1, 0, 0],\n'


def test_new_item_used_by_troop_converges(tmp_path: Path) -> None:
    # process_items.py writes ID_items.py before it imports process_operations (which
    # imports module_troops), so the stale reference is repaired within the first pass.
    src = copy_source(tmp_path / "src")
    edit_once(src / "module_items.py", NEW_ITEM_ANCHOR, NEW_ITEM_ANCHOR + NEW_ITEM)
    edit_once(
        src / "module_troops.py",
        "[itm_cleaver,itm_knife,itm_club,itm_quarter_staff",
        "[itm_new_thing,itm_cleaver,itm_knife,itm_club,itm_quarter_staff",
    )
    out = tmp_path / "out"
    result = build(src, out, quiet=True)
    assert result.passes == 2
    assert "itm_new_thing = 620\n" in result.ids["ID_items.py"].decode()
    assert b" itm_new_thing " in (out / "item_kinds1.txt").read_bytes()


def test_item_referenced_from_constants_deadlocks(tmp_path: Path) -> None:
    # module_items imports module_constants before ID_items.py can be regenerated.
    src = copy_source(tmp_path / "src")
    edit_once(src / "module_items.py", NEW_ITEM_ANCHOR, NEW_ITEM_ANCHOR + NEW_ITEM)
    edit_once(
        src / "module_constants.py",
        "from ID_factions import *\n",
        "from ID_factions import *\nnew_thing_marker = itm_new_thing\n",
    )
    out = tmp_path / "out"
    with pytest.raises(BuildError) as info:
        build(src, out, quiet=True)
    report = info.value.report()
    assert "NameError" in report and "itm_new_thing" in report
    assert info.value.passes == 1  # no ID changed in pass 1, so a rerun cannot help
    assert not out.exists()
