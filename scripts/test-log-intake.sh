#!/usr/bin/env bash
set -x
# Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
# SPDX-License-Identifier: Apache-2.0
#
# test-log-intake.sh — Run the log agent example against a local capture server.
#
# The script starts a tiny Python HTTP server that prints every incoming request
# body to stdout so you can inspect the JSON payloads the log agent sends.
#
# USAGE
#   # Local capture (default) — no real Datadog traffic:
#   ./scripts/test-log-intake.sh
#
#   # Send N entries instead of the default 5:
#   LOG_ENTRY_COUNT=50 ./scripts/test-log-intake.sh
#
#   # Flush to a real Datadog endpoint instead of the local server:
#   DD_API_KEY=<your-key> ./scripts/test-log-intake.sh --real
#
# REQUIREMENTS
#   python3   (macOS system python is fine)
#   cargo

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
PORT="${LOG_CAPTURE_PORT:-9999}"
REAL_MODE=false

for arg in "$@"; do
  [[ "$arg" == "--real" ]] && REAL_MODE=true
done

# ── Build the example first ──────────────────────────────────────────────────
echo "Building send_logs example..."
cargo build -p datadog-logs-agent --example send_logs --quiet 2>&1

# ── Real Datadog mode ─────────────────────────────────────────────────────────
if [[ "$REAL_MODE" == true ]]; then
  if [[ -z "${DD_API_KEY:-}" ]]; then
    echo "Error: DD_API_KEY must be set for --real mode" >&2
    exit 1
  fi
  echo ""
  echo "Flushing to real Datadog endpoint..."
  LOG_ENTRY_COUNT="${LOG_ENTRY_COUNT:-5}" \
    cargo run -p datadog-logs-agent --example send_logs --quiet 2>&1
  exit $?
fi

# ── Local capture server mode ─────────────────────────────────────────────────

# Python HTTP server that prints the request body as formatted JSON
CAPTURE_SERVER_SCRIPT=$(cat <<'PYEOF'
import http.server
import json
import sys

class Handler(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(length)
        encoding = self.headers.get("Content-Encoding", "none")
        content_type = self.headers.get("Content-Type", "")

        print(f"\n{'─'*60}")
        print(f"POST {self.path}")
        print(f"DD-API-KEY : {self.headers.get('DD-API-KEY', '(not set)')}")
        print(f"DD-PROTOCOL: {self.headers.get('DD-PROTOCOL', '(not set)')}")
        print(f"Content-Encoding: {encoding}")
        print(f"Content-Type    : {content_type}")

        if encoding == "zstd":
            try:
                import zstd
                body = zstd.decompress(body)
                print("(decompressed zstd payload)")
            except ImportError:
                print("(zstd payload — install python-zstd to decompress: pip install zstd)")

        if "json" in content_type or body.startswith(b"[") or body.startswith(b"{"):
            try:
                parsed = json.loads(body)
                print(f"\nPayload ({len(parsed) if isinstance(parsed, list) else 1} entries):")
                print(json.dumps(parsed, indent=2))
            except json.JSONDecodeError:
                print(f"\nRaw body ({len(body)} bytes): {body[:500]}")
        else:
            print(f"\nRaw body ({len(body)} bytes)")

        self.send_response(200)
        self.end_headers()
        sys.stdout.flush()

    def log_message(self, fmt, *args):
        pass  # suppress default access log noise

port = int(sys.argv[1])
print(f"Capture server listening on http://localhost:{port}")
print("Waiting for log flush... (Ctrl-C to stop)\n")
sys.stdout.flush()

httpd = http.server.HTTPServer(("localhost", port), Handler)
httpd.serve_forever()
PYEOF
)

# Start capture server in background
python3 -c "$CAPTURE_SERVER_SCRIPT" "$PORT" &
SERVER_PID=$!

cleanup() {
  kill "$SERVER_PID" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

# Give the server a moment to start
sleep 0.3

echo ""
echo "Running send_logs example → http://localhost:${PORT}/logs"
echo ""

DD_OBSERVABILITY_PIPELINES_WORKER_LOGS_ENABLED=true \
DD_OBSERVABILITY_PIPELINES_WORKER_LOGS_URL="http://localhost:${PORT}/logs" \
DD_API_KEY="${DD_API_KEY:-local-test-key}" \
LOG_ENTRY_COUNT="${LOG_ENTRY_COUNT:-5}" \
  cargo run -p datadog-logs-agent --example send_logs --quiet 2>&1

echo ""
echo "Done. Press Ctrl-C to stop the capture server."
wait "$SERVER_PID"
