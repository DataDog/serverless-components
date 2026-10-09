#!/usr/bin/env bash
set -euo pipefail

# ---------------------------------------------------------------------------
# Triggers both serverless-compat test functions to fire inventory payloads
# to the Datadog intake. Prints the UUID returned by the Azure diagnostic
# endpoint so you can correlate with DDSQL rows later.
#
# Usage: ./trigger.sh
# Optional overrides:
#   AZURE_FUNCTION_APP  — default: nina-compat-inventory-node
#   GCP_FUNCTION_NAME   — default: nina-compat-inventory-nodejs
#   GCP_PROJECT         — default: datadog-sandbox
#   GCP_REGION          — default: us-central1
# ---------------------------------------------------------------------------

AZURE_FUNCTION_APP="${AZURE_FUNCTION_APP:-nina-compat-inventory-node}"
GCP_FUNCTION_NAME="${GCP_FUNCTION_NAME:-nina-compat-inventory-nodejs}"
GCP_PROJECT="${GCP_PROJECT:-datadog-serverless-gcp-demo}"
GCP_REGION="${GCP_REGION:-us-central1}"

log() { echo "[$(date -u '+%Y-%m-%dT%H:%M:%SZ')] $*"; }

# ---------------------------------------------------------------------------
# #1 — Azure Function
# Hits /api/httptest to trigger the Rust binary's startup inventory payload.
# Also hits /api/testinventory (the Node.js diagnostic endpoint) to get the
# UUID that was sent — useful for correlating with DDSQL rows.
# ---------------------------------------------------------------------------
trigger_azure() {
  log "=== #1 Azure Function: ${AZURE_FUNCTION_APP} ==="

  local base_url="https://${AZURE_FUNCTION_APP}.azurewebsites.net"

  log "Triggering httptest (starts compat binary, fires inventory)..."
  local resp
  resp=$(curl -sf "${base_url}/api/httptest" 2>/dev/null || echo "(no response)")
  log "  httptest response: ${resp}"

  # NOTE: testinventory sends its own crypto.randomUUID() — NOT the Rust binary's UUID.
  # It's useful for confirming the network path works (status 202) but the UUID it
  # returns won't appear in DDSQL as a serverless-compat row.
  log "Triggering testinventory (confirms network path to intake)..."
  local inv_resp
  inv_resp=$(curl -sf "${base_url}/api/testinventory" 2>/dev/null || echo "(no response)")
  log "  testinventory (JS diagnostic): ${inv_resp}"

  local status
  status=$(echo "${inv_resp}" | python3 -c "import json,sys; d=json.load(sys.stdin); print(d.get('status',''))" 2>/dev/null || true)
  log "  Intake HTTP status: ${status} (202 = accepted by EPRW)"
  log ""
  log "  NOTE: The Rust binary's UUID is logged on stdout by the compat binary."
  log "  Look for: 'Inventory payload sent (uuid=<uuid>, ...)' in the function logs."
  log "  Use Query 3 in check-redapl.sh (24h window, no UUID filter) to find the row."
}

# ---------------------------------------------------------------------------
# #2 — GCP Cloud Function gen1
# Requires a GCP identity token (gcloud auth login must be done first).
# ---------------------------------------------------------------------------
trigger_gcp() {
  log "=== #2 GCP Cloud Function: ${GCP_FUNCTION_NAME} ==="

  local fn_url
  fn_url=$(gcloud functions describe "${GCP_FUNCTION_NAME}" \
    --project="${GCP_PROJECT}" \
    --region="${GCP_REGION}" \
    --format="value(httpsTrigger.url)" 2>/dev/null)

  # Fall back to the standard URL pattern if describe doesn't return a URL
  if [[ -z "${fn_url}" ]]; then
    fn_url="https://${GCP_REGION}-${GCP_PROJECT}.cloudfunctions.net/${GCP_FUNCTION_NAME}"
    log "  Using derived URL: ${fn_url}"
  fi

  log "Getting GCP identity token..."
  local token
  token=$(gcloud auth print-identity-token 2>/dev/null || true)

  if [[ -z "${token}" ]]; then
    log "  WARNING: no GCP identity token — run 'gcloud auth login' first"
    log "  Falling back to unauthenticated request..."
    local resp
    resp=$(curl -sf "${fn_url}" 2>/dev/null || echo "(no response)")
  else
    local resp
    resp=$(curl -sf -H "Authorization: Bearer ${token}" "${fn_url}" 2>/dev/null || echo "(no response)")
  fi

  log "  GCP function response: ${resp}"
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------
rm -f /tmp/compat_trigger_uuids.txt

log "Triggering serverless-compat test functions..."
trigger_azure   # #1
trigger_gcp     # #2

echo ""
echo "=========================================="
echo "Trigger complete."
if [[ -f /tmp/compat_trigger_uuids.txt ]]; then
  echo "Captured UUIDs:"
  cat /tmp/compat_trigger_uuids.txt | sed 's/^/  /'
fi
echo ""
echo "These UUIDs were sent to the intake. DDSQL rows can take ~1hr to appear."
echo "Run ./check-redapl.sh to generate the DDSQL query."
echo "=========================================="
