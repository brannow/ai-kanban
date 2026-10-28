#!/bin/sh
# Packs the plugin as users install it: plugin/ as it is, plus one binary per build in libexec/.
#
#   scripts/package-plugin.sh <out.zip> <dir holding ai-kanban-<build> binaries> [version]
#
# With a version (the release tag, without its v), the zip's manifest carries it, so users see
# "0.1.0" rather than a hash. plugin/ itself has none: a version in git would pin every build,
# including test builds, to whatever it last said.
#
# The release workflow runs this; so can anyone checking a zip before it is published, with
# `claude --plugin-dir <out.zip>`. One script for both, so the zip tested is the zip shipped.
set -eu
out=$1
bins=$2
version=${3:-}
root=$(cd "$(dirname "$0")/.." && pwd)
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT

mkdir -p "$stage/ai-kanban/libexec"
cp -R "$root/plugin/.claude-plugin" "$root/plugin/hooks" "$root/plugin/bin" "$root/plugin/.mcp.json" "$root/plugin/README.md" "$stage/ai-kanban/"
if [ -n "$version" ]; then
    awk -v v="$version" '{ print } /^  "name": "ai-kanban",$/ { print "  \"version\": \"" v "\"," }' \
        "$root/plugin/.claude-plugin/plugin.json" > "$stage/ai-kanban/.claude-plugin/plugin.json"
    grep -q "\"version\": \"$version\"" "$stage/ai-kanban/.claude-plugin/plugin.json" \
        || { echo "package-plugin: could not write the version into plugin.json" >&2; exit 1; }
fi
found=0
for b in "$bins"/ai-kanban-*; do
    [ -f "$b" ] || continue
    install -m 755 "$b" "$stage/ai-kanban/libexec/$(basename "$b")"
    found=$((found + 1))
done
[ "$found" -gt 0 ] || { echo "package-plugin: no ai-kanban-<build> binaries in $bins" >&2; exit 1; }

# The plugin one directory down, which the archive source accepts. zip keeps the unix modes,
# so the wrapper and the binaries arrive executable.
# The output's directory may not exist yet (CI's dist/). Created here, and resolved outside a
# command substitution, where a failed cd would not stop the script under `set -e`.
mkdir -p "$(dirname "$out")"
rm -f "$out"
out_dir=$(cd "$(dirname "$out")" && pwd) || exit 1
out_abs=$out_dir/$(basename "$out")
(cd "$stage" && zip -qr -X "$out_abs" ai-kanban)
echo "$out_abs"
