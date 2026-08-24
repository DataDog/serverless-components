# SVLS-9604 independent workload runner

This runner builds the candidate `serverless-init` binary from the local
`datadog-agent` checkout and the Compat package from the local
`serverless-components` and `datadog-serverless-compat-js` checkouts. It does
not invoke either self-monitoring repository.

Authentication is always supplied internally through:

```sh
dd-auth --site=datad0g.com --org-uuid=2
```

Preview either exact resource matrix without creating resources:

```sh
./scripts/svls9604/run.sh --profile gcp --plan
./scripts/svls9604/run.sh --profile azure --plan
```

Execute a profile:

```sh
./scripts/svls9604/run.sh --profile gcp --yes
./scripts/svls9604/run.sh --profile azure --yes
```

These commands run the full automated suite by default:

- L0: one baseline request or execution per deployed resource
- L1: 10 concurrent requests per HTTP init resource
- L2: 50 concurrent requests per HTTP init resource
- L3: 100 concurrent requests per HTTP init resource
- L4: one concurrent request per distinct HTTP resource
- L5: 15 minutes of unchanged traffic at one-minute intervals

L6 changed-value and L7 traffic-split scenarios are included in the generated
report as `NOT MEASURED` until their provider-specific mutation fixtures run.
Use `--suite baseline` for deployment debugging without the load stages.

The GCP profile creates 44 resources: 14 Cloud Run in-container services, 14
Cloud Run sidecars, one Cloud Run job, 14 real Gen2 Cloud Functions with a
sidecar attached to the backing Cloud Run service, and one Gen1 Compat
function.

The Azure profile creates 63 resources: 14 Container Apps in-container, 14
Container Apps sidecar, 14 App Service container in-container, 14 App Service
container sidecar, six App Service Linux-code sidecars, and one Node.js Compat
Function App.

Azure defaults target the existing test infrastructure:

- Azure Functions: resource group `dd-serverless-test-aca` (the App Service test
  resource group does not permit Linux Dynamic workers)

- Container Apps: `dd-serverless-test-aca/dd-serverless-env`
- ACR: `ddsvlstestaca.azurecr.io`
- App Service: `dd-serverless-test-aas` and its container, sidecar, and
  Linux-code plans

Every default is overridable through the corresponding CLI option or
environment variable. Each run writes a mode-`0700` evidence directory under
`/tmp/svls9604-RUN_ID` with a machine-readable deployment manifest. Generated
parameter files are mode `0600` because they contain temporary secure values.

The runner sets `DD_ENV=svls9604-RUN_ID` and records `started_at`, `updated_at`,
and `completed_at` in the manifest. This makes a run isolatable in EPRW metrics
without using high-cardinality `resource_id` metric tags.

Current automated scope is deployment inventory, endpoint/job baseline
execution, L1-L5 load stages, and report generation. An HTTP response alone is
not proof that inventory traversed EPRW or deduplicated in Iris.

The runner always rebuilds the Serverless Compat binary before packaging it.
This prevents a stale `target/` artifact from being deployed under a new run ID.

Each run produces `serverless-redapl-rc-results.md` and `report.json`. To add
table and pipeline evidence after the run, place these files in the evidence
directory and rerun `report.py`:

- `serverless_init_agent.csv`
- `serverless_compat_agent.csv`
- `pipeline-evidence.json`

```sh
python3 scripts/svls9604/report.py --manifest /path/to/run-manifest.json
```

## Pipeline evidence for an RFC run

Before the run, temporarily enable full EPRW debug tracking for org 2 and the
two resource types:

```ini
[dd.event_platform_resource_writer.debug_tracking.track_type_resource_type:agentmetadata:serverless_init_agent:2]
sampling_rate = 1

[dd.event_platform_resource_writer.debug_tracking.track_type_resource_type:agentmetadata:serverless_compat_agent:2]
sampling_rate = 1
```

Also enable the Iris experiment `serverless-inventory-upsert-telemetry`. It
emits one structured log per serverless upsert with `resource_id`, hashed
resource identity, result, origin Kafka partition/offset, and `shadow_mode`.
Disable these temporary controls after the validation window.

Collect these counts for the manifest's exact time window:

1. Producer starts: Cloud logs containing `inventory report queued
   reason=startup` for init and `Inventory payload sent
   (report_reason=startup` for Compat. Group by cloud resource and revision.
2. Decoder accepts:
   `event_platform_resource_writer.agentmetadata.serverless_write.accepted`
   filtered by the manifest's unique `dd_env`.
3. Resource-edge responses:
   `event_platform_resource_writer.agentmetadata.serverless_write.resource_edge_response`
   filtered by `dd_env`, grouped by `outcome`.
4. Iris outcomes: feature-gated `serverless inventory upsert result` logs,
   restricted to `shadow_mode:false` and the manifest resource names, grouped
   by `resource_id` and `result`.
5. Final rows: DDSQL rows from `udm.all.serverless_init_agent` and
   `udm.all.serverless_compat_agent` whose `resource_id` contains the run ID.

The hard gates are:

```text
decoder accepts = resource-edge ok + resource-edge failures
resource-edge ok = primary Iris CREATED + UPDATED + EXTENDED + IGNORED + ERROR
unique DDSQL resource_id count = manifest resource count
duplicate DDSQL keys = 0
```

Interpret Iris results per stable `resource_id`: `CREATED` is the first row,
`UPDATED` is a real configuration change, `EXTENDED` is an unchanged report
deduplicated to the existing row, and `IGNORED` is stale/out-of-order. A cold
start or new process UUID may increase report attempts, but must not increase
row cardinality. Use `--skip-burst` only for deployment debugging.
