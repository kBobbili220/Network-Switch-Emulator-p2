#!/usr/bin/env bash
# Build the viewer wasm + JS bindings into web/pkg/.
#
# Requires `wasm-pack` (https://rustwasm.github.io/wasm-pack/installer/).
set -euo pipefail
cd "$(dirname "$0")"
wasm-pack build --release --target web --out-dir web/pkg
echo "Built. Serve web/ over HTTP, e.g.:"
echo "  python3 -m http.server -d web 8080"
echo "  open http://localhost:8080"
