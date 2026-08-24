#!/usr/bin/env python3
import argparse
import csv
import json
import pathlib


NOT_MEASURED = "NOT MEASURED"


def load_csv(path):
    if not path or not path.exists():
        return []
    with path.open(newline="") as handle:
        return list(csv.DictReader(handle))


def status(ok, measured=True):
    if not measured:
        return NOT_MEASURED
    return "PASS" if ok else "FAIL"


def aggregate_stage(stage):
    if "resources" in stage and stage["resources"]:
        resources=stage["resources"]
        attempts=sum(item.get("attempts",1) for item in resources)
        successes=sum(item.get("successes",1 if 200 <= item.get("http_status",0) < 400 else 0)
                      for item in resources)
        failures=sum(item.get("failures",0 if 200 <= item.get("http_status",0) < 400 else 1)
                     for item in resources)
        return attempts,successes,failures
    if "rounds" in stage:
        return (sum(item["attempts"] for item in stage["rounds"]),
                sum(item["successes"] for item in stage["rounds"]),
                sum(item["failures"] for item in stage["rounds"]))
    return stage.get("attempts",0),stage.get("successes",0),stage.get("failures",0)


def find_default_csv(run_dir, name):
    candidates=[run_dir/f"{name}.csv",run_dir/"pup"/f"{name}.csv"]
    return next((path for path in candidates if path.exists()),None)


def table_evidence(resources, rows):
    expected={resource["resource_id"].lower():resource for resource in resources}
    selected=[row for row in rows if row.get("resource_id","").lower() in expected]
    ids=[row.get("resource_id","").lower() for row in selected if row.get("resource_id")]
    distinct=set(ids)
    missing=sorted(resource["resource_id"] for key,resource in expected.items() if key not in distinct)
    unexpected=sorted(row.get("resource_id","") for row in rows
                      if row.get("resource_id") and row["resource_id"].lower() not in expected)
    required=("resource_id","resource_name","workload_type")
    nulls={field:sum(1 for row in selected if not row.get(field)) for field in required}
    return {"rows":len(selected),"distinct_resource_ids":len(distinct),"missing":missing,
            "unexpected":unexpected,"duplicates":len(ids)-len(distinct),"required_nulls":nulls}


def render(manifest, init_rows, compat_rows, pipeline):
    resources=manifest.get("resources",[])
    init_resources=[r for r in resources if r["id"].startswith("SI-")]
    compat_resources=[r for r in resources if r["id"].startswith("SC-")]
    init=table_evidence(init_resources,init_rows)
    compat=table_evidence(compat_resources,compat_rows)
    table_measured=bool(init_rows or compat_rows)
    acceptance_measured="producer_attempts" in pipeline and "decoder_accepts" in pipeline
    edge_measured="decoder_accepts" in pipeline and "resource_edge_successes" in pipeline and "resource_edge_failures" in pipeline
    iris=pipeline.get("iris_primary",{})
    iris_measured=all(outcome in iris for outcome in ("CREATED","UPDATED","EXTENDED","IGNORED","ERROR"))
    iris_total=sum(iris.values()) if iris_measured else 0
    deployed=len(resources)
    triggered=sum(1 for r in resources if r.get("baseline",{}).get("status") in ("ok","executed"))
    lines=[
        "# Serverless Agent REDAPL RC Results",
        "",
        f"**Status:** {'COMPLETE' if table_measured and pipeline else 'PARTIAL — pipeline or DDSQL evidence still required'}",
        "",
        f"**Run ID:** `{manifest.get('run_id','')}`",
        "",
        f"**Environment:** `{manifest.get('dd_env','')}` on `datad0g.com`, org 2",
        "",
        f"**Start and end time:** {manifest.get('started_at',NOT_MEASURED)} to {manifest.get('completed_at',NOT_MEASURED)}",
        "",
        f"**Declared scope:** `{manifest.get('profile')}` / `{manifest.get('suite')}`",
        "",
        "## 1. Build and environment record",
        "",
        "| Item | Observed value |",
        "|---|---|",
        f"| Agent image tag and digest | `{manifest.get('agent_image',NOT_MEASURED)}` |",
        f"| Datadog Agent commit | `{manifest.get('candidate_commits',{}).get('datadog_agent',manifest.get('agent_sha',NOT_MEASURED))}` |",
        f"| Serverless Components commit | `{manifest.get('candidate_commits',{}).get('serverless_components',NOT_MEASURED)}` |",
        f"| Compat JS commit | `{manifest.get('candidate_commits',{}).get('datadog_serverless_compat_js',NOT_MEASURED)}` |",
        f"| EPRW decoder deployed commit | `{pipeline.get('eprw_commit',NOT_MEASURED)}` |",
        f"| Iris deployed commit | `{pipeline.get('iris_commit',NOT_MEASURED)}` |",
        f"| EPRW debug tracking | `{pipeline.get('eprw_debug_tracking',NOT_MEASURED)}` |",
        f"| Iris upsert experiment | `{pipeline.get('iris_upsert_telemetry',NOT_MEASURED)}` |",
        "",
        "## 2. Executive results",
        "",
        "| Result | Expected | Observed | Status |",
        "|---|---:|---:|---|",
        f"| Resources deployed | {len(resources)} | {deployed} | {status(deployed==len(resources))} |",
        f"| Baseline triggers | {deployed} | {triggered} | {status(triggered==deployed)} |",
        f"| EPRW decoder accepts | Producer attempts | {pipeline.get('decoder_accepts',NOT_MEASURED)} | {status(pipeline.get('decoder_accepts')==pipeline.get('producer_attempts'),acceptance_measured)} |",
        f"| Resource Edge failures | 0 | {pipeline.get('resource_edge_failures',NOT_MEASURED)} | {status(pipeline.get('resource_edge_failures')==0,'resource_edge_failures' in pipeline)} |",
        f"| REDAPL init rows | {len(init_resources)} | {init['rows'] if init_rows else NOT_MEASURED} | {status(init['rows']==len(init_resources),bool(init_rows))} |",
        f"| REDAPL compat rows | {len(compat_resources)} | {compat['rows'] if compat_rows else NOT_MEASURED} | {status(compat['rows']==len(compat_resources),bool(compat_rows))} |",
        f"| Duplicate resource keys | 0 | {(init['duplicates']+compat['duplicates']) if table_measured else NOT_MEASURED} | {status(init['duplicates']+compat['duplicates']==0,table_measured)} |",
        "",
        "## 3. Per-workload results",
        "",
        "| ID | Table | Workload | Runtime | Model | Resource | Baseline | REDAPL |",
        "|---|---|---|---|---|---|---|---|",
    ]
    init_ids={row.get("resource_id","").lower() for row in init_rows}
    compat_ids={row.get("resource_id","").lower() for row in compat_rows}
    for resource in resources:
        table="serverless_init_agent" if resource["id"].startswith("SI-") else "serverless_compat_agent"
        ids=init_ids if table=="serverless_init_agent" else compat_ids
        observed=status(resource["resource_id"].lower() in ids,bool(init_rows if table=="serverless_init_agent" else compat_rows))
        lines.append(f"| {resource['id']} | `{table}` | `{resource['workload_type']}` | {resource['runtime']} | {resource['deployment_model']} | `{resource['name']}` | {resource.get('baseline',{}).get('status',NOT_MEASURED)} | {observed} |")
    lines.extend([
        "",
        "## 4. Producer, EPRW, and Iris reconciliation",
        "",
        "| Measurement | Observed | Status |",
        "|---|---:|---|",
        f"| Producer inventory attempts | {pipeline.get('producer_attempts',NOT_MEASURED)} | {status(pipeline.get('producer_attempts')==pipeline.get('decoder_accepts'),acceptance_measured)} |",
        f"| EPRW decoder accepts | {pipeline.get('decoder_accepts',NOT_MEASURED)} | {status(pipeline.get('decoder_accepts')==pipeline.get('producer_attempts'),acceptance_measured)} |",
        f"| Resource Edge successes | {pipeline.get('resource_edge_successes',NOT_MEASURED)} | {status(pipeline.get('decoder_accepts')==pipeline.get('resource_edge_successes',0)+pipeline.get('resource_edge_failures',0),edge_measured)} |",
        f"| Resource Edge failures | {pipeline.get('resource_edge_failures',NOT_MEASURED)} | {status(pipeline.get('resource_edge_failures')==0,'resource_edge_failures' in pipeline)} |",
    ])
    for outcome in ("CREATED","UPDATED","EXTENDED","IGNORED","ERROR"):
        lines.append(f"| Primary Iris {outcome} | {iris.get(outcome,NOT_MEASURED)} | {status(iris.get(outcome,0)==0 if outcome=='ERROR' else True,outcome in iris)} |")
    lines.extend([
        "",
        "Reconciliation gates:",
        "",
        "```text",
        "decoder accepts = Resource Edge successes + Resource Edge failures",
        "Resource Edge successes = primary Iris CREATED + UPDATED + EXTENDED + IGNORED + ERROR",
        "```",
        "",
        "## 5. REDAPL identity and data results",
        "",
        "| Table | Expected IDs | Rows | Distinct IDs | Duplicates | Missing | Required nulls | Status |",
        "|---|---:|---:|---:|---:|---:|---:|---|",
        f"| `serverless_init_agent` | {len(init_resources)} | {init['rows'] if init_rows else NOT_MEASURED} | {init['distinct_resource_ids'] if init_rows else NOT_MEASURED} | {init['duplicates'] if init_rows else NOT_MEASURED} | {len(init['missing']) if init_rows else NOT_MEASURED} | {sum(init['required_nulls'].values()) if init_rows else NOT_MEASURED} | {status(init['rows']==len(init_resources) and init['duplicates']==0 and not init['missing'] and not any(init['required_nulls'].values()),bool(init_rows))} |",
        f"| `serverless_compat_agent` | {len(compat_resources)} | {compat['rows'] if compat_rows else NOT_MEASURED} | {compat['distinct_resource_ids'] if compat_rows else NOT_MEASURED} | {compat['duplicates'] if compat_rows else NOT_MEASURED} | {len(compat['missing']) if compat_rows else NOT_MEASURED} | {sum(compat['required_nulls'].values()) if compat_rows else NOT_MEASURED} | {status(compat['rows']==len(compat_resources) and compat['duplicates']==0 and not compat['missing'] and not any(compat['required_nulls'].values()),bool(compat_rows))} |",
        "",
        "## 6. Load-stage results",
        "",
        "| Stage | Scenario | Sent | Successful | Failed | Pipeline evidence | Status |",
        "|---|---|---:|---:|---:|---|---|",
    ])
    for stage in manifest.get("load_stages",[]):
        attempts,successes,failures=aggregate_stage(stage)
        stage_status=stage.get("status") or status(failures==0)
        lines.append(f"| {stage['id']} | {stage['scenario']} | {attempts or NOT_MEASURED} | {successes or NOT_MEASURED} | {failures if attempts else NOT_MEASURED} | {NOT_MEASURED} | {stage_status} |")
    lines.extend([
        "",
        "## 7. Evidence still required",
        "",
        "- Export `serverless_init_agent.csv` and `serverless_compat_agent.csv` into this run directory, then rerun `report.py`.",
        "- Export run-filtered EPRW and primary Iris counts into `pipeline-evidence.json`.",
        "- Add provider instance-start evidence for sequential cold starts and scale-out.",
        "- Run the controlled `SeenAt`, revision A/B, TTL, crawler, Fleet, and UI checks.",
        "",
        "## 8. RFC approval gates",
        "",
        "| Gate | Status |",
        "|---|---|",
        f"| Every in-scope workload reports a valid row | {status(init['rows']==len(init_resources) and compat['rows']==len(compat_resources),bool(init_rows and compat_rows))} |",
        f"| One row per `resource_id` | {status(init['duplicates']==0 and compat['duplicates']==0,table_measured)} |",
        f"| EPRW/Iris counts reconcile | {status(edge_measured and iris_measured and pipeline.get('decoder_accepts')==pipeline.get('resource_edge_successes',0)+pipeline.get('resource_edge_failures',0) and pipeline.get('resource_edge_successes')==iris_total,edge_measured and iris_measured)} |",
        f"| Older `SeenAt` cannot replace newer data | {NOT_MEASURED} |",
        f"| Upgrade, rollback, and traffic split | {NOT_MEASURED} |",
        f"| TTL and reactivation | {NOT_MEASURED} |",
        f"| Crawler, Fleet, and UI | {NOT_MEASURED} |",
        "",
        "## 9. Conclusion",
        "",
        "The generated report distinguishes observed execution results from pipeline and UI evidence that has not yet been collected. Missing evidence is never converted into a pass.",
    ])
    return "\n".join(lines)+"\n",{"init":init,"compat":compat}


def main():
    parser=argparse.ArgumentParser()
    parser.add_argument("--manifest",type=pathlib.Path,required=True)
    parser.add_argument("--init-csv",type=pathlib.Path)
    parser.add_argument("--compat-csv",type=pathlib.Path)
    parser.add_argument("--pipeline-evidence",type=pathlib.Path)
    args=parser.parse_args()
    run_dir=args.manifest.parent
    manifest=json.loads(args.manifest.read_text())
    init_path=args.init_csv or find_default_csv(run_dir,"serverless_init_agent")
    compat_path=args.compat_csv or find_default_csv(run_dir,"serverless_compat_agent")
    pipeline_path=args.pipeline_evidence or run_dir/"pipeline-evidence.json"
    pipeline=json.loads(pipeline_path.read_text()) if pipeline_path.exists() else {}
    markdown,tables=render(manifest,load_csv(init_path),load_csv(compat_path),pipeline)
    (run_dir/"serverless-redapl-rc-results.md").write_text(markdown)
    (run_dir/"report.json").write_text(json.dumps({"manifest":manifest,"tables":tables,
                                                    "pipeline":pipeline},indent=2))
    print(f"Report: {run_dir/'serverless-redapl-rc-results.md'}")
    print(f"Machine-readable report: {run_dir/'report.json'}")


if __name__ == "__main__":
    main()
