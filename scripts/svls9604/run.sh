#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

for arg in "$@"; do
  if [[ "${arg}" == "--plan" ]]; then
    exec python3 "${SCRIPT_DIR}/runner.py" "$@"
  fi
done

if [[ "${SVLS9604_DD_AUTHENTICATED:-}" != "1" ]]; then
  exec dd-auth --site=datad0g.com --org-uuid=2 -- env \
    SVLS9604_DD_AUTHENTICATED=1 \
    python3 "${SCRIPT_DIR}/runner.py" "$@"
fi

exec python3 "${SCRIPT_DIR}/runner.py" "$@"
