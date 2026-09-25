"""Output must not depend on hash seed, locale or UTF-8 mode."""

from __future__ import annotations

import os
from pathlib import Path

import pytest
from golden_compare import GOLDEN_EXPORT, assert_same_tree, read_tree
from support import read_output, run_cli


@pytest.mark.slow
def test_hash_seed_and_locale_do_not_change_output(tmp_path: Path) -> None:
    variants = {
        "seed1-C-noutf8": {"PYTHONHASHSEED": "1", "LC_ALL": "C", "PYTHONUTF8": "0"},
        "seed2-utf8": {"PYTHONHASHSEED": "2", "LC_ALL": "C", "PYTHONUTF8": "1"},
    }
    outputs = {}
    for name, overrides in variants.items():
        out = tmp_path / name
        proc = run_cli("-q", "-o", out, cwd=tmp_path, env=dict(os.environ, **overrides))
        assert proc.returncode == 0, f"{name}: {proc.stderr}"
        outputs[name] = read_output(out)
    first, second = outputs.values()
    assert_same_tree(first, second)
    assert_same_tree(read_tree(GOLDEN_EXPORT), first)
