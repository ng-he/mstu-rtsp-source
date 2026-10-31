#!/usr/bin/env bash
# Builds this plugin. The library lands in target/release, where the center
# repo's collect.sh looks for it; a plugin in another language puts its own
# library and ui folder in dist/ instead.
set -euo pipefail

cd "$(dirname "$0")"
cargo build --release
