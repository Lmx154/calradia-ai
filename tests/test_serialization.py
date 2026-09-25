"""Unit tests of the legacy serialization helpers, with Python 2 expectations.

The legacy modules are flat star-import scripts with import-time side effects, so each
test imports them in a fresh interpreter (``-B``: no bytecode written into game/) with
game/module_system on sys.path and returns results as JSON.
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import textwrap
from pathlib import Path
from typing import Any

import pytest
from golden_compare import SOURCE

_MARK = "@@RESULT@@"


def run_legacy(code: str, tmp_path: Path) -> Any:
    env = {k: v for k, v in os.environ.items() if k not in ("PYTHONPATH", "PYTHONSAFEPATH")}
    env["WARBAND_EXPORT_DIR"] = tmp_path.as_posix() + "/"
    prelude = f"import json, sys\nsys.path.insert(0, {str(SOURCE)!r})\n"
    epilogue = f"\nprint({_MARK!r} + json.dumps(result))\n"
    proc = subprocess.run(
        [sys.executable, "-B", "-c", prelude + textwrap.dedent(code) + epilogue],
        cwd=tmp_path,
        env=env,
        capture_output=True,
        text=True,
        check=False,
    )
    assert proc.returncode == 0, proc.stderr
    lines = [line for line in proc.stdout.splitlines() if line.startswith(_MARK)]
    assert len(lines) == 1, proc.stdout
    return json.loads(lines[0][len(_MARK) :])


def test_opmask_values(tmp_path: Path) -> None:
    result = run_legacy(
        """
        from header_common import *
        result = [op_num_value_bits, opmask_register, opmask_variable, opmask_quest_index,
                  opmask_local_variable, opmask_quick_string, tag_troop, tag_item, tags_end]
        """,
        tmp_path,
    )
    assert result == [
        56,
        72057594037927936,
        144115188075855872,
        504403158265495552,
        1224979098644774912,
        1585267068834414592,
        5,
        4,
        26,
    ]


def test_operation_opcodes(tmp_path: Path) -> None:
    result = run_legacy(
        """
        from header_operations import *
        result = [call_script, try_end, try_begin, eq, gt, display_message, assign,
                  neg, this_or_next, neg | this_or_next | eq, neg | call_script]
        """,
        tmp_path,
    )
    assert result == [1, 3, 4, 31, 32, 1106, 2133, 0x80000000, 0x40000000, 3221225503, 2147483649]


def test_process_param_operands(tmp_path: Path) -> None:
    result = run_legacy(
        """
        from process_operations import *
        from header_common import tags_end
        tag_uses = [[] for _ in range(tags_end)]
        quick_strings = []
        gvars, guses = ["a", "var"], [0, 0]
        lvars, luses = ["x", "local"], [0, 0]
        def p(param):
            return process_param(param, gvars, guses, lvars, luses, tag_uses, quick_strings)
        result = {
            "global": p("$var"),
            "local": p(":local"),
            "quick": p("@quick string"),
            "quick_again": p("@quick string"),
            "troop": p("trp_player"),
            "troop_mixed_case": p("trp_Player"),
            "item": p("itm_heraldic_mail_with_surcoat"),
            "negative": p(-1),
            "large": p(0x7FFFFFFFFFFFFFFF),
            "quick_strings": quick_strings,
            "global_uses": guses,
            "local_uses": luses,
        }
        """,
        tmp_path,
    )
    assert result == {
        "global": 144115188075855873,
        "local": 1224979098644774913,
        "quick": 1585267068834414592,
        "quick_again": 1585267068834414592,
        "troop": 360287970189639680,
        "troop_mixed_case": 360287970189639680,
        "item": 288230376151712034,
        "negative": -1,
        "large": 9223372036854775807,
        "quick_strings": [["qstr_quick_string", "quick_string"]],
        "global_uses": [0, 1],
        "local_uses": [0, 1],
    }


def test_statement_block_serialization(tmp_path: Path) -> None:
    result = run_legacy(
        """
        import io
        from process_operations import *
        from header_common import tags_end
        out = io.StringIO()
        block = [(assign, "$var", 5), (try_begin), (neg | eq, ":v", -3), (try_end)]
        save_statement_block(out, "test", 1, [(assign, ":v", 1)] + block,
                             ["a", "var"], [0, 0], [[] for _ in range(tags_end)], [])
        result = out.getvalue()
        """,
        tmp_path,
    )
    assert result == (
        " 5 2133 2 1224979098644774912 1 2133 2 144115188075855873 5 4 0 "
        "2147483679 2 1224979098644774912 -3 3 0 "
    )


def test_identifier_escaping(tmp_path: Path) -> None:
    result = run_legacy(
        """
        from process_common import *
        s = "Lord's (Old)-Keep, of|the\\tNorth`s"
        result = [convert_to_identifier(s), convert_to_identifier_with_no_lowercase(s),
                  replace_spaces("a b\\tc  d")]
        """,
        tmp_path,
    )
    assert result == [
        "lord_s__old__keep_ofthe_north_s",
        "Lord_s__Old__Keep_ofthe_North_s",
        "a_b_c__d",
    ]


def test_carries_gold_floor(tmp_path: Path) -> None:
    # header_parties.carries_gold refers to an undefined `big_num` (upstream typo; the
    # function is never called by Native). Bind it to header_common.bignum, the value
    # carries_goods uses, to exercise the x // 20 floor.
    result = run_legacy(
        """
        import header_parties
        header_parties.big_num = header_parties.bignum
        result = [header_parties.carries_gold(x) for x in (250, 259, 19, 20, 10000, -5, 20000)]
        """,
        tmp_path,
    )
    assert result == [
        12 << 56,  # 250 // 20 == 12 (Py2 250 / 20 == 12)
        12 << 56,  # 259 // 20 == 12
        0,
        1 << 56,
        244 << 56,  # 10000 // 20 == 500, masked to 8 bits
        0,  # clamped to 0
        244 << 56,  # clamped to 10000
    ]
    assert result[0] == 864691128455135232
    assert result[4] == 17582052945254416384


def test_troop_proficiency_floor_for_negative_values(tmp_path: Path) -> None:
    # In Py2, `int(x / 10)` on ints floors: -15 / 10 == -2 (Py3 int(-1.5) would give -1).
    # module_troops.wp computes r = 10 + floor(x / 10); r is not returned, so trace it.
    result = run_legacy(
        """
        import sys
        import module_troops
        seen = []
        def tracer(frame, event, arg):
            if frame.f_code.co_name in ("wp", "wp_melee"):
                def local(frame, event, arg):
                    if event == "return":
                        seen.append(frame.f_locals["r"])
                    return local
                return local
            return None
        result = {}
        for x in (-15, -10, -1, 0, 9, 15, 100):
            seen.clear()
            sys.settrace(tracer)
            module_troops.wp(x)
            module_troops.wp_melee(x)
            sys.settrace(None)
            result[str(x)] = list(seen)
        result["wp60"] = module_troops.wp(60)
        """,
        tmp_path,
    )
    expected_r = {"-15": 8, "-10": 9, "-1": 9, "0": 10, "9": 10, "15": 11, "100": 20}
    wp60 = result.pop("wp60")
    assert result == {x: [r, r] for x, r in expected_r.items()}
    assert wp60 > 0


@pytest.mark.parametrize(
    ("value", "expected"),
    [
        (0.1, "0.100000 "),
        (-1.5, "-1.500000 "),
        (1e-7, "0.000000 "),
        (-1e-7, "-0.000000 "),
        (2.5e10, "25000000000.000000 "),
        (1 / 3, "0.333333 "),
        (3, "3.000000 "),
    ],
)
def test_float_formatting_matches_py2(tmp_path: Path, value: float, expected: str) -> None:
    # save_simple_triggers writes the trigger interval with "%f " (Py2: "%f" % 0.1 == "0.100000")
    result = run_legacy(
        f"""
        import io
        from process_operations import *
        from header_common import tags_end
        out = io.StringIO()
        save_simple_triggers(out, [({value!r}, [])], [], [], [[] for _ in range(tags_end)], [])
        result = out.getvalue()
        """,
        tmp_path,
    )
    assert result == f"1\n{expected} 0 \n\n"
