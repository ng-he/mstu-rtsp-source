#!/usr/bin/env bash
# Runs this plugin's tests.
#
# The center repository calls this rather than knowing how the plugin is
# built, so a plugin written in another language tests itself however suits
# it. The only contract is the exit status: zero passed, anything else failed.
set -euo pipefail

cd "$(dirname "$0")"
# Unformatted code fails too, the same as a failing test would.
cargo fmt -- --check
cargo test "$@"
