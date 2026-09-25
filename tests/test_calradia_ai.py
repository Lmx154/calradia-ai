"""CalradiaAI mod (vanilla + mods/calradia_ai/overlay): builds, and changes only what it should.

The mod adds one camp-menu option, the presentation prsnt_cai_talk, the scripts cai_new_id and
cai_tx_send, a new body for script_game_receive_url_response, and cai_* global variables
(docs/protocol-v1.md). Everything else must stay vanilla:
- files the mod does not touch are byte-identical to the golden export;
- in the touched files, vanilla content differs only by renumbered quick-string operands
  (tag_quick_string), following the exact old -> new index mapping of quick_strings.txt;
- new presentations and scripts are appended, so no vanilla ID moves.
The mod's own code is checked against an opcode whitelist and may only write cai_* globals,
so it cannot change game state, and send_message_to_url (opcode 380) exists only in
script_cai_tx_send.
"""

from __future__ import annotations

import re
from collections.abc import Iterator

import pytest
from golden_compare import GOLDEN_EXPORT, REPO, SOURCE
from support import read_output

from modsys.build import build

OVERLAY = REPO / "mods" / "calradia_ai" / "overlay"
PROTOCOL_RS = REPO / "calradia-server" / "src" / "protocol.rs"
PROTOCOL_DOC = REPO / "docs" / "protocol-v1.md"

OP_NUM_VALUE_BITS = 56  # header_common.op_num_value_bits
TAG_VARIABLE, TAG_SCRIPT, TAG_PRESENTATION, TAG_QUICK_STRING = 2, 13, 21, 22
OPCODE_FLAGS = 0x80000000 | 0x40000000  # neg | this_or_next
SEND_MESSAGE_TO_URL = 380

# The mod's global variables, appended after the vanilla list in this order.
NEW_VARIABLES = [
    b"cai_tx_rid",
    b"cai_tx_prev_rid",
    b"cai_tx_op",
    b"cai_tx_job",
    b"cai_tx_sent_ms",
    b"cai_tx_inst",
    b"cai_prsnt_inst",
    b"cai_now_ms",
    b"cai_conv_state",
    b"cai_conv_job",
    b"cai_conv_start_ms",
    b"cai_cancel_owed_job",
    b"cai_reply_new",
    b"cai_ui_warn",
    b"cai_obj_reply",
    b"cai_obj_status",
    b"cai_obj_warn",
    b"cai_obj_text_box",
    b"cai_obj_say",
    b"cai_obj_cancel",
    b"cai_obj_reset",
    b"cai_obj_close",
    b"cai_tx_abandoned",
]
NEW_SCRIPTS = [b"cai_new_id", b"cai_tx_send"]
CHANGED_SCRIPT = b"game_receive_url_response"
NEW_PRESENTATION = b"prsnt_cai_talk"
MENU_OPTION = b"mno_cai_talk"

# Files whose only permitted differences are renumbered quick-string operands.
QSTR_ONLY = ["conversation.txt", "mission_templates.txt", "simple_triggers.txt", "triggers.txt"]
CHANGED = {
    "menus.txt",
    "presentations.txt",
    "scripts.txt",
    "quick_strings.txt",
    "variables.txt",
    "variable_uses.txt",
}

# Everything the mod's own code may execute: flow control, arithmetic on locals, registers
# and cai_* globals, string registers, reading the player's name and the day, the HTTP send,
# and its own presentation's overlays. Nothing that writes troops, parties, factions, items,
# slots, quests or other game state.
ALLOWED_OPERATIONS = {
    "call_script",
    "try_begin",
    "else_try",
    "try_end",
    "try_for_range",
    "store_script_param",
    "store_trigger_param_1",
    "ge",
    "eq",
    "gt",
    "is_between",
    "assign",
    "val_add",
    "val_sub",
    "val_mod",
    "store_sub",
    "store_mul",
    "store_random_in_range",
    "store_current_day",
    "str_is_empty",
    "str_clear",
    "str_store_string",
    "str_store_string_reg",
    "str_store_troop_name",
    "send_message_to_url",
    "key_clicked",
    "set_fixed_point_multiplier",
    "position_set_x",
    "position_set_y",
    "start_presentation",
    "presentation_set_duration",
    "create_text_overlay",
    "create_button_overlay",
    "create_simple_text_box_overlay",
    "create_text_box_overlay",
    "overlay_set_text",
    "overlay_set_color",
    "overlay_set_alpha",
    "overlay_set_size",
    "overlay_set_position",
    "overlay_set_area_size",
    "overlay_set_display",
}

Op = tuple[int, list[int]]


@pytest.fixture(scope="module")
def mod(tmp_path_factory: pytest.TempPathFactory) -> dict[str, bytes]:
    out = tmp_path_factory.mktemp("calradia_ai") / "CalradiaAI"
    result = build(SOURCE, out, overlay=OVERLAY, quiet=True)
    assert result.passes == 1
    assert not result.warnings
    return read_output(out)


def golden(name: str) -> bytes:
    return (GOLDEN_EXPORT / name).read_bytes()


# --- compiled-file parsing -------------------------------------------------------------


def tag(value: int) -> int:
    return value >> OP_NUM_VALUE_BITS


def index(value: int) -> int:
    return value & ((1 << OP_NUM_VALUE_BITS) - 1)


def parse_block(tokens: list[bytes], i: int) -> tuple[list[Op], int]:
    """An operation block "N op argc args... op argc args..." starting at tokens[i]."""
    ops: list[Op] = []
    count, i = int(tokens[i]), i + 1
    for _ in range(count):
        opcode, argc = int(tokens[i]), int(tokens[i + 1])
        ops.append((opcode, [int(t) for t in tokens[i + 2 : i + 2 + argc]]))
        i += 2 + argc
    return ops, i


def scripts(data: bytes) -> list[tuple[bytes, list[Op]]]:
    """scripts.txt -> [(name, ops)], checking the declared count."""
    lines = data.split(b"\n")
    count = int(lines[1])
    result = []
    for k in range(count):
        name = lines[2 + 2 * k].split()[0]
        body = lines[3 + 2 * k].split()
        ops, end = parse_block(body, 0)
        assert end == len(body), name
        result.append((name, ops))
    assert all(not line.strip() for line in lines[2 + 2 * count :])
    return result


def presentations(data: bytes) -> list[tuple[list[bytes], list[tuple[bytes, list[Op]]]]]:
    """presentations.txt -> [(header tokens, [(trigger interval, ops)])]."""
    lines = data.split(b"\n")
    count, i, result = int(lines[1]), 2, []
    for _ in range(count):
        header = lines[i].split()
        triggers = []
        for line in lines[i + 1 : i + 1 + int(header[3])]:
            tokens = line.split()
            ops, end = parse_block(tokens, 1)
            assert end == len(tokens), header
            triggers.append((tokens[0], ops))
        result.append((header, triggers))
        i += 1 + int(header[3])
        assert lines[i : i + 2] == [b"", b""], header  # two blank lines per presentation
        i += 2
    assert all(not line.strip() for line in lines[i:])
    return result


def menu_option(tokens: list[bytes], k: int) -> tuple[list[Op], bytes, list[Op], bytes, int]:
    """Menu option at tokens[k] (its id): conditions, text, consequences, door text, end."""
    conditions, i = parse_block(tokens, k + 1)
    text = tokens[i]
    consequences, j = parse_block(tokens, i + 1)
    return conditions, text, consequences, tokens[j], j + 1


def quick_strings(data: bytes) -> list[bytes]:
    lines = data.split(b"\n")
    count = int(lines[0])
    assert all(not line.strip() for line in lines[1 + count :])
    return lines[1 : 1 + count]


def names(data: bytes) -> list[bytes]:
    return data.split()


def header_operations() -> dict[str, int]:
    text = (SOURCE / "header_operations.py").read_text(encoding="cp1254")
    return {m[1]: int(m[2]) for m in re.finditer(r"^(\w+)\s*=\s*(\d+)\b", text, re.M)}


# --- quick-string renumbering ---------------------------------------------------------


@pytest.fixture(scope="module")
def qmap(mod: dict[str, bytes]) -> dict[int, int]:
    """Golden quick-string index -> mod index. Vanilla entries keep their relative order."""
    old, new = quick_strings(golden("quick_strings.txt")), quick_strings(mod["quick_strings.txt"])
    position = {entry: i for i, entry in enumerate(new)}
    assert len(position) == len(new)
    mapping = {i: position[entry] for i, entry in enumerate(old)}
    assert list(mapping.values()) == sorted(mapping.values())
    return mapping


def same_but_renumbered(a: bytes, b: bytes, qmap: dict[int, int]) -> bool:
    """Equal token by token, except quick-string operands renumbered exactly per qmap."""
    ta, tb = a.split(), b.split()
    if len(ta) != len(tb):
        return False
    for x, y in zip(ta, tb, strict=True):
        if x == y:
            continue
        try:
            vx, vy = int(x), int(y)
        except ValueError:
            return False
        if not (tag(vx) == tag(vy) == TAG_QUICK_STRING and qmap.get(index(vx)) == index(vy)):
            return False
    return True


# --- the mod's own code ---------------------------------------------------------------


def mod_code(mod: dict[str, bytes]) -> Iterator[tuple[str, list[Op]]]:
    """(where, ops) for every operation block the mod contributes."""
    for name, ops in scripts(mod["scripts.txt"]):
        if name in NEW_SCRIPTS or name == CHANGED_SCRIPT:
            yield f"script {name.decode()}", ops
    for header, triggers in presentations(mod["presentations.txt"]):
        if header[0] == NEW_PRESENTATION:
            for interval, ops in triggers:
                yield f"presentation trigger {interval.decode()}", ops
    for line in mod["menus.txt"].split(b"\n"):
        tokens = line.split()
        if MENU_OPTION in tokens:
            conditions, _, consequences, _, _ = menu_option(tokens, tokens.index(MENU_OPTION))
            yield "menu option conditions", conditions
            yield "menu option consequences", consequences


def script_index(mod: dict[str, bytes], name: bytes) -> int:
    return [n for n, _ in scripts(mod["scripts.txt"])].index(name)


def presentation_index(mod: dict[str, bytes], name: bytes) -> int:
    return [h[0] for h, _ in presentations(mod["presentations.txt"])].index(name)


# --- tests: untouched and renumbered files --------------------------------------------


def test_untouched_files_are_vanilla(mod: dict[str, bytes]) -> None:
    assert set(mod) == {p.name for p in GOLDEN_EXPORT.iterdir()}
    for name in sorted(set(mod) - CHANGED - set(QSTR_ONLY)):
        assert mod[name] == golden(name), name


@pytest.mark.parametrize("name", QSTR_ONLY)
def test_only_quick_string_indices_move(
    mod: dict[str, bytes], qmap: dict[int, int], name: str
) -> None:
    assert same_but_renumbered(golden(name), mod[name], qmap)


def test_new_quick_strings_are_the_mods_own(mod: dict[str, bytes], qmap: dict[int, int]) -> None:
    new_indices = set(range(len(quick_strings(mod["quick_strings.txt"])))) - set(qmap.values())
    used = {
        index(arg)
        for _, ops in mod_code(mod)
        for _, args in ops
        for arg in args
        if tag(arg) == TAG_QUICK_STRING
    }
    assert new_indices and new_indices <= used


# --- tests: variables -----------------------------------------------------------------


def test_vanilla_global_variables_keep_their_indices(mod: dict[str, bytes]) -> None:
    expected = golden("variables.txt") + b"".join(v + b"\n" for v in NEW_VARIABLES)
    assert mod["variables.txt"] == expected
    assert (OVERLAY / "variables.txt").read_bytes() == expected
    assert all(v.startswith(b"cai_") for v in NEW_VARIABLES)


def test_variable_use_counts(mod: dict[str, bytes]) -> None:
    old, new = names(golden("variable_uses.txt")), names(mod["variable_uses.txt"])
    variables = names(golden("variables.txt"))
    # Build artifact: the vanilla variables that the overlay's variables.txt seeds (those
    # missing from game/module_system/variables.txt) are counted once more.
    seeded = set(variables) - set(names((SOURCE / "variables.txt").read_bytes()))
    assert seeded
    expected = [int(n) + (variables[i] in seeded) for i, n in enumerate(old)]
    assert [int(n) for n in new[: len(old)]] == expected
    assert len(new) == len(old) + len(NEW_VARIABLES)
    assert all(int(n) > 0 for n in new[len(old) :])


# --- tests: scripts, presentations, menus ---------------------------------------------


def test_only_the_url_response_script_changes_and_new_scripts_are_appended(
    mod: dict[str, bytes], qmap: dict[int, int]
) -> None:
    a_lines, b_lines = golden("scripts.txt").split(b"\n"), mod["scripts.txt"].split(b"\n")
    a, b = scripts(golden("scripts.txt")), scripts(mod["scripts.txt"])
    assert [name for name, _ in b] == [name for name, _ in a] + NEW_SCRIPTS
    changed = []
    for k, (name, _) in enumerate(a):
        assert a_lines[2 + 2 * k] == b_lines[2 + 2 * k], name
        if not same_but_renumbered(a_lines[3 + 2 * k], b_lines[3 + 2 * k], qmap):
            changed.append(name)
    assert changed == [CHANGED_SCRIPT]


def test_vanilla_presentations_unchanged_and_new_one_appended(
    mod: dict[str, bytes], qmap: dict[int, int]
) -> None:
    a_lines = golden("presentations.txt").split(b"\n")
    b_lines = mod["presentations.txt"].split(b"\n")
    a, b = presentations(golden("presentations.txt")), presentations(mod["presentations.txt"])
    assert [h[0] for h, _ in b] == [h[0] for h, _ in a] + [NEW_PRESENTATION]
    assert b_lines[0] == a_lines[0]
    vanilla_lines = sum(3 + len(triggers) for _, triggers in a)
    for x, y in zip(a_lines[2 : 2 + vanilla_lines], b_lines[2 : 2 + vanilla_lines], strict=True):
        assert same_but_renumbered(x, y, qmap)
    header, triggers = b[-1]
    load_window = [h for h, _ in a if h[0] == b"prsnt_name_kingdom"][0][2]
    assert header[1:3] == [b"2", load_window]  # prsntf_manual_end_only, mesh_load_window
    assert [t for t, _ in triggers] == [b"-60.000000", b"-61.000000", b"-62.000000"]


def test_camp_menu_gets_one_option_that_opens_the_presentation(
    mod: dict[str, bytes], qmap: dict[int, int]
) -> None:
    a_lines, b_lines = golden("menus.txt").split(b"\n"), mod["menus.txt"].split(b"\n")
    assert len(a_lines) == len(b_lines)
    prsnt = (TAG_PRESENTATION << OP_NUM_VALUE_BITS) | presentation_index(mod, NEW_PRESENTATION)
    options = [i for i, line in enumerate(b_lines) if MENU_OPTION in line.split()]
    assert len(options) == 1
    k = options[0]
    tokens = b_lines[k].split()
    at = tokens.index(MENU_OPTION)
    conditions, text, consequences, door, end = menu_option(tokens, at)
    opens = [(header_operations()["start_presentation"], [prsnt])]
    assert (conditions, text, consequences, door) == ([], b"Talk_with_Hrodvar.", opens, b".")
    assert tokens[end] == b"mno_resume_travelling"
    # The option line minus the new option, and the menu line with one option fewer, are vanilla.
    assert same_but_renumbered(a_lines[k], b" ".join(tokens[:at] + tokens[end:]), qmap)
    menu = b_lines[k - 1].split()
    assert menu[0] == b"menu_camp"
    one_fewer = menu[:-1] + [b"%d" % (int(menu[-1]) - 1)]
    assert same_but_renumbered(a_lines[k - 1], b" ".join(one_fewer), qmap)
    for i, (x, y) in enumerate(zip(a_lines, b_lines, strict=True)):
        if i not in (k - 1, k):
            assert same_but_renumbered(x, y, qmap), i


# --- tests: the mod cannot change game state ------------------------------------------


def test_mod_code_uses_only_whitelisted_operations(mod: dict[str, bytes]) -> None:
    by_name = header_operations()
    allowed = {by_name[name] for name in ALLOWED_OPERATIONS}
    variables = names(mod["variables.txt"])
    own_scripts = {script_index(mod, name) for name in NEW_SCRIPTS}
    own_presentation = presentation_index(mod, NEW_PRESENTATION)
    blocks = list(mod_code(mod))
    assert len(blocks) == 3 + 3 + 2
    for where, ops in blocks:
        for opcode, args in ops:
            base = opcode & ~OPCODE_FLAGS
            assert base in allowed, (where, opcode)
            for arg in args:
                if tag(arg) == TAG_VARIABLE:
                    assert variables[index(arg)].startswith(b"cai_"), (where, opcode)
            if base == by_name["call_script"]:
                assert tag(args[0]) == TAG_SCRIPT and index(args[0]) in own_scripts, where
            if base == by_name["start_presentation"]:
                assert args == [(TAG_PRESENTATION << OP_NUM_VALUE_BITS) | own_presentation]


def test_send_message_to_url_only_in_cai_tx_send(mod: dict[str, bytes]) -> None:
    sends = [
        name
        for name, ops in scripts(mod["scripts.txt"])
        for opcode, _ in ops
        if opcode & ~OPCODE_FLAGS == SEND_MESSAGE_TO_URL
    ]
    sends += [
        header[0]
        for header, triggers in presentations(mod["presentations.txt"])
        for _, ops in triggers
        for opcode, _ in ops
        if opcode & ~OPCODE_FLAGS == SEND_MESSAGE_TO_URL
    ]
    assert sends == [b"cai_tx_send"] * 3
    # Every other compiled file is vanilla (checked above) and vanilla never sends:
    for path in sorted(SOURCE.glob("module_*.py")):
        code = [ln.split("#")[0] for ln in path.read_text(encoding="cp1254").splitlines()]
        assert not any("send_message_to_url" in ln for ln in code), path.name
    for path in sorted(OVERLAY.glob("module_*.py")):
        code = [ln.split("#")[0] for ln in path.read_text(encoding="cp1254").splitlines()]
        count = sum("send_message_to_url" in ln for ln in code)
        assert count == (3 if path.name == "module_scripts.py" else 0), path.name


def test_reply_delivery_is_guarded_by_request_id(mod: dict[str, bytes]) -> None:
    """Case (b) must not swallow wrong-rid frames before failure case (d)."""
    by_name = header_operations()
    ops = dict(scripts(mod["scripts.txt"]))[CHANGED_SCRIPT]
    tx_rid = (TAG_VARIABLE << OP_NUM_VALUE_BITS) | names(mod["variables.txt"]).index(b"cai_tx_rid")
    # The callback saves reg0 (the received rid) before evaluating the case table.
    rid = next(
        args[0]
        for opcode, args in ops
        if opcode == by_name["assign"] and args[1] == 1 << OP_NUM_VALUE_BITS
    )
    completions = [
        i
        for i, (opcode, args) in enumerate(ops)
        if opcode == by_name["assign"] and args == [tx_rid, 0]
    ]
    assert len(completions) == 2  # Delivery (b), then failure (d).
    delivery = completions[0]
    branch = max(i for i in range(delivery) if ops[i][0] == by_name["else_try"])
    assert (by_name["eq"], [rid, tx_rid]) in ops[branch + 1 : delivery]


# --- tests: URL templates and protocol constants --------------------------------------


def overlay_templates() -> list[str]:
    text = (OVERLAY / "module_scripts.py").read_text(encoding="cp1254")
    return re.findall(r'\(send_message_to_url, "@([^"]*)", 1\),', text)


def test_url_templates_follow_the_protocol(mod: dict[str, bytes]) -> None:
    templates = overlay_templates()
    doc = PROTOCOL_DOC.read_text(encoding="utf-8")
    expected = re.findall(r"^(/v1/\S+)$", doc, re.M)
    assert len(templates) == len(expected) == 3
    registers: dict[str, str] = {}
    for template, pattern in zip(templates, expected, strict=True):
        prefix, _, path = template.partition("/v1/")
        assert prefix == "http://127.0.0.1:8766"
        assert not set(template) & {"_", " ", "^"}
        # The doc's {regA}.. placeholders stand for one fixed register each.
        for placeholder, register in zip(
            re.findall(r"\{reg[A-Z]\}", pattern),
            re.findall(r"\{reg\d+\}", "/v1/" + path),
            strict=True,
        ):
            assert registers.setdefault(placeholder, register) == register
        concrete = re.sub(r"\{reg[A-Z]\}", lambda m: registers[m[0]], pattern)
        assert "/v1/" + path == concrete
        params = path.partition("?")[2].split("&")
        assert params[0] == f"v={mirrored_constants()['PROTOCOL_VERSION']}"
        assert params[1].startswith("rid={reg") and params[-1] == "end=1"
        first_text = next((i for i, p in enumerate(params) if "{s" in p), len(params) - 1)
        assert all("{reg" not in p for p in params[first_text:])
    assert len(set(registers.values())) == len(registers)
    # The compiled sends use exactly these quick strings, with encode_url = 1.
    table = quick_strings(mod["quick_strings.txt"])
    texts = []
    for name, ops in scripts(mod["scripts.txt"]):
        for opcode, args in ops:
            if name == b"cai_tx_send" and opcode == SEND_MESSAGE_TO_URL:
                assert tag(args[0]) == TAG_QUICK_STRING and args[1] == 1
                texts.append(table[index(args[0])].split(b" ", 1)[1].decode())
    assert texts == templates


def mirrored_constants() -> dict[str, int]:
    """The overlay's "# mirrors calradia-server/src/protocol.rs" block, without CAI_."""
    lines = (OVERLAY / "module_scripts.py").read_text(encoding="cp1254").splitlines()
    start = lines.index("# mirrors calradia-server/src/protocol.rs") + 1
    end = lines.index("", start)
    mirrored = {}
    for line in lines[start:end]:
        m = re.fullmatch(r"CAI_(\w+) = (-?\d+)", line)
        assert m, line
        mirrored[m[1]] = int(m[2])
    assert mirrored
    return mirrored


def test_protocol_constants_match_the_server() -> None:
    if not PROTOCOL_RS.exists():
        pytest.skip("calradia-server/src/protocol.rs does not exist yet")
    rust = {
        m[1]: int(m[2])
        for m in re.finditer(
            r"^\s*pub const (\w+)\s*:\s*\w+\s*=\s*(-?\d+)\s*;", PROTOCOL_RS.read_text(), re.M
        )
    }
    mirrored = mirrored_constants()
    assert {k: rust.get(k) for k in mirrored} == mirrored
    assert {k for k in rust if k.startswith("CODE_")} <= set(mirrored)


# --- tests: overlay sources -----------------------------------------------------------


def strip_mod_blocks(lines: list[str]) -> list[str]:
    """Remove every "# --- Calradia AI ..." .. "# --- end Calradia AI ---" block."""
    out, inside = [], False
    for line in lines:
        marker = line.strip()
        if marker.startswith("# --- Calradia AI"):
            assert not inside
            inside = True
        elif marker == "# --- end Calradia AI ---":
            assert inside
            inside = False
        elif not inside:
            out.append(line)
    assert not inside
    return out


@pytest.mark.parametrize(
    "name", ["module_game_menus.py", "module_presentations.py", "module_scripts.py"]
)
def test_overlay_sources_are_vanilla_outside_the_mod_blocks(name: str) -> None:
    base = (SOURCE / name).read_text(encoding="cp1254").splitlines()
    mine = (OVERLAY / name).read_text(encoding="cp1254").splitlines()
    assert strip_mod_blocks(mine) != mine
    if name == "module_scripts.py":
        # The upstream commented-out example body of game_receive_url_response goes.
        start = base.index("      #here is an example usage")
        end = base.index("      ]),", start)
        base = base[:start] + base[end:]
    assert strip_mod_blocks(mine) == base


@pytest.mark.parametrize(
    ("name", "prefix", "new"),
    [
        ("ID_scripts.py", "script_", NEW_SCRIPTS),
        ("ID_presentations.py", "prsnt_", [b"cai_talk"]),
    ],
)
def test_overlay_id_files_only_append(name: str, prefix: str, new: list[bytes]) -> None:
    base = (SOURCE / name).read_text(encoding="cp1254").splitlines()
    mine = (OVERLAY / name).read_text(encoding="cp1254").splitlines()
    ids = [i for i, line in enumerate(base) if re.fullmatch(rf"{prefix}\w+ = \d+", line)]
    added = [f"{prefix}{n.decode()} = {len(ids) + k}" for k, n in enumerate(new)]
    assert mine == base[: ids[-1] + 1] + added + base[ids[-1] + 1 :]
