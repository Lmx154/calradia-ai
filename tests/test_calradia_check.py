"""tools/calradia_check.py: the engine emulation the desktop agent's pipeline check uses.

The check replays the mod's compiled request templates against calradia-server; these tests
pin how it fills them (as send_message_to_url with encode_url=1 does) and how it judges
answers (as the mod's callback does). test_calradia_ai.py checks that it fills every
template of the built mod exactly.
"""

from __future__ import annotations

import pytest
from support import load_local_check

from modsys.build import MARKER_NAME

check = load_local_check()


def test_values_are_encoded_like_the_engine() -> None:
    assert check.engine_encode("Ylva") == "Ylva"
    assert check.engine_encode(-5) == "%2D5"
    assert check.engine_encode("King Harlaus") == "King%20Harlaus"
    assert check.engine_encode(".1.0") == "%2E1%2E0"
    assert check.engine_encode("a_b?&=") == "a%5Fb%3F%26%3D"


def test_templates_are_filled_exactly() -> None:
    base = check.TEMPLATE_BASE
    template = f"{base}v2/tick?v=2&rid={{reg60}}&day={{reg63}}&pname={{s65}}&end=1"
    url = check.fill(template, {"rid": 7, "day": -1, "pname": "A B"}, 9000)
    assert url == "http://127.0.0.1:9000/v2/tick?v=2&rid=7&day=%2D1&pname=A%20B&end=1"
    with pytest.raises(check.TemplateDrift, match=r"\['pname'\]"):
        check.fill(template, {"rid": 7, "day": 1}, 9000)
    with pytest.raises(check.TemplateDrift, match=r"\['head'\]"):
        check.fill(template, {"rid": 7, "day": 1, "pname": "A", "head": 0}, 9000)


def test_templates_are_found_in_quick_strings() -> None:
    quick = (
        "3\n"
        f"qstr_a {check.TEMPLATE_BASE}v1/result?v=1&rid={{reg60}}&job={{reg61}}&end=1\n"
        "qstr_b Talk_with_Hrodvar.\n"
        "qstr_c http://example.com/v2/talk?x=1\n"
    )
    assert list(check.load_templates(quick)) == ["v1/result"]


@pytest.mark.parametrize(
    ("body", "expected"),
    [
        ("5|0|Aye.|5", (0, "Aye.", None)),
        ("5|1|pending|5", (1, "pending", None)),
        ("5|0|stored|10|0|210|0|5", (0, "stored", (10, 0, 210, 0))),
        ("5|0|Take it.|2|300|210|0|5", (0, "Take it.", (2, 300, 210, 0))),
    ],
)
def test_good_frames(body: str, expected: tuple) -> None:
    frame = check.parse_frame(body, 5)
    assert (frame.code, frame.text, frame.extra) == expected


@pytest.mark.parametrize(
    ("body", "problem"),
    [
        ("6|0|Aye.|6", "request id"),
        ("5|0|Aye.|6", "request id"),
        ("5|9|Aye.|5", "code"),
        ("5|0|Aye.|1|2|5", "fields"),
        ("5|0||5", "empty"),
        ("5|0|123|5", "no letter"),
        ("5|0|Café|5", "not ASCII"),
        ("5|0|a {s1}|5", "engine characters"),
        ("5|0|a^b|5", "engine characters"),
        ("5|0|" + "a" * 501 + "|5", "over 500"),
        ("5|x|Aye.|5", "non-numeric"),
        ("5|0|" + "a" * 600 + "|5", "frame of"),
    ],
)
def test_bad_frames(body: str, problem: str) -> None:
    with pytest.raises(ValueError, match=problem):
        check.parse_frame(body, 5)


def test_shared_values_come_from_the_sources() -> None:
    assert check.BUILD_MARKER == MARKER_NAME
    assert (check.P["CODE_READY"], check.P["CODE_PENDING"], check.P["CODE_BUSY"]) == (0, 1, 6)
    assert check.P["MAX_TEXT"] == 500
    assert check.rust_default("DEFAULT_MODEL")
    assert check.rust_default("DEFAULT_UPSTREAM").startswith("http://")
    assert check.module_constant("logent_lord_defeated_by_player") == 11
    assert check.module_constant("slto_player_companion") == 5


def test_snapshot_lists_have_the_servers_shape() -> None:
    world = check.World()
    game = check.Game({}, 1, world)
    snap = world.snapshot(game.wars, game.owners, {world.troops["trp_knight_1_1"]})
    # protocol.rs snapshot_shape: 7 realms (21 pairs), 70 walled centers, 132 lords.
    assert snap["wars"].count(".") == 21 and snap["wars"].count(".1") == 1
    assert snap["owners"].count(".") == 70
    assert snap["lords"].count(".") == 66 and snap["lords2"].count(".") == 66
    assert snap["alive"] == 0b1111110
    swadia = world.factions["fac_kingdom_1"]
    klargus = list(world.lords).index(world.troops["trp_knight_1_1"])
    assert snap["lords"].split(".")[1 + klargus] == str(swadia * 2 + 1)
    # Talk field `wars`: bit k = realm fac_player_supporters_faction + k.
    vaegirs = world.factions["fac_kingdom_2"]
    assert world.wars_mask(swadia, game.wars) == 1 << world.realms.index(vaegirs)


def test_the_game_tracks_heads_like_the_mod() -> None:
    """READY advances the heads; a change of regard counts as done, gold as refused."""
    game = check.Game({}, 1, check.World())
    replies = iter(
        [
            check.Frame(0, "Hm.", (0, 0, 0, 0)),
            check.Frame(0, "Take it.", (check.P["ACT_GIVE"], 50, 210, 0)),
            check.Frame(0, "Fine.", (check.P["ACT_RELATION"], 1, 210, 0)),
            check.Frame(2, "timeout", (0, 0, 0, 0)),
        ]
    )
    sent = []

    def send(route: str, **values: object) -> check.Frame:
        sent.append(values)
        return next(replies)

    game.send = send
    outcomes = []
    for n in range(4):
        game.talk("trp_kingdom_1_lord", "Hello", n)
        outcomes.append((game.head, game.outcome))
    heads = [values["job"] for values in sent]
    assert [values["hres"] for values in sent] == [0, 0, check.P["OUT_DECLINED"],
                                                   check.P["OUT_ACCEPTED"]]  # fmt: skip
    assert outcomes[:3] == [
        (heads[0], 0),
        (heads[1], check.P["OUT_DECLINED"]),
        (heads[2], check.P["OUT_ACCEPTED"]),
    ]
    assert outcomes[3] == outcomes[2]  # a failed talk shows nothing
