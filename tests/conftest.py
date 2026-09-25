from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path

import pytest
from golden_compare import GAME, SOURCE, check_manifest
from support import hash_tree, read_output

from modsys.build import BuildResult, build


@dataclass
class VanillaBuild:
    result: BuildResult
    output_dir: Path
    files: dict[str, bytes]
    game_before: dict[str, str]
    game_after: dict[str, str]


@pytest.fixture(scope="session")
def golden_manifest_ok() -> None:
    """Manifest self-check; every golden comparison depends on it."""
    problems = check_manifest()
    assert not problems, "golden tree does not match MANIFEST.json:\n" + "\n".join(problems)


@pytest.fixture(scope="session")
def vanilla(tmp_path_factory: pytest.TempPathFactory) -> VanillaBuild:
    """One clean build of game/module_system shared by the whole session."""
    out = tmp_path_factory.mktemp("vanilla") / "Native"
    before = hash_tree(GAME)
    result = build(SOURCE, out, quiet=True)
    after = hash_tree(GAME)
    return VanillaBuild(result, out, read_output(out), before, after)
