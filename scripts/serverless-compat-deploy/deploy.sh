#!/usr/bin/env bash
set -euo pipefail

# ---------------------------------------------------------------------------
# Deploys the serverless-compat test functions:
#
#   #1 Azure Function (Linux Consumption, Node.js) — zipdeploy
#   #2 GCP Cloud Function gen1 (Node.js)           — gcloud functions deploy
#
# Requires the binary to be built first:
#   ./build.sh
#
# Required env vars:
#   DD_API_KEY           — Datadog API key for the function apps
#
# Optional overrides:
#   DD_SITE              — default: datadoghq.com
#   GCP_PROJECT          — default: datadog-sandbox
#   GCP_REGION           — default: us-central1
#   AZURE_SUBSCRIPTION_ID — default: auto-detected from `az account show`
#   AZURE_RESOURCE_GROUP — default: self-monitoring-nina-dev
#   AZURE_FUNCTION_APP   — default: nina-compat-inventory-node
#   GCP_FUNCTION_NAME    — default: nina-compat-inventory-nodejs
#   SELF_MONITORING_DIR  — default: ~/dd/serverless-compat-self-monitoring
# ---------------------------------------------------------------------------

: "${DD_API_KEY:?DD_API_KEY must be set}"

DD_SITE="${DD_SITE:-datadoghq.com}"
GCP_PROJECT="${GCP_PROJECT:-datadog-serverless-gcp-demo}"
GCP_REGION="${GCP_REGION:-us-central1}"
AZURE_SUBSCRIPTION_ID="${AZURE_SUBSCRIPTION_ID:-$(az account show --query id -o tsv 2>/dev/null)}"
AZURE_RESOURCE_GROUP="${AZURE_RESOURCE_GROUP:-self-monitoring-nina-dev}"
AZURE_FUNCTION_APP="${AZURE_FUNCTION_APP:-nina-compat-inventory-node}"
GCP_FUNCTION_NAME="${GCP_FUNCTION_NAME:-nina-compat-inventory-nodejs}"
SELF_MONITORING_DIR="${SELF_MONITORING_DIR:-${HOME}/dd/serverless-compat-self-monitoring}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
TARGET="x86_64-unknown-linux-musl"
BINARY="${REPO_ROOT}/target/${TARGET}/release/datadog-serverless-compat"

log() { echo "[$(date -u '+%Y-%m-%dT%H:%M:%SZ')] $*"; }

_require_binary() {
  if [[ ! -f "${BINARY}" ]]; then
    echo "ERROR: binary not found at ${BINARY}"
    echo "Run ./build.sh first."
    exit 1
  fi
}

# ---------------------------------------------------------------------------
# #1 — Azure Function (Linux Consumption, Node.js) via zipdeploy
#
# The function app ships the binary inside the npm package at:
#   node_modules/@datadog/serverless-compat/bin/linux-amd64/datadog-serverless-compat
#
# We copy the fresh binary in, zip the whole app directory, and POST to the
# Kudu zipdeploy endpoint. The function restarts automatically.
# ---------------------------------------------------------------------------
deploy_azure_function() {
  log "=== #1 Azure Function: ${AZURE_FUNCTION_APP} ==="

  local app_dir="${SELF_MONITORING_DIR}/azure_functions/compat_node"
  local binary_dest="${app_dir}/node_modules/@datadog/serverless-compat/bin/linux-amd64/datadog-serverless-compat"
  local zip_path="/tmp/compat_node_deploy_$(date +%s).zip"

  if [[ ! -d "${app_dir}" ]]; then
    echo "ERROR: Azure function directory not found: ${app_dir}"
    echo "Set SELF_MONITORING_DIR to the serverless-compat-self-monitoring checkout."
    exit 1
  fi

  log "Copying binary into function package..."
  cp "${BINARY}" "${binary_dest}"
  chmod +x "${binary_dest}"

  log "Zipping function app..."
  (cd "${app_dir}" && zip -r "${zip_path}" . --exclude "*.git*" --quiet)

  log "Getting Azure bearer token..."
  local token
  token=$(az account get-access-token --resource https://management.azure.com --query accessToken -o tsv)

  log "Deploying via zipdeploy..."
  local http_status
  http_status=$(curl -s -o /tmp/zipdeploy_out.txt -w "%{http_code}" \
    -X POST \
    "https://${AZURE_FUNCTION_APP}.scm.azurewebsites.net/api/zipdeploy" \
    -H "Authorization: Bearer ${token}" \
    -H "Content-Type: application/zip" \
    --data-binary "@${zip_path}")

  rm -f "${zip_path}"

  if [[ "${http_status}" == "200" ]]; then
    log "#1 Azure Function deployed (HTTP ${http_status})"
    log "    URL: https://${AZURE_FUNCTION_APP}.azurewebsites.net/api/httptest"
  else
    log "ERROR: zipdeploy returned HTTP ${http_status}"
    cat /tmp/zipdeploy_out.txt
    exit 1
  fi

  export AZURE_FUNCTION_URL="https://${AZURE_FUNCTION_APP}.azurewebsites.net"
}

# ---------------------------------------------------------------------------
# #2 — GCP Cloud Function gen1 (Node.js) via gcloud functions deploy
#
# The function directory contains app.js + node_modules (including the
# serverless-compat package with the binary inside).
#
# We copy the fresh binary into the package, then redeploy.
# ---------------------------------------------------------------------------
deploy_gcp_function() {
  log "=== #2 GCP Cloud Function: ${GCP_FUNCTION_NAME} ==="

  local fn_dir="${SELF_MONITORING_DIR}/gcp_functions/nodejs"
  local binary_dest="${fn_dir}/node_modules/@datadog/serverless-compat/bin/linux-amd64/datadog-serverless-compat"

  # GCP gen1 also ships the binary at the top level (legacy path used by older
  # versions of the npm package init script). Keep both in sync.
  local binary_dest_root="${fn_dir}/datadog-serverless-compat"

  if [[ ! -d "${fn_dir}" ]]; then
    echo "ERROR: GCP function directory not found: ${fn_dir}"
    echo "Set SELF_MONITORING_DIR to the serverless-compat-self-monitoring checkout."
    exit 1
  fi

  log "Copying binary into GCP function package..."
  if [[ -d "$(dirname "${binary_dest}")" ]]; then
    cp "${BINARY}" "${binary_dest}"
    chmod +x "${binary_dest}"
  fi
  # Also update the top-level binary if present (init.js legacy path)
  if [[ -f "${binary_dest_root}" ]]; then
    cp "${BINARY}" "${binary_dest_root}"
    chmod +x "${binary_dest_root}"
  fi

  log "Deploying Cloud Function ${GCP_FUNCTION_NAME}..."
  gcloud functions deploy "${GCP_FUNCTION_NAME}" \
    --project="${GCP_PROJECT}" \
    --region="${GCP_REGION}" \
    --runtime=nodejs20 \
    --trigger-http \
    --allow-unauthenticated \
    --entry-point=httpexample \
    --source="${fn_dir}" \
    --set-env-vars="DD_API_KEY=${DD_API_KEY},DD_SITE=${DD_SITE},DD_ENV=nina,DD_SERVICE=${GCP_FUNCTION_NAME}" \
    --quiet 2>&1 | tail -5

  GCP_FUNCTION_URL=$(gcloud functions describe "${GCP_FUNCTION_NAME}" \
    --project="${GCP_PROJECT}" \
    --region="${GCP_REGION}" \
    --format="value(httpsTrigger.url)" 2>/dev/null)
  log "#2 GCP Cloud Function deployed"
  log "    URL: ${GCP_FUNCTION_URL}"

  export GCP_FUNCTION_URL
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------
main() {
  _require_binary
  log "Binary: ${BINARY} ($(du -sh "${BINARY}" | cut -f1))"

  deploy_azure_function   # #1
  deploy_gcp_function     # #2

  echo ""
  echo "=========================================="
  echo "Deployment complete. Function URLs:"
  printf "  #1 Azure Function (Linux, Node.js): %s/api/httptest\n" "${AZURE_FUNCTION_URL:-N/A}"
  printf "  #2 GCP Cloud Function gen1 (Node.js): %s\n"            "${GCP_FUNCTION_URL:-N/A}"
  echo "=========================================="
  echo ""
  echo "Next: ./trigger.sh   — send inventory payloads and capture UUIDs"
  echo "      ./check-redapl.sh — query DDSQL for serverless-compat rows (allow ~1hr)"
}

# Allow sourcing to call individual functions.
# Usage: source ./deploy.sh && deploy_azure_function
[[ "${BASH_SOURCE[0]:-}" == "${0}" ]] && main "$@"
