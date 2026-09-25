"""Identifier stability: the generated ID_*.py files parse to the reference values."""

from __future__ import annotations

import ast

import pytest
from golden_compare import GOLDEN_IDS, read_tree


def parse_ids(data: bytes) -> dict[str, int]:
    """Bind names exactly as `from ID_x import *` would (repeats: last value wins).

    Every statement must be an assignment of an integer literal to plain names; note that
    upstream emits chained assignments such as `str_..._temp_=_reg4 = 1900`.
    """
    ids: dict[str, int] = {}
    for node in ast.parse(data.decode("ascii")).body:
        assert isinstance(node, ast.Assign), ast.dump(node)
        value = node.value
        if isinstance(value, ast.UnaryOp) and isinstance(value.op, ast.USub):
            value = value.operand
            sign = -1
        else:
            sign = 1
        assert isinstance(value, ast.Constant) and type(value.value) is int, ast.dump(node)
        for target in node.targets:
            assert isinstance(target, ast.Name), ast.dump(node)
            ids[target.id] = sign * value.value
    return ids


EXPECTED = {
    "ID_troops.py": {
        "trp_player": 0,
        "trp_townsman": 26,
        "trp_watchman": 27,
        "trp_kingdom_1_lord": 210,
        "trp_knight_1_1": 216,
        "trp_coop_companion_equipment_sets_end": 1071,
    },
    "ID_factions.py": {"fac_no_faction": 0, "fac_commoners": 1, "fac_player_faction": 13},
    "ID_items.py": {
        "itm_no_item": 0,
        "itm_heraldic_mail_with_surcoat": 290,
        "itm_ccoop_new_items_end": 619,
    },
    "ID_parties.py": {"p_main_party": 0, "p_town_1": 21, "p_reserved_5": 239},
    "ID_party_templates.py": {"pt_none": 0, "pt_looters": 6, "pt_leaded_looters": 62},
    "ID_quests.py": {"qst_deliver_message": 0, "qst_deliver_message_to_enemy_lord": 1},
    "ID_menus.py": {"menu_start_game_0": 0, "menu_notification_relieved_as_marshal": 216},
    "ID_scenes.py": {"scn_random_scene": 0, "scn_town_1_center": 54},
    "ID_scripts.py": {
        "script_game_start": 0,
        "script_game_event_party_encounter": 9,
        "script_game_get_money_text": 31,
    },
    "ID_strings.py": {"str_no_string": 0, "str_empty_string": 1},
    "ID_skills.py": {"skl_trade": 0},
    "ID_sounds.py": {"snd_click": 0},
    "ID_presentations.py": {"prsnt_game_credits": 0},
    "ID_mission_templates.py": {"mst_multiplayer_duel": 49},
    "ID_scene_props.py": {"spr_multiplayer_coop_item_drop": 1045},
    "ID_map_icons.py": {"icon_bandit_lair": 188},
}

GOLDEN = {name: parse_ids(data) for name, data in read_tree(GOLDEN_IDS).items()}


@pytest.mark.parametrize("name", sorted(EXPECTED))
def test_selected_ids_in_golden(name: str) -> None:
    ids = GOLDEN[name]
    assert {k: ids.get(k) for k in EXPECTED[name]} == EXPECTED[name]


@pytest.mark.parametrize("name", sorted(EXPECTED))
def test_selected_ids_in_build(vanilla, name: str) -> None:
    ids = parse_ids(vanilla.result.ids[name])
    assert {k: ids.get(k) for k in EXPECTED[name]} == EXPECTED[name]


@pytest.mark.parametrize("name", sorted(GOLDEN))
def test_built_ids_equal_golden(vanilla, name: str) -> None:
    assert name in vanilla.result.ids
    assert parse_ids(vanilla.result.ids[name]) == GOLDEN[name]


def test_same_id_files(vanilla) -> None:
    assert sorted(vanilla.result.ids) == sorted(GOLDEN)
