"""End-to-end behaviour of the build driver on the real Module System."""

from __future__ import annotations

import json
import os
import re
from pathlib import Path

import pytest
from golden_compare import GAME, GOLDEN_EXPORT, MANIFEST, SOURCE, assert_same_tree, read_tree
from support import copy_source, edit_once, hash_tree, read_output, run_cli

from modsys.build import (
    BAT_NAME,
    MARKER_NAME,
    GuardError,
    build,
    parse_stages,
)


def test_fresh_build_accepted_in_one_pass(vanilla) -> None:
    assert vanilla.result.passes == 1
    assert vanilla.result.warnings == []
    assert vanilla.result.ids_differ_from_source == []
    assert sorted(vanilla.result.new) == sorted(vanilla.files)


def test_marker_records_provenance(vanilla) -> None:
    marker = json.loads((vanilla.output_dir / MARKER_NAME).read_text())
    assert marker["passes"] == 1
    assert marker["stages"] == parse_stages(SOURCE / BAT_NAME)
    assert set(marker["files"]) == set(vanilla.files)
    assert marker["files"] == vanilla.result.files


def test_game_tree_untouched_by_build(vanilla) -> None:
    assert vanilla.game_after == vanilla.game_before


@pytest.mark.slow
def test_build_from_unrelated_cwd(vanilla, tmp_path: Path) -> None:
    cwd = tmp_path / "elsewhere"
    cwd.mkdir()
    proc = run_cli("-q", "-o", tmp_path / "out", cwd=cwd)
    assert proc.returncode == 0, proc.stderr
    assert_same_tree(vanilla.files, read_output(tmp_path / "out"))
    assert sorted(p.name for p in cwd.iterdir()) == []


@pytest.mark.slow
def test_output_path_with_space_and_ampersand(vanilla, tmp_path: Path) -> None:
    out = tmp_path / "Mount & Blade" / "my mod"
    result = build(SOURCE, out, quiet=True)
    assert result.passes == 1
    # a second, different output dir gives identical bytes
    assert out != vanilla.output_dir
    assert_same_tree(vanilla.files, read_output(out))


@pytest.mark.slow
def test_safepath_and_decoy_pythonpath_are_ignored(tmp_path: Path) -> None:
    decoy = tmp_path / "decoy"
    decoy.mkdir()
    (decoy / "module_info.py").write_text(
        'export_dir = "/decoy/"\nraise RuntimeError("decoy module_info imported")\n'
    )
    env = dict(os.environ, PYTHONSAFEPATH="1", PYTHONPATH=str(decoy))
    proc = run_cli("-q", "-o", tmp_path / "out", cwd=tmp_path, env=env)
    assert proc.returncode == 0, proc.stderr
    assert_same_tree(read_tree(GOLDEN_EXPORT), read_output(tmp_path / "out"))


def test_stage_list_matches_bat() -> None:
    bat = (SOURCE / BAT_NAME).read_bytes().decode("latin-1")
    bat_lines = re.findall(r"^python (process_\w+\.py)\r?$", bat, re.MULTILINE)
    stages = parse_stages(SOURCE / BAT_NAME)
    assert stages == bat_lines
    assert len(stages) == 29
    assert stages == json.loads(MANIFEST.read_text())["stages"]
    assert "process_line_correction.py" not in stages
    assert "process_tags_unused.py" not in stages


# --- output guards ------------------------------------------------------------------------


@pytest.mark.parametrize("parts", [("Modules", "Native"), ("modules", "NATIVE")])
def test_native_output_refused(tmp_path: Path, parts: tuple[str, str]) -> None:
    out = tmp_path.joinpath("Mount&Blade Warband", *parts)
    with pytest.raises(GuardError, match="--allow-native"):
        build(SOURCE, out, quiet=True)
    assert not out.exists()


def test_native_output_refused_cli_exit_2(tmp_path: Path) -> None:
    out = tmp_path / "Modules" / "Native"
    proc = run_cli("-o", out, cwd=tmp_path)
    assert proc.returncode == 2
    assert "--allow-native" in proc.stderr
    assert not out.exists()


def test_foreign_nonempty_dir_refused(tmp_path: Path) -> None:
    out = tmp_path / "foreign"
    out.mkdir()
    (out / "notes.txt").write_text("mine")
    with pytest.raises(GuardError, match="--force"):
        build(SOURCE, out, quiet=True)
    assert sorted(p.name for p in out.iterdir()) == ["notes.txt"]


def test_output_inside_game_refused(tmp_path: Path) -> None:
    before = hash_tree(GAME)
    for out in (GAME / "build_out", SOURCE / "export", GAME):
        with pytest.raises(GuardError, match="inside the game sources"):
            build(SOURCE, out, quiet=True)
        assert out == GAME or not out.exists()
    assert hash_tree(GAME) == before


def test_bad_arguments_exit_2(tmp_path: Path) -> None:
    proc = run_cli("--no-such-option", cwd=tmp_path)
    assert proc.returncode == 2


@pytest.mark.slow
def test_failed_build_leaves_output_untouched(vanilla, tmp_path: Path) -> None:
    src = copy_source(tmp_path / "src")
    edit_once(
        src / "module_strings.py",
        "strings = [",
        "strings = [undefined_name_for_test] + [",
    )
    out = tmp_path / "out"
    out.mkdir()
    for name, data in vanilla.files.items():
        (out / name).write_bytes(data)
    (out / MARKER_NAME).write_bytes((vanilla.output_dir / MARKER_NAME).read_bytes())
    before = {p.name: (p.read_bytes(), p.stat().st_mtime_ns) for p in out.iterdir()}

    proc = run_cli("--source-dir", src, "-o", out, cwd=tmp_path)
    assert proc.returncode == 1
    assert "NameError" in proc.stderr and "undefined_name_for_test" in proc.stderr
    assert "after 1 pass(es)" in proc.stderr
    assert {p.name: (p.read_bytes(), p.stat().st_mtime_ns) for p in out.iterdir()} == before
