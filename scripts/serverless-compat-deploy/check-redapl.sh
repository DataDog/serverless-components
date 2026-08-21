#!/usr/bin/env bash
set -euo pipefail

# ---------------------------------------------------------------------------
# Prints DDSQL queries to find serverless-compat rows in BOTH the
# datadog_agent and serverless_compat_agent REDAPL tables.
#
# Usage: ./check-redapl.sh
# ---------------------------------------------------------------------------

# api_key_uuid: MD5-UUID of your DD_API_KEY. Find yours with:
#   python3 -c "import hashlib,uuid; print(str(uuid.UUID(bytes=hashlib.md5('<your-key>'.encode()).digest())))"
DEFAULT_API_KEY_UUID="<your-api-key-uuid>"
API_KEY_UUID="${1:-${DEFAULT_API_KEY_UUID}}"

TODAY=$(date -u +%Y-%m-%dT00:00:00Z)

echo "================================================================"
echo "SVLS-9604 — serverless-compat REDAPL queries"
echo "Date: $(date -u '+%Y-%m-%d %H:%M:%S UTC')"
echo "api_key_uuid: ${API_KEY_UUID}"
echo "================================================================"
echo ""
echo "Run these in DDSQL (redapl/staging for staging, prod otherwise)."
echo ""

# ---------------------------------------------------------------------------
# Query 1: serverless_compat_agent table — the primary target
# ---------------------------------------------------------------------------
cat <<EOF
-- Query 1: serverless_compat_agent rows (primary target)
-- Expects: 2 rows — one azure_function, one gcp_cloud_function_gen1
SELECT _key, resource_name, workload_type, serverless_compat_version,
       region, _display_name, first_seen_at
FROM udm.all.serverless_compat_agent
WHERE api_key_uuid = '${API_KEY_UUID}'
ORDER BY first_seen_at DESC
LIMIT 20;

EOF

# ---------------------------------------------------------------------------
# Query 2: serverless_compat_agent — field completeness check
# ---------------------------------------------------------------------------
cat <<EOF
-- Query 2: field completeness for serverless_compat_agent
SELECT resource_id, resource_name, workload_type,
       serverless_compat_version, serverless_compat_runtime_version,
       region, gcp_project_id, azure_subscription_id, azure_resource_group,
       dd_env, dd_service, first_seen_at
FROM udm.all.serverless_compat_agent
WHERE api_key_uuid = '${API_KEY_UUID}'
ORDER BY first_seen_at DESC
LIMIT 20;

EOF

# ---------------------------------------------------------------------------
# Query 3: datadog_agent table — the secondary write (ECS Fargate UUID path)
# Still useful to confirm the payload reaches EPRW at all.
# ---------------------------------------------------------------------------
cat <<EOF
-- Query 3: datadog_agent rows for serverless-compat (secondary write)
SELECT _key, hostname, agent_version, install_method_tool, _first_seen_at
FROM udm.all.datadog_agent
WHERE api_key_uuid = '${API_KEY_UUID}'
  AND agent_version = '7.83.0'
ORDER BY _first_seen_at DESC
LIMIT 20;

EOF

# ---------------------------------------------------------------------------
# Query 4: serverless_init_agent — for comparison with the init agent
# ---------------------------------------------------------------------------
cat <<EOF
-- Query 4: serverless_init_agent rows (for comparison)
SELECT _key, resource_name, workload_type, agent_version_base,
       serverless_init_version, region, first_seen_at
FROM udm.all.serverless_init_agent
WHERE api_key_uuid = '${API_KEY_UUID}'
ORDER BY first_seen_at DESC
LIMIT 10;

EOF

echo "================================================================"
echo "Expected results:"
echo "  Query 1: 2 rows (azure_function + gcp_cloud_function_gen1)"
echo "  Query 2: same rows with all fields populated"
echo "  Query 3: N rows (one per cold start, UUID-keyed)"
echo "  Query 4: serverless-init rows for cross-reference"
echo ""
echo "Notes:"
echo "  - serverless_compat_agent rows key on resource_id (not UUID)."
echo "    Each distinct function has one permanent row that updates in-place."
echo "  - Rows can take ~1hr to appear after EPRW accepts the payload (HTTP 202)."
echo "  - If Query 1 returns 0 rows, check:"
echo "      1. Log: 'Inventory payload sent (uuid=..., resource_id=...)' in the binary output"
echo "      2. EPRW metric: event_platform_resource_writer.agentmetadata.serverless_write.accepted{resource_type:serverless_compat_agent}"
echo "      3. resource_id must not be empty (check WEBSITE_OWNER_NAME / GCP env vars)"
echo "================================================================"
