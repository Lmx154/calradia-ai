#!/usr/bin/env bash
# Reproduce the Python 2.7 reference build of the ORIGINAL (unmodified) Module
# System 1.171 in a throwaway Docker container.
#
# This is an optional provenance tool. Building and testing the project never
# needs Docker or Python 2. The input is always the pristine upstream tree from
# git commit 1c213ba, never the ported sources in game/.
#
# Usage: tools/make_reference.sh OUT_DIR [--replace]
#   OUT_DIR must not already exist. With --replace, the result is also copied
#   over tests/golden/ (only do this deliberately; never to "fix" a failing test).
set -euo pipefail

IMAGE="python:2.7.18-slim@sha256:6c1ffdff499e29ea663e6e67c9b6b9a3b401d554d2c9f061f9a45344e3992363"
UPSTREAM_COMMIT="1c213ba"
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

out="${1:?usage: $0 OUT_DIR [--replace]}"
replace="${2:-}"
[ -e "$out" ] && { echo "refusing: $out already exists" >&2; exit 2; }
mkdir -p "$out"
out="$(cd "$out" && pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

git -C "$REPO" archive "$UPSTREAM_COMMIT" mb_warband_module_system_1171 | tar -x -C "$work"
ms="$work/ms"; md="$work/data/Module_data"
mv "$work/mb_warband_module_system_1171/Module_system 1.171" "$ms"
mkdir -p "$work/data" "$work/data/Module_system"   # sink for Ground_specs.py's ../Module_system/ write
mv "$work/mb_warband_module_system_1171/Module_data 1.171" "$md"
mkdir -p "$work/export"

# The only edit to the pristine tree: point export_dir at the container's sink.
sed -i 's#^export_dir = .*#export_dir = "/export/"#' "$ms/module_info.py"
stages="$(grep -o 'python [a-z_]*\.py' "$ms/build_module.bat" | awk '{print $2}' | tr '\n' ' ')"

docker run --rm -u "$(id -u):$(id -g)" \
  -v "$ms:/src" -v "$work/export:/export" -v "$work/data:/data" \
  "$IMAGE" sh -ec "
    python --version
    cd /src
    for f in $stages; do echo \"=== \$f\"; python -B \$f; done
    cd /data/Module_data
    for f in Flora_kinds.py Ground_specs.py Skyboxes.py; do echo \"=== \$f\"; python -B \$f; done
  " > "$out/reference_build.log" 2>&1

mkdir -p "$out/module_system_1171/export" "$out/module_system_1171/ids" "$out/module_data_1171"
cp "$work/export/"* "$out/module_system_1171/export/"
cp "$ms"/ID_*.py "$out/module_system_1171/ids/"
cp "$md"/{flora_kinds.txt,ground_specs.txt,skyboxes.txt,ground_spec_codes.h} "$out/module_data_1171/"
cp "$work/data/Module_system/header_ground_types.py" "$out/module_data_1171/"
echo "$stages" > "$out/stages.txt"
python3 "$REPO/tools/write_manifest.py" "$out" "$IMAGE" "$UPSTREAM_COMMIT"
echo "reference written to $out"

if [ "$replace" = "--replace" ]; then
  rm -rf "$REPO/tests/golden"
  cp -r "$out" "$REPO/tests/golden"
  echo "tests/golden replaced"
fi
