#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
cd "$ROOT_DIR"

# The CLI loads .env itself. Supported variables:
#   ASH_PROTOCOL, ASH_MODEL, ASH_BASE_URL, ASH_API_KEY
exec cargo run --quiet -p ash-cli -- "$@"
