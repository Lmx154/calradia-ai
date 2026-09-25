"""Write MANIFEST.json (sha256 + size per file, plus provenance) for a reference tree."""

import hashlib
import json
import sys
from pathlib import Path

root, image, commit = Path(sys.argv[1]), sys.argv[2], sys.argv[3]
files = {
    p.relative_to(root).as_posix(): {
        "sha256": hashlib.sha256(p.read_bytes()).hexdigest(),
        "size": p.stat().st_size,
    }
    for p in sorted(root.rglob("*"))
    if p.is_file() and p.name not in ("MANIFEST.json", "reference_build.log", "stages.txt")
}
manifest = {
    "description": "Python 2.7.18 Linux reference build of the unmodified TaleWorlds "
    "Mount&Blade Warband Module System 1.171",
    "upstream_commit": commit,
    "upstream_paths": [
        "mb_warband_module_system_1171/Module_system 1.171",
        "mb_warband_module_system_1171/Module_data 1.171",
    ],
    "upstream_zip": "mb_warband_module_system_1171.zip",
    "upstream_zip_sha256": "b8d5095031ed2b9df0a4020097bea06b6137022a52cd24a9d5c6b575444b16e1",
    "docker_image": image,
    "python": "2.7.18 (default, Apr 20 2020, 19:34:11) [GCC 8.3.0]",
    "source_edit": 'module_info.py: export_dir = "/export/" (inside the container only)',
    "stages": (root / "stages.txt").read_text().split(),
    "module_data_scripts": ["Flora_kinds.py", "Ground_specs.py", "Skyboxes.py"],
    "files": files,
}
(root / "MANIFEST.json").write_text(json.dumps(manifest, indent=2) + "\n")
