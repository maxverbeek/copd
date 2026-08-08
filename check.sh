#!/usr/bin/env bash
# Pre-refresh checks. The shaders live inside JS template literals, so a stray
# backtick in GLSL silently truncates the string and the page dies with a
# JavaScript SyntaxError — validate both languages, not just Rust.
set -euo pipefail
cd "$(dirname "$0")"

python3 - <<'PY'
import re, pathlib
s = pathlib.Path("index.html").read_text()
out = pathlib.Path(".check"); out.mkdir(exist_ok=True)
(out / "mod.mjs").write_text(re.search(r'<script type="module">(.*?)</script>', s, re.S).group(1))
for name, ext in (("VERT", "vert"), ("FRAG", "frag")):
    (out / f"s.{ext}").write_text(re.search(r"const %s = `(.*?)`;" % name, s, re.S).group(1))
PY

nix develop --command cargo test --quiet
nix shell nixpkgs#nodejs --command node --check .check/mod.mjs
nix shell nixpkgs#glslang --command glslangValidator .check/s.vert .check/s.frag >/dev/null
nix develop --command wasm-pack build --target web --release 2>&1 | grep -E "^\[INFO\]: ✨"
echo "ok — rust, js, glsl, wasm"
