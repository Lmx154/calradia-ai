"""warband-build-data regenerates the Module_data files byte-identically."""

from __future__ import annotations

from pathlib import Path

import pytest
from golden_compare import GOLDEN_DATA, MODULE_DATA_SOURCE, assert_same_tree, read_tree
from support import hash_tree, run_cli

from modsys.build import GuardError
from modsys.module_data import build_data


def test_module_data_matches_golden(golden_manifest_ok, tmp_path: Path) -> None:
    before = hash_tree(MODULE_DATA_SOURCE)
    out = tmp_path / "data out"
    proc = run_cli("-o", out, cwd=tmp_path, module="modsys.module_data")
    assert proc.returncode == 0, proc.stderr
    assert_same_tree(read_tree(GOLDEN_DATA), read_tree(out))
    assert hash_tree(MODULE_DATA_SOURCE) == before


def test_module_data_refuses_game_sources() -> None:
    target = MODULE_DATA_SOURCE.parent / "data_out"
    with pytest.raises(GuardError):
        build_data(output_dir=target, quiet=True)
    assert not target.exists()


def test_module_data_refuses_game_install(tmp_path: Path) -> None:
    target = tmp_path / "Mount&Blade Warband" / "Modules" / "Native" / "Data"
    with pytest.raises(GuardError):
        build_data(output_dir=target, quiet=True)
    assert not target.exists()
