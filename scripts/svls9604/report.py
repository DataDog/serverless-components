#!/usr/bin/env python3
import argparse
import csv
import json
import pathlib


NOT_MEASURED = "NOT MEASURED"

def measured_number(value):
    return isinstance(value,(int,float)) and not isinstance(value,bool)


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

def resource_expected_ids(resource):
    return resource.get("expected_resource_ids") or [resource["resource_id"]]

def table_evidence(resources, rows):
    expected={resource_id.lower():resource for resource in resources
              for resource_id in resource_expected_ids(resource)}
    selected=[row for row in rows if row.get("resource_id","").lower() in expected]
    ids=[row.get("resource_id","").lower() for row in selected if row.get("resource_id")]
    distinct=set(ids)
    missing=sorted(key for key in expected if key not in distinct)
    unexpected=sorted(row.get("resource_id","") for row in rows
                      if row.get("resource_id") and row["resource_id"].lower() not in expected)
    required=("resource_id","resource_name","workload_type")
    nulls={field:sum(1 for row in selected if not row.get(field)) for field in required}
    revision_rows=[row for row in selected if row.get("workload_type") in
                   ("cloud_run_service","cloud_run_function","azure_container_app")]
    nulls["parent_resource_id"]=sum(1 for row in revision_rows if not row.get("parent_resource_id"))
    nulls["deployment_id"]=sum(1 for row in revision_rows if not row.get("deployment_id"))
    return {"rows":len(selected),"distinct_resource_ids":len(distinct),"missing":missing,
            "unexpected":unexpected,"duplicates":len(ids)-len(distinct),"required_nulls":nulls}


def render(manifest, init_rows, compat_rows, pipeline):
    resources=manifest.get("resources",[])
    init_resources=[r for r in resources if r["id"].startswith("SI-")]
    compat_resources=[r for r in resources if r["id"].startswith("SC-")]
    init=table_evidence(init_resources,init_rows)
    compat=table_evidence(compat_resources,compat_rows)
    table_measured=bool(init_rows or compat_rows)
    acceptance_measured=all(measured_number(pipeline.get(key)) for key in ("producer_attempts","decoder_accepts"))
    edge_measured=all(measured_number(pipeline.get(key)) for key in ("decoder_accepts","resource_edge_successes","resource_edge_failures"))
    iris=pipeline.get("iris_primary",{})
    iris_measured=all(measured_number(iris.get(outcome)) for outcome in ("CREATED","UPDATED","EXTENDED","IGNORED","ERROR"))
    iris_total=sum(iris.values()) if iris_measured else 0
    producer_reasons=pipeline.get("producer_reasons",{})
    decoder_reasons=pipeline.get("decoder_reasons",{})
    reason_names=("startup","periodic","refresh")
    reasons_measured=all(
        measured_number(producer_reasons.get(reason)) and measured_number(decoder_reasons.get(reason))
        for reason in reason_names
    )
    deployed=len(resources)
    expected_init_ids=sum(len(resource_expected_ids(r)) for r in init_resources)
    expected_compat_ids=sum(len(resource_expected_ids(r)) for r in compat_resources)
    revision_stages={stage.get("id"):stage for stage in manifest.get("load_stages",[]) if stage.get("id") in ("L6","L7")}
    revision_measured=all(stage_id in revision_stages and revision_stages[stage_id].get("status") != NOT_MEASURED
                          for stage_id in ("L6","L7"))
    stage_pipeline_complete=all(
        all(measured_number(pipeline.get("stages",{}).get(stage.get("id"),{}).get(key))
            for key in ("producer_attempts","decoder_accepts","resource_edge_successes","resource_edge_failures"))
        for stage in manifest.get("load_stages",[])
    )
    expected_revision_ids={resource_id.lower() for resource in init_resources for resource_id in resource_expected_ids(resource)}
    resource_pipeline_complete=all(
        resource_id in {key.lower() for key in pipeline.get("resources",{})}
        for resource_id in expected_revision_ids
    ) and all(
        all(measured_number(evidence.get(key)) for key in ("producer_attempts","decoder_accepts","resource_edge_successes")) and
        all(measured_number(evidence.get("iris_primary",{}).get(outcome)) for outcome in ("CREATED","UPDATED","EXTENDED","IGNORED","ERROR"))
        for evidence in pipeline.get("resources",{}).values()
    )
    tables_valid=(init["rows"]==expected_init_ids and compat["rows"]==expected_compat_ids and
                  init["duplicates"]==0 and compat["duplicates"]==0 and not init["missing"] and
                  not compat["missing"] and not init["unexpected"] and not compat["unexpected"] and
                  not any(init["required_nulls"].values()) and not any(compat["required_nulls"].values()))
    reason_totals_valid=(reasons_measured and
                         sum(producer_reasons.values())==pipeline.get("producer_attempts") and
                         sum(decoder_reasons.values())==pipeline.get("decoder_accepts") and
                         all(producer_reasons[reason]==decoder_reasons[reason] for reason in reason_names))
    pipeline_valid=(acceptance_measured and edge_measured and iris_measured and reason_totals_valid and
                    pipeline.get("producer_attempts")==pipeline.get("decoder_accepts") and
                    pipeline.get("decoder_accepts")==pipeline.get("resource_edge_successes",0)+pipeline.get("resource_edge_failures",0) and
                    pipeline.get("resource_edge_failures")==0 and pipeline.get("resource_edge_successes")==iris_total)
    complete=table_measured and tables_valid and pipeline_valid and revision_measured and stage_pipeline_complete and resource_pipeline_complete
    triggered=sum(1 for r in resources if r.get("baseline",{}).get("status") in ("ok","executed"))
    lines=[
        "# Serverless Agent REDAPL RC Results",
        "",
        f"**Status:** {'COMPLETE' if complete else 'PARTIAL — pipeline, revision, or DDSQL evidence still required'}",
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
        f"| REDAPL init revision/workload rows | {expected_init_ids} | {init['rows'] if init_rows else NOT_MEASURED} | {status(init['rows']==expected_init_ids,bool(init_rows))} |",
        f"| REDAPL compat rows | {expected_compat_ids} | {compat['rows'] if compat_rows else NOT_MEASURED} | {status(compat['rows']==expected_compat_ids,bool(compat_rows))} |",
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
        observed=status(all(resource_id.lower() in ids for resource_id in resource_expected_ids(resource)),bool(init_rows if table=="serverless_init_agent" else compat_rows))
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
        "### Collection reasons",
        "",
        "| Reason | Producer reports | EPRW accepts | Status |",
        "|---|---:|---:|---|",
    ])
    for reason in reason_names:
        producer=producer_reasons.get(reason,NOT_MEASURED)
        decoder=decoder_reasons.get(reason,NOT_MEASURED)
        measured=measured_number(producer) and measured_number(decoder)
        lines.append(f"| `{reason}` | {producer} | {decoder} | {status(producer==decoder,measured)} |")
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
        "| Table | Expected IDs | Rows | Distinct IDs | Duplicates | Missing | Unexpected | Required nulls | Status |",
        "|---|---:|---:|---:|---:|---:|---:|---:|---|",
        f"| `serverless_init_agent` | {expected_init_ids} | {init['rows'] if init_rows else NOT_MEASURED} | {init['distinct_resource_ids'] if init_rows else NOT_MEASURED} | {init['duplicates'] if init_rows else NOT_MEASURED} | {len(init['missing']) if init_rows else NOT_MEASURED} | {len(init['unexpected']) if init_rows else NOT_MEASURED} | {sum(init['required_nulls'].values()) if init_rows else NOT_MEASURED} | {status(init['rows']==expected_init_ids and init['duplicates']==0 and not init['missing'] and not init['unexpected'] and not any(init['required_nulls'].values()),bool(init_rows))} |",
        f"| `serverless_compat_agent` | {expected_compat_ids} | {compat['rows'] if compat_rows else NOT_MEASURED} | {compat['distinct_resource_ids'] if compat_rows else NOT_MEASURED} | {compat['duplicates'] if compat_rows else NOT_MEASURED} | {len(compat['missing']) if compat_rows else NOT_MEASURED} | {len(compat['unexpected']) if compat_rows else NOT_MEASURED} | {sum(compat['required_nulls'].values()) if compat_rows else NOT_MEASURED} | {status(compat['rows']==expected_compat_ids and compat['duplicates']==0 and not compat['missing'] and not compat['unexpected'] and not any(compat['required_nulls'].values()),bool(compat_rows))} |",
        "",
        "## 6. Load-stage results",
        "",
        "| Stage | Scenario | Sent | Successful | Failed | Pipeline evidence | Status |",
        "|---|---|---:|---:|---:|---|---|",
    ])
    for stage in manifest.get("load_stages",[]):
        attempts,successes,failures=aggregate_stage(stage)
        evidence=pipeline.get("stages",{}).get(stage["id"],{})
        stage_pipeline_measured=all(measured_number(evidence.get(key)) for key in ("producer_attempts","decoder_accepts","resource_edge_successes","resource_edge_failures"))
        pipeline_summary=(f"producer={evidence['producer_attempts']}, decoder={evidence['decoder_accepts']}, "
                          f"edge_ok={evidence['resource_edge_successes']}, edge_failed={evidence['resource_edge_failures']}") if stage_pipeline_measured else NOT_MEASURED
        if stage.get("status") == NOT_MEASURED:
            stage_status=NOT_MEASURED
        elif failures:
            stage_status="FAIL"
        elif not stage_pipeline_measured:
            stage_status="PARTIAL"
        else:
            stage_status=status(evidence["producer_attempts"]==evidence["decoder_accepts"] and
                                evidence["decoder_accepts"]==evidence["resource_edge_successes"]+evidence["resource_edge_failures"] and
                                evidence["resource_edge_failures"]==0)
        lines.append(f"| {stage['id']} | {stage['scenario']} | {attempts or NOT_MEASURED} | {successes or NOT_MEASURED} | {failures if attempts else NOT_MEASURED} | {pipeline_summary} | {stage_status} |")
    lines.extend([
        "",
        "### Per-revision reconciliation",
        "",
        "| Resource ID | Producer starts | Decoder accepts | Edge successes | Iris total | Status |",
        "|---|---:|---:|---:|---:|---|",
    ])
    for resource_id,evidence in sorted(pipeline.get("resources",{}).items()):
        per_iris=evidence.get("iris_primary",{})
        per_iris_measured=all(measured_number(per_iris.get(outcome)) for outcome in ("CREATED","UPDATED","EXTENDED","IGNORED","ERROR"))
        per_measured=all(measured_number(evidence.get(key)) for key in ("producer_attempts","decoder_accepts","resource_edge_successes")) and per_iris_measured
        per_iris_total=sum(per_iris.values()) if per_iris_measured else NOT_MEASURED
        per_status=status(evidence.get("producer_attempts")==evidence.get("decoder_accepts")==evidence.get("resource_edge_successes")==per_iris_total,per_measured)
        lines.append(f"| `{resource_id}` | {evidence.get('producer_attempts',NOT_MEASURED)} | {evidence.get('decoder_accepts',NOT_MEASURED)} | {evidence.get('resource_edge_successes',NOT_MEASURED)} | {per_iris_total} | {per_status} |")
    if not pipeline.get("resources"):
        lines.append(f"| {NOT_MEASURED} | {NOT_MEASURED} | {NOT_MEASURED} | {NOT_MEASURED} | {NOT_MEASURED} | {NOT_MEASURED} |")
    lines.extend([
        "",
        "## 7. Staging evidence queries",
        "",
        "Use the run time window shown above and the following filters in `datad0g.com`:",
        "",
        "```text",
        f"Environment: {manifest.get('dd_env','')}",
        f"EPRW accepts: sum:event_platform_resource_writer.agentmetadata.serverless_write.accepted{{resource_type:serverless_init_agent}} by {{resource_type,workload_type,deployment_model,report_reason}}",
        f"EPRW accepts (compat): sum:event_platform_resource_writer.agentmetadata.serverless_write.accepted{{resource_type:serverless_compat_agent}} by {{resource_type,workload_type,report_reason}}",
        f"Producer startup log (Init): env:{manifest.get('dd_env','')} \"serverless-init: inventory report queued\"",
        f"Iris primary logs: service:iris-node-go @message:\"serverless inventory upsert result\" @shadow_mode:false @resource_id:*{manifest.get('run_id','').lower()}*",
        "EPRW debug logs: service:event-platform-resource-writer @resource_id:*RUN_ID*",
        "```",
        "",
        "For L6, record provider startup/replica counts for `revision_b`; the request total is pressure applied, not the number of cold starts. For L7, confirm both revision resource IDs reach EPRW/Iris and both remain related to the same parent service/app.",
        "",
        "DDSQL exports:",
        "",
        "```sql",
        "SELECT resource_id, parent_resource_id, resource_name, workload_type, deployment_model, deployment_id, runtime, dd_env, _first_seen_at, _modification_detected_at, _updated_at",
        "FROM udm.all.serverless_init_agent",
        f"WHERE dd_env = '{manifest.get('dd_env','')}'",
        "ORDER BY parent_resource_id, deployment_id, resource_id;",
        "",
        "SELECT resource_id, resource_name, workload_type, runtime, dd_env, _first_seen_at, _modification_detected_at, _updated_at",
        "FROM udm.all.serverless_compat_agent",
        f"WHERE dd_env = '{manifest.get('dd_env','')}'",
        "ORDER BY resource_id;",
        "```",
        "",
        "## 8. Evidence still required",
        "",
        "- Export `serverless_init_agent.csv` and `serverless_compat_agent.csv` into this run directory, then rerun `report.py`.",
        "- Export run-filtered EPRW and primary Iris counts into `pipeline-evidence.json`.",
        "- Add producer and EPRW counts grouped by `report_reason` (`startup`, `periodic`, `refresh`).",
        "- Add provider instance-start evidence for sequential cold starts and scale-out.",
        "- Run the controlled `SeenAt`, revision A/B, TTL, crawler, Fleet, and UI checks.",
        "",
        "## 9. RFC approval gates",
        "",
        "| Gate | Status |",
        "|---|---|",
        f"| Every in-scope workload/revision reports a valid row | {status(init['rows']==expected_init_ids and compat['rows']==expected_compat_ids,bool(init_rows and compat_rows))} |",
        f"| One row per `resource_id` | {status(init['duplicates']==0 and compat['duplicates']==0,table_measured)} |",
        f"| EPRW/Iris counts reconcile | {status(edge_measured and iris_measured and pipeline.get('decoder_accepts')==pipeline.get('resource_edge_successes',0)+pipeline.get('resource_edge_failures',0) and pipeline.get('resource_edge_successes')==iris_total,edge_measured and iris_measured)} |",
        f"| Producer reasons reconcile with EPRW | {status(all(producer_reasons.get(reason)==decoder_reasons.get(reason) for reason in reason_names),reasons_measured)} |",
        f"| Older `SeenAt` cannot replace newer data | {NOT_MEASURED} |",
        f"| Revision creation and traffic split executed | {status(revision_measured,revision_measured)} |",
        f"| TTL and reactivation | {NOT_MEASURED} |",
        f"| Crawler, Fleet, and UI | {NOT_MEASURED} |",
        "",
        "## 10. Conclusion",
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
    producer_path=run_dir/"producer-evidence.json"
    if producer_path.exists():
        producer=json.loads(producer_path.read_text())
        pipeline.setdefault("producer_attempts",producer.get("events"))
        pipeline.setdefault("producer_reasons",producer.get("reasons",{}))
        pipeline.setdefault("stages",{})
        for stage_id,evidence in producer.get("stages",{}).items():
            pipeline["stages"].setdefault(stage_id,{})
            pipeline["stages"][stage_id].setdefault("producer_attempts",evidence.get("reports"))
        pipeline.setdefault("resources",{})
        for resource_id,evidence in producer.get("resources",{}).items():
            pipeline["resources"].setdefault(resource_id,{})
            pipeline["resources"][resource_id].setdefault("producer_attempts",evidence.get("reports"))
    markdown,tables=render(manifest,load_csv(init_path),load_csv(compat_path),pipeline)
    (run_dir/"serverless-redapl-rc-results.md").write_text(markdown)
    (run_dir/"report.json").write_text(json.dumps({"manifest":manifest,"tables":tables,
                                                    "pipeline":pipeline},indent=2))
    print(f"Report: {run_dir/'serverless-redapl-rc-results.md'}")
    print(f"Machine-readable report: {run_dir/'report.json'}")


if __name__ == "__main__":
    main()
