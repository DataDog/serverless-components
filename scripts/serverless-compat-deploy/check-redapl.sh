#!/usr/bin/env bash
set -euo pipefail

# ---------------------------------------------------------------------------
# check-redapl.sh — DDSQL validation for serverless_init_agent and
#                   serverless_compat_agent
#
# Prints queries to paste in: go/redapl → Queries → SQL (staging environment)
#
# Schema facts (from dd-source UDM specs):
#   - Both tables key on resource_id via SanitizeString → stored as _key
#   - _display_name resolves from resource_name
#   - _first_seen_at comes from the generic_resource base (not first_seen_at)
#   - last_seen_at is a schema property but may not be populated by the
#     decoder yet — check _modified_at (generic_resource base) as an
#     alternative for "when was this row last updated"
#   - Neither table has an api_key_uuid column — do not filter by it
#   - Neither table has a uuid column — do not query it
#   - udm.all.resource_changes is not an exposed DDSQL table
#
# Service name env vars (override for your own test services):
#   INIT_SERVICE_FILTER  — partial resource_id match for init services
#   COMPAT_SERVICE_FILTER — partial resource_id match for compat services
#
# Usage: ./check-redapl.sh
# ---------------------------------------------------------------------------

# Partial strings used in WHERE resource_id LIKE '% ... %' filters.
# Set these to a substring that matches your deployed service names.
INIT_SERVICE_FILTER="${INIT_SERVICE_FILTER:-}"     # e.g. "my-project"
COMPAT_SERVICE_FILTER="${COMPAT_SERVICE_FILTER:-}" # e.g. "nina-compat"

NOW=$(date -u '+%Y-%m-%dT%H:%M:%SZ')
TODAY=$(date -u +%Y-%m-%dT00:00:00Z)

echo "================================================================"
echo "SVLS-9604 — REDAPL DDSQL validation (both tables)"
echo "Date        : $(date -u '+%Y-%m-%d %H:%M:%S UTC')"
echo "Environment : ${REDAPL_ENV:-staging  (set REDAPL_ENV=prod for production)}"
echo "================================================================"
echo ""
echo "Run these in: go/redapl → Queries → SQL"
echo ""

# Helper: emit a filter clause only when a filter is set
init_filter() {
  if [[ -n "${INIT_SERVICE_FILTER}" ]]; then
    echo "WHERE resource_id LIKE '%${INIT_SERVICE_FILTER}%'"
  fi
}
compat_filter() {
  if [[ -n "${COMPAT_SERVICE_FILTER}" ]]; then
    echo "WHERE resource_id LIKE '%${COMPAT_SERVICE_FILTER}%'"
  fi
}

# ---------------------------------------------------------------------------
# Block 1: serverless_init_agent — current rows
# Expected: one row per deployed resource (SI-01 through SI-09 from the RFC)
# ---------------------------------------------------------------------------
cat <<EOF
-- 1a. serverless_init_agent: current rows
--     _key resolves from resource_id via SanitizeString
--     _first_seen_at and _modified_at come from the generic_resource base
SELECT _key, resource_id, resource_name, workload_type, deployment_model,
       agent_version_base, serverless_init_version, runtime, region,
       gcp_project_id, azure_subscription_id,
       dd_env, dd_service, dd_version,
       _first_seen_at, _modified_at
FROM udm.all.serverless_init_agent
$(init_filter)
ORDER BY workload_type, deployment_model, _first_seen_at DESC
LIMIT 30;

-- 1b. serverless_init_agent: cardinality check
--     PASS: total_rows == distinct_resources for every (workload_type, deployment_model)
--     FAIL: total_rows > distinct_resources means per-instance fields are in the key (R1/R3 risk)
SELECT workload_type, deployment_model,
       COUNT(*) AS total_rows,
       COUNT(DISTINCT resource_id) AS distinct_resources
FROM udm.all.serverless_init_agent
$(init_filter)
GROUP BY workload_type, deployment_model
ORDER BY workload_type, deployment_model;

-- 1c. serverless_init_agent: _key correctness
--     _key must equal SanitizeString(resource_id). It must not contain uuid or
--     a trailing separator that would indicate an old (resource_id|uuid) schema.
SELECT _key, resource_id, workload_type
FROM udm.all.serverless_init_agent
$(init_filter)
LIMIT 20;

EOF

# ---------------------------------------------------------------------------
# Block 2: serverless_compat_agent — current rows
# Expected: exactly 2 rows (SC-01 azure_function + SC-02 gcp_cloud_function_gen1)
# ---------------------------------------------------------------------------
cat <<EOF
-- 2a. serverless_compat_agent: current rows
SELECT _key, resource_id, resource_name, workload_type,
       serverless_compat_version, serverless_compat_runtime_version, runtime,
       region, gcp_project_id, azure_subscription_id, azure_resource_group,
       dd_env, dd_service, dd_version,
       _first_seen_at, _modified_at
FROM udm.all.serverless_compat_agent
$(compat_filter)
ORDER BY workload_type, _first_seen_at DESC
LIMIT 20;

-- 2b. serverless_compat_agent: cardinality check
--     PASS: exactly 1 row per function regardless of burst size
SELECT workload_type,
       COUNT(*) AS total_rows,
       COUNT(DISTINCT resource_id) AS distinct_resources
FROM udm.all.serverless_compat_agent
$(compat_filter)
GROUP BY workload_type
ORDER BY workload_type;

EOF

# ---------------------------------------------------------------------------
# Block 3: Cross-table cardinality summary
# ---------------------------------------------------------------------------
cat <<EOF
-- 3. Combined cardinality across both tables
SELECT 'serverless_init_agent'  AS flavor, workload_type,
       COUNT(*) AS total_rows,
       COUNT(DISTINCT resource_id) AS distinct_resources
FROM udm.all.serverless_init_agent
GROUP BY workload_type
UNION ALL
SELECT 'serverless_compat_agent', workload_type,
       COUNT(*), COUNT(DISTINCT resource_id)
FROM udm.all.serverless_compat_agent
GROUP BY workload_type
ORDER BY flavor, workload_type;

EOF

# ---------------------------------------------------------------------------
# Block 4: Freshness — rows updated since test start
# Uses _modified_at (generic_resource base field) as the update timestamp.
# last_seen_at is a schema property but may not be populated end-to-end yet.
# ---------------------------------------------------------------------------
cat <<EOF
-- 4. Freshness: rows modified since ${NOW}
--    _modified_at is the generic_resource field updated on each write.
SELECT 'serverless_init_agent' AS flavor, resource_id, workload_type,
       _modified_at,
       TIMESTAMPDIFF(MINUTE, TIMESTAMP '${TODAY}', _modified_at) AS minutes_since_today_start
FROM udm.all.serverless_init_agent
WHERE _modified_at >= TIMESTAMP '${TODAY}'
UNION ALL
SELECT 'serverless_compat_agent', resource_id, workload_type,
       _modified_at,
       TIMESTAMPDIFF(MINUTE, TIMESTAMP '${TODAY}', _modified_at)
FROM udm.all.serverless_compat_agent
WHERE _modified_at >= TIMESTAMP '${TODAY}'
ORDER BY flavor, _modified_at DESC;

EOF

# ---------------------------------------------------------------------------
# Block 5: Crawler relationship joins
# Each row's resource_id must resolve to the expected crawler resource.
# The service-level CCRID (//run.googleapis.com/.../services/{name}) joins
# to gcp_run_service, not gcp_run_revision (revision CCRIDs are different).
# Azure resource_ids are lowercased before SanitizeString → use LOWER().
# ---------------------------------------------------------------------------
cat <<EOF
-- 5a. cloud_run_service: two joins — service-level and revision-level.
--
--     The UDM schema defines the relationship as gcp_run_revision, sourced from
--     /metadata.key_overrides.gcp_run_revision_key (set by the decoder) with
--     on_empty fallback to resource_id (service-level CCRID).
--
--     If the decoder sets a revision CCRID in gcp_run_revision_key:
--       → the gcp_run_revision join resolves; gcp_run_service join may be NULL.
--     If the decoder does not set it (fallback to resource_id):
--       → resource_id is service-level; gcp_run_service join resolves; gcp_run_revision is NULL.
--
--     Check BOTH. Exactly one should be non-NULL per row for the relationship to be healthy.
SELECT a.resource_id, a.workload_type, a._key,
       svc._key  AS gcp_run_service_key,
       rev._key  AS gcp_run_revision_key
FROM udm.all.serverless_init_agent a
LEFT JOIN udm.all.gcp_run_service  svc ON a._key = svc._key
LEFT JOIN udm.all.gcp_run_revision rev ON a._key = rev._key
WHERE a.workload_type = 'cloud_run_service'
$(if [[ -n "${INIT_SERVICE_FILTER}" ]]; then echo "  AND a.resource_id LIKE '%${INIT_SERVICE_FILTER}%'"; fi)
LIMIT 20;
-- PASS: gcp_run_service_key OR gcp_run_revision_key is non-NULL (not both NULL)
-- FAIL (R6): both are NULL → CCRID format doesn't match either crawler table

-- 5b. cloud_function_gen2: same dual join pattern.
--     Gen 2 functions deploy as Cloud Run services; expect gcp_run_service to resolve.
SELECT a.resource_id, a.workload_type, a._key,
       svc._key  AS gcp_run_service_key,
       rev._key  AS gcp_run_revision_key
FROM udm.all.serverless_init_agent a
LEFT JOIN udm.all.gcp_run_service  svc ON a._key = svc._key
LEFT JOIN udm.all.gcp_run_revision rev ON a._key = rev._key
WHERE a.workload_type = 'cloud_function_gen2'
$(if [[ -n "${INIT_SERVICE_FILTER}" ]]; then echo "  AND a.resource_id LIKE '%${INIT_SERVICE_FILTER}%'"; fi)
LIMIT 10;

-- 5c. azure_container_app → azure_container_app crawler join
--     Azure resource_ids are lowercased in the relationship key override.
SELECT a.resource_id, a.workload_type, a._key,
       c._key AS crawler_key
FROM udm.all.serverless_init_agent a
LEFT JOIN udm.all.azure_container_app c ON a._key = c._key
WHERE a.workload_type = 'azure_container_app'
$(if [[ -n "${INIT_SERVICE_FILTER}" ]]; then echo "  AND a.resource_id LIKE '%${INIT_SERVICE_FILTER}%'"; fi)
LIMIT 20;

-- 5d. azure_app_service → azure_app_service crawler join
SELECT a.resource_id, a.workload_type, a._key,
       c._key AS crawler_key
FROM udm.all.serverless_init_agent a
LEFT JOIN udm.all.azure_app_service c ON a._key = c._key
WHERE a.workload_type = 'azure_app_service'
$(if [[ -n "${INIT_SERVICE_FILTER}" ]]; then echo "  AND a.resource_id LIKE '%${INIT_SERVICE_FILTER}%'"; fi)
LIMIT 20;

-- 5e. serverless_compat_agent → gcp_cloudfunctions_function (Gen 1) crawler join
SELECT a.resource_id, a.workload_type, a._key,
       c._key AS crawler_key
FROM udm.all.serverless_compat_agent a
LEFT JOIN udm.all.gcp_cloudfunctions_function c ON a._key = c._key
WHERE a.workload_type = 'gcp_cloud_function_gen1'
$(if [[ -n "${COMPAT_SERVICE_FILTER}" ]]; then echo "  AND a.resource_id LIKE '%${COMPAT_SERVICE_FILTER}%'"; fi)
LIMIT 10;

EOF

# ---------------------------------------------------------------------------
# Block 6: Legacy datadog_agent secondary write (dual-write validation)
# UUID-keyed, one row per cold start. NOT the target for Fleet queries.
# ---------------------------------------------------------------------------
cat <<EOF
-- 6. datadog_agent secondary write (install_method_tool filter)
--    This is the legacy hostless path. These rows are UUID-keyed.
--    Useful for confirming the payload reached EPRW at all.
SELECT _key AS uuid, hostname, agent_version, install_method_tool, _first_seen_at
FROM udm.all.datadog_agent
WHERE install_method_tool IN ('serverless-init', 'serverless-compat')
  AND _first_seen_at >= TIMESTAMP '${TODAY}'
ORDER BY _first_seen_at DESC
LIMIT 20;

EOF

echo "================================================================"
echo "Expected results:"
echo ""
echo "  serverless_init_agent (Blocks 1a, 1b):"
echo "    cloud_run_service     | total_rows = distinct_resources  (SI-01, SI-02)"
echo "    azure_container_app   | total_rows = distinct_resources  (SI-05, SI-06)"
echo "    azure_app_service     | total_rows = distinct_resources  (SI-07, SI-08, SI-09)"
echo "    cloud_function_gen2   | total_rows = distinct_resources  (SI-04)"
echo "    cloud_run_job         | total_rows = distinct_resources  (SI-03) — trigger via Cloud Scheduler + Jobs API"
echo ""
echo "  serverless_compat_agent (Block 2b):"
echo "    azure_function        | total_rows = 1  (SC-01)"
echo "    gcp_cloud_function_gen1 | total_rows = 1  (SC-02)"
echo ""
echo "FAIL conditions:"
echo "  total_rows > distinct_resources → per-instance field in key (R1/R3)"
echo "  crawler_key IS NULL             → resource_id CCRID does not match crawler (R6)"
echo "                                    For GCP services: join is to gcp_run_service (not gcp_run_revision)"
echo "                                    For Azure: _key uses lowercased resource_id"
echo "  rows = 0                        → payload not reaching EPRW — check metric:"
echo "    event_platform_resource_writer.agentmetadata.serverless_write.accepted"
echo "      {resource_type:serverless_init_agent}"
echo "      {resource_type:serverless_compat_agent}"
echo "  Rejection metrics to check (all should be 0):"
echo "    event_platform_resource_writer.agentmetadata.serverless_rejected.missing_resource_id"
echo "    event_platform_resource_writer.agentmetadata.serverless_rejected.missing_resource_name"
echo "    event_platform_resource_writer.agentmetadata.serverless_rejected.missing_workload_type"
echo "    event_platform_resource_writer.agentmetadata.serverless_rejected.invalid_workload_type"
echo "================================================================"
