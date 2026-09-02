#!/usr/bin/env python3
import argparse
import concurrent.futures
import datetime as dt
import hashlib
import json
import os
import pathlib
import re
import secrets
import shlex
import subprocess
import sys
import tempfile
import time
import urllib.request
import zipfile

ROOT = pathlib.Path(__file__).resolve().parent
WORKSPACE = ROOT.parents[2]
AGENT_REPO = pathlib.Path(os.environ.get("DATADOG_AGENT_DIR", WORKSPACE / "datadog-agent"))
COMPONENTS_REPO = pathlib.Path(os.environ.get("SERVERLESS_COMPONENTS_DIR", WORKSPACE / "serverless-components"))
COMPAT_JS_REPO = pathlib.Path(os.environ.get("COMPAT_JS_DIR", WORKSPACE / "datadog-serverless-compat-js"))
MATRIX = json.loads((ROOT / "matrix.json").read_text())
RUN_ENV = "svls9604"
RUN_STARTED_AT = None

FULL_LOAD_STAGES = (("L1", 10), ("L2", 50), ("L3", 100))

def aggregate_stage(stage):
    if stage.get("resources"):
        return (sum(item.get("attempts",1) for item in stage["resources"]),
                sum(item.get("successes",1 if 200 <= item.get("http_status",0) < 400 else 0) for item in stage["resources"]),
                sum(item.get("failures",0 if 200 <= item.get("http_status",0) < 400 else 1) for item in stage["resources"]))
    if stage.get("rounds"):
        return (sum(item["attempts"] for item in stage["rounds"]),
                sum(item["successes"] for item in stage["rounds"]),
                sum(item["failures"] for item in stage["rounds"]))
    return stage.get("attempts",0),stage.get("successes",0),stage.get("failures",0)

def run(cmd, *, cwd=None, capture=False, env=None):
    shown = " ".join(shlex.quote(str(x)) for x in cmd)
    api_key = os.environ.get("DD_API_KEY")
    app_key = os.environ.get("DD_APP_KEY")
    acr_password = os.environ.get("SVLS9604_ACR_PASSWORD")
    if api_key:
        shown = shown.replace(api_key, "***DD_API_KEY***")
    if app_key:
        shown = shown.replace(app_key, "***DD_APP_KEY***")
    if acr_password:
        shown = shown.replace(acr_password, "***ACR_PASSWORD***")
    print(f"+ {shown}", flush=True)
    merged = os.environ.copy()
    if env:
        merged.update(env)
    result = subprocess.run(cmd, cwd=cwd, env=merged, text=True,
                            stdout=subprocess.PIPE if capture else None,
                            stderr=subprocess.PIPE if capture else None)
    if result.returncode:
        if capture:
            print(result.stdout, end="", file=sys.stderr)
            print(result.stderr, end="", file=sys.stderr)
        raise RuntimeError(f"command failed ({result.returncode}): {shown}")
    return result.stdout.strip() if capture else ""

def git_sha(repo):
    return run(["git", "rev-parse", "HEAD"], cwd=repo, capture=True)

def utc_now():
    return dt.datetime.now(dt.timezone.utc).isoformat()


def write_manifest(path, *, run_id, profile, agent_sha, agent_image, resources,
                   stages=None, suite="full", **provider):
    path.write_text(json.dumps({
        "run_id":run_id,
        "dd_env":RUN_ENV,
        "profile":profile,
        "started_at":RUN_STARTED_AT,
        "updated_at":utc_now(),
        "suite":suite,
        "agent_sha":agent_sha,
        "agent_image":agent_image,
        **provider,
        "resources":resources,
        "load_stages":stages or [],
    },indent=2))

def remote_image_exists(image):
    return subprocess.run(["docker", "buildx", "imagetools", "inspect", image],
                          stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL).returncode == 0

def short_runtime(runtime):
    return {"python":"py", "node":"node", "go":"go", "java":"java", "dotnet":"dotnet", "ruby":"ruby", "php":"php"}[runtime]

def expand(profile):
    resources=[]
    for item in MATRIX["profiles"][profile]:
        runtimes=MATRIX["runtimes"] if item["runtimes"] == "all" else item["runtimes"]
        for runtime in runtimes:
            for variant in item["variants"]:
                resources.append({**item, "runtime":runtime, "variant":variant})
    return resources

def name_for(run_id, r):
    model={"in-container":"in", "sidecar":"sc", "compat":"compat"}.get(r.get("deployment_model","compat"),"compat")
    if r["provider"] == "azure":
        # Container Apps names are limited to 32 characters. These names are
        # also globally unique enough for App Service because run_id is random.
        return f"sv{run_id[:10]}-{r['id'].lower().replace('-', '')}-{model}-{short_runtime(r['runtime'])}-{r['variant'][:4]}"[:32].rstrip("-")
    return f"sv-{run_id}-{r['id'].lower().replace('-', '')}-{model}-{short_runtime(r['runtime'])}-{r['variant'][:4]}"[:63]

def preflight(args):
    required=["docker", "git", "python3", "gcloud" if args.profile in ("gcp", "gcp-sanity") else "az"]
    for tool in required:
        run(["sh", "-c", f"command -v {shlex.quote(tool)} >/dev/null"])
    if not os.environ.get("DD_API_KEY") or os.environ.get("DD_SITE") != "datad0g.com":
        raise RuntimeError("runner must be invoked through dd-auth for datad0g.com")
    if args.profile in ("gcp", "gcp-sanity"):
        account=run(["gcloud","auth","list","--filter=status:ACTIVE","--format=value(account)"],capture=True)
        if not account:
            raise RuntimeError("gcloud has no active account")
        print(f"Authenticated gcloud account: {account}")
    else:
        account=run(["az","account","show","--query","{name:name,id:id}","-o","json"],capture=True)
        print(f"Authenticated Azure subscription: {account}")
    run(["docker","info","--format={{.ServerVersion}}"],capture=True)
    print(f"Datadog site: {os.environ['DD_SITE']} (dd-auth org UUID 2)")

def image_digest(image):
    output=run(["docker","buildx","imagetools","inspect",image],capture=True)
    match=re.search(r"^Digest:\s+(sha256:[0-9a-f]+)$",output,re.MULTILINE)
    if not match:
        raise RuntimeError(f"could not determine digest for {image}")
    return match.group(1)

def build_agent(project, region, registry, run_id):
    image=f"{registry}/serverless-init:{run_id}-v3"
    sha=git_sha(AGENT_REPO)
    release=json.loads((AGENT_REPO/"release.json").read_text())
    version=f"{release['current_milestone']}-dev"
    if remote_image_exists(image):
        print(f"Reusing candidate image {image}")
    else:
        run(["docker","buildx","build","--platform=linux/amd64","--push",
             "--build-arg",f"GIT_COMMIT={sha[:12]}","--build-arg",f"AGENT_VERSION={version}",
             "--build-arg",f"SERVERLESS_INIT_VERSION={version}",
             "-f",str(AGENT_REPO/"scripts/serverless-deploy/Dockerfile.serverless-init"),
             "-t",image,str(AGENT_REPO)])
    digest=run(["gcloud","artifacts","docker","images","describe",image,
                f"--project={project}",f"--format=value(image_summary.digest)"],capture=True)
    return f"{registry}/serverless-init@{digest}", sha

def build_runtime_images(registry, run_id, agent_image):
    fixture=ROOT/"fixtures"
    images={}
    def build_one(pair):
        runtime,target=pair
        # v2 init images use runtime-specific stages with explicit CMD values;
        # serverless-init needs those argv values to launch the wrapped app.
        image_suffix=f"{target}-v3" if target == "init" else target
        build_target=f"{runtime}-init" if target == "init" else "plain"
        image=f"{registry}/fixture-{runtime}-{image_suffix}:{run_id}"
        if remote_image_exists(image):
            print(f"Reusing fixture image {image}")
        else:
            run(["docker","buildx","build","--platform=linux/amd64","--push",
                 "--build-arg",f"RUNTIME={runtime}","--build-arg",f"AGENT_IMAGE={agent_image}",
                 "--target",build_target,"-t",image,"-f",str(fixture/"Dockerfile"),str(fixture)])
        return (runtime,target,image)
    pairs=[(r,t) for r in MATRIX["runtimes"] for t in ("plain","init")]
    with concurrent.futures.ThreadPoolExecutor(max_workers=3) as pool:
        for runtime,target,image in pool.map(build_one,pairs):
            images[f"{runtime}:{target}"]=image
    return images

def ensure_registry(project, region):
    repo="svls9604"
    exists=subprocess.run(["gcloud","artifacts","repositories","describe",repo,
                           f"--location={region}",f"--project={project}"],
                          stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL).returncode == 0
    if not exists:
        run(["gcloud","artifacts","repositories","create",repo,"--repository-format=docker",
             f"--location={region}",f"--project={project}"])
    run(["gcloud","auth","configure-docker",f"{region}-docker.pkg.dev","--quiet"])
    return f"{region}-docker.pkg.dev/{project}/{repo}"

def env_list(name):
    return {"DD_API_KEY":os.environ["DD_API_KEY"],"DD_SITE":"datad0g.com","DD_ENV":RUN_ENV,
            "DD_SERVICE":name,"DD_SERVERLESS_DIAGNOSTIC_INFO":"true","DD_LOG_LEVEL":"debug"}

def env_arg(values):
    return ",".join(f"{k}={v}" for k,v in values.items())

def service_json(project, name, app_image, agent_image, variant, runtime, function=False):
    labels={"svls9604":"true","svls9604-run":name.split("-")[1],"runtime":runtime,
            "workload":"function-gen2" if function else "cloud-run"}
    annotations={"autoscaling.knative.dev/minScale":"1" if variant=="busy" else "0",
                 "autoscaling.knative.dev/maxScale":"100",
                 "run.googleapis.com/container-dependencies":json.dumps({"app":["datadog-sidecar"]})}
    return {"apiVersion":"serving.knative.dev/v1","kind":"Service",
      "metadata":{"name":name,"namespace":project,"labels":labels},
      "spec":{"template":{"metadata":{"annotations":annotations},"spec":{"containerConcurrency":1 if variant=="coldstart" else 80,"timeoutSeconds":300,"containers":[
        {"name":"app","image":app_image,"ports":[{"containerPort":8080}],"env":[{"name":"DD_ENV","value":RUN_ENV}]},
        {"name":"datadog-sidecar","image":agent_image,"startupProbe":{"tcpSocket":{"port":5555},"periodSeconds":3,"failureThreshold":20},
         "env":[{"name":k,"value":v} for k,v in {**env_list(name),"DD_HEALTH_PORT":"5555","DD_APM_NON_LOCAL_TRAFFIC":"true","DD_DOGSTATSD_NON_LOCAL_TRAFFIC":"true", **({"FUNCTION_TARGET":"main"} if function else {})}.items()]}
      ]}}}}

def gcp_service_identity(project, region, service, revision=None):
    status=json.loads(run(["gcloud","run","services","describe",service,
                           f"--project={project}",f"--region={region}",
                           "--format=json(status.url,status.latestReadyRevisionName)"],capture=True))
    revision=revision or status["status"]["latestReadyRevisionName"]
    return {"endpoint":status["status"]["url"],
            "resource_id":f"//run.googleapis.com/projects/{project}/locations/{region}/revisions/{revision}",
            "parent_resource_id":f"//run.googleapis.com/projects/{project}/locations/{region}/services/{service}",
            "deployment_id":revision,
            "expected_resource_ids":[f"//run.googleapis.com/projects/{project}/locations/{region}/revisions/{revision}"]}

def gcp_function_source(runtime, name, run_dir):
    source=run_dir/f"function-{name}"; source.mkdir(exist_ok=True)
    specs={
      "python":("python312","main"), "node":("nodejs22","main"), "go":("go126","main"),
      "java":("java21","functions.Main"), "dotnet":("dotnet10","Function"),
      "ruby":("ruby33","main"), "php":("php84","main")}
    if runtime == "python":
        (source/"main.py").write_text("import functions_framework\n@functions_framework.http\ndef main(request): return 'Hello World!'\n")
        (source/"requirements.txt").write_text("functions-framework==3.*\n")
    elif runtime == "node":
        (source/"index.js").write_text("const functions=require('@google-cloud/functions-framework');functions.http('main',(q,r)=>r.send('Hello World!'));\n")
        (source/"package.json").write_text(json.dumps({"name":name,"version":"1.0.0","main":"index.js","dependencies":{"@google-cloud/functions-framework":"^3.4.0"}}))
    elif runtime == "go":
        (source/"go.mod").write_text("module example.com/svls9604\n\ngo 1.24\n\nrequire github.com/GoogleCloudPlatform/functions-framework-go v1.9.0\n")
        (source/"function.go").write_text('package function\nimport("fmt";"net/http";"github.com/GoogleCloudPlatform/functions-framework-go/functions")\nfunc init(){functions.HTTP("main",handler)}\nfunc handler(w http.ResponseWriter,r *http.Request){fmt.Fprint(w,"Hello World!")}\n')
    elif runtime == "java":
        package=source/"src/main/java/functions"; package.mkdir(parents=True,exist_ok=True)
        (source/"pom.xml").write_text('<project xmlns="http://maven.apache.org/POM/4.0.0"><modelVersion>4.0.0</modelVersion><groupId>functions</groupId><artifactId>svls9604</artifactId><version>1.0</version><properties><maven.compiler.release>21</maven.compiler.release></properties><dependencies><dependency><groupId>com.google.cloud.functions</groupId><artifactId>functions-framework-api</artifactId><version>1.1.4</version></dependency></dependencies></project>')
        (package/"Main.java").write_text('package functions;import com.google.cloud.functions.*;import java.io.*;public class Main implements HttpFunction{public void service(HttpRequest q,HttpResponse r)throws IOException{r.getWriter().write("Hello World!");}}\n')
    elif runtime == "dotnet":
        (source/"Function.csproj").write_text('<Project Sdk="Microsoft.NET.Sdk"><PropertyGroup><TargetFramework>net10.0</TargetFramework><OutputType>Exe</OutputType><ImplicitUsings>enable</ImplicitUsings></PropertyGroup><ItemGroup><PackageReference Include="Google.Cloud.Functions.Hosting" Version="2.2.1"/><PackageReference Include="Google.Cloud.Functions.Framework" Version="2.2.1"/></ItemGroup></Project>')
        (source/"Function.cs").write_text('using Google.Cloud.Functions.Framework;using Microsoft.AspNetCore.Http;public class Function:IHttpFunction{public async Task HandleAsync(HttpContext c){await c.Response.WriteAsync("Hello World!");}}\n')
    elif runtime == "ruby":
        (source/"Gemfile").write_text("source 'https://rubygems.org'\ngem 'functions_framework', '~> 1.4'\n")
        (source/"Gemfile.lock").write_text("""GEM
  remote: https://rubygems.org/
  specs:
    cloud_events (0.9.0)
    functions_framework (1.7.0)
      cloud_events (>= 0.7.0, < 2.a)
      puma (>= 4.3.0, < 9.a)
      rack (>= 2.1, < 4.a)
    nio4r (2.7.5)
    puma (8.0.2)
      nio4r (~> 2.0)
    rack (3.2.7)

PLATFORMS
  aarch64-linux
  ruby

DEPENDENCIES
  functions_framework (~> 1.4)

BUNDLED WITH
   2.5.22
""")
        (source/"app.rb").write_text("require 'functions_framework'\nFunctionsFramework.http 'main' do |_request|\n  'Hello World!'\nend\n")
    else:
        (source/"composer.json").write_text(json.dumps({"require":{"google/cloud-functions-framework":"^1.4"}}))
        (source/"index.php").write_text("<?php\nuse Psr\\Http\\Message\\ServerRequestInterface;\nfunction main(ServerRequestInterface $request): string { return 'Hello World!'; }\n")
    return source,specs[runtime][0],specs[runtime][1]

def deploy_service(project, region, r, name, images, agent_image, run_dir):
    if r["id"]=="SI-01":
        run(["gcloud","run","deploy",name,f"--project={project}",f"--region={region}",
             f"--image={images[r['runtime']+':init']}","--allow-unauthenticated","--port=8080",
             f"--min-instances={'1' if r['variant']=='busy' else '0'}","--max-instances=100",
             f"--concurrency={'1' if r['variant']=='coldstart' else '80'}",
             f"--set-env-vars={env_arg(env_list(name))}","--labels=svls9604=true"])
    elif r["id"]=="SI-02":
        body=service_json(project,name,images[r['runtime']+':plain'],agent_image,r["variant"],r["runtime"],r["id"]=="SI-04")
        path=run_dir/f"{name}.json"; path.write_text(json.dumps(body))
        run(["gcloud","run","services","replace",str(path),f"--project={project}",f"--region={region}"])
        run(["gcloud","run","services","add-iam-policy-binding",name,f"--project={project}",f"--region={region}",
             "--member=allUsers","--role=roles/run.invoker","--quiet"])
    elif r["id"]=="SI-04":
        source,runtime,entrypoint=gcp_function_source(r["runtime"],name,run_dir)
        run(["gcloud","functions","deploy",name,"--gen2",f"--project={project}",f"--region={region}",
             f"--runtime={runtime}",f"--entry-point={entrypoint}",f"--source={source}","--trigger-http",
             "--allow-unauthenticated",f"--min-instances={'1' if r['variant']=='busy' else '0'}",
             "--max-instances=100",f"--concurrency={'1' if r['variant']=='coldstart' else '80'}","--memory=1Gi","--cpu=1",
             f"--set-env-vars={env_arg({'DD_ENV':RUN_ENV,'DD_SERVICE':name})}","--quiet"])
        fn=json.loads(run(["gcloud","functions","describe",name,"--gen2",f"--project={project}",f"--region={region}",
                           "--format=json(serviceConfig.service,serviceConfig.uri)"],capture=True))
        service=fn["serviceConfig"]["service"].rsplit("/",1)[-1]
        run(["gcloud","run","services","update",service,f"--project={project}",f"--region={region}",
             "--container=datadog-sidecar",f"--image={agent_image}","--cpu=250m","--memory=512Mi",
             f"--set-env-vars={env_arg({**env_list(name),'DD_HEALTH_PORT':'5555','DD_APM_NON_LOCAL_TRAFFIC':'true','DD_DOGSTATSD_NON_LOCAL_TRAFFIC':'true','FUNCTION_TARGET':'main'})}",
             "--startup-probe=tcpSocket.port=5555,periodSeconds=3,failureThreshold=20"])
        identity=gcp_service_identity(project,region,service)
        identity["endpoint"]=fn["serviceConfig"]["uri"]
        return {"name":name,**identity}
    return {"name":name,**gcp_service_identity(project,region,name)}

def deploy_job(project, region, r, name, images):
    cmd=["gcloud","run","jobs","deploy",name,f"--project={project}",f"--region={region}",
         f"--image={images['python:init']}","--args=python,-c,print('Hello World!')",
         f"--set-env-vars={env_arg(env_list(name))}","--labels=svls9604=true"]
    run(cmd)
    return {"name":name,"endpoint":None,"resource_id":f"//run.googleapis.com/projects/{project}/locations/{region}/jobs/{name}"}

def build_compat_package(run_dir):
    binary=COMPONENTS_REPO/"target/x86_64-unknown-linux-musl/release/datadog-serverless-compat"
    npm_cache=run_dir/"npm-cache"
    npm_cache.mkdir(exist_ok=True)
    build_env={"NPM_CONFIG_CACHE":str(npm_cache)}
    # Always rebuild. Reusing an existing release binary can silently package a
    # Compat artifact from a previous commit and invalidate the run record.
    run([str(COMPONENTS_REPO/"scripts/serverless-compat-deploy/build.sh")],cwd=COMPONENTS_REPO,env=build_env)
    if not binary.exists():
        raise RuntimeError(f"Compat build did not produce {binary}")
    dest=COMPAT_JS_REPO/"bin/linux-amd64/datadog-serverless-compat"
    dest.write_bytes(binary.read_bytes()); dest.chmod(0o755)
    run(["npm","run","build"],cwd=COMPAT_JS_REPO,env=build_env)
    run(["npm","pack","--pack-destination",str(run_dir)],cwd=COMPAT_JS_REPO,env=build_env)
    return max(run_dir.glob("datadog-serverless-compat-*.tgz"),key=lambda p:p.stat().st_mtime)

def deploy_compat_gcp(project, region, name, run_dir):
    package=build_compat_package(run_dir)
    source=run_dir/"compat-gcp"; source.mkdir()
    (source/"package.tgz").write_bytes(package.read_bytes())
    (source/"package.json").write_text(json.dumps({"name":"svls9604-compat","version":"1.0.0","main":"app.js","dependencies":{"@datadog/serverless-compat":"file:package.tgz","dd-trace":"5.45.0"}}))
    (source/"app.js").write_text("require('@datadog/serverless-compat/init'); const t=require('dd-trace').init(); exports.main=t.wrap('svls9604',async(q,r)=>r.status(200).send('Hello World!'));\n")
    run(["gcloud","functions","deploy",name,"--no-gen2",f"--project={project}",f"--region={region}","--runtime=nodejs20",
         "--trigger-http","--allow-unauthenticated","--entry-point=main",f"--source={source}",
         f"--set-env-vars={env_arg(env_list(name))}","--quiet","--format=value(name)"])
    run(["gcloud","functions","add-iam-policy-binding",name,f"--project={project}",f"--region={region}",
         "--member=allUsers","--role=roles/cloudfunctions.invoker","--quiet"])
    url=run(["gcloud","functions","describe",name,f"--project={project}",f"--region={region}","--format=value(httpsTrigger.url)"],capture=True)
    return {"name":name,"endpoint":url,"resource_id":f"//cloudfunctions.googleapis.com/projects/{project}/locations/{region}/functions/{name}"}

def trigger(resource, project, region):
    if resource["id"]=="SI-03":
        run(["gcloud","run","jobs","execute",resource["name"],f"--project={project}",f"--region={region}","--wait"])
        return {"status":"executed"}
    url=resource["endpoint"]
    result=subprocess.run(["curl","-fsS","--max-time","60",url],text=True,capture_output=True)
    return {"status":"ok" if result.returncode==0 else "failed","http_body":result.stdout[:200],"error":result.stderr[:300]}

def http_request(url):
    started=time.monotonic()
    try:
        with urllib.request.urlopen(url,timeout=60) as response:
            response.read(256)
            return response.status,round((time.monotonic()-started)*1000,1),None
    except Exception as error:
        return 0,round((time.monotonic()-started)*1000,1),str(error)[:300]

def burst(resource, count=80):
    with concurrent.futures.ThreadPoolExecutor(max_workers=count) as pool:
        results=list(pool.map(http_request,[resource["endpoint"]]*count))
    latencies=sorted(x[1] for x in results)
    successes=sum(1 for status,_,_ in results if 200 <= status < 400)
    failures=[error or f"HTTP {status}" for status,_,error in results if not 200 <= status < 400]
    def percentile(p):
        return latencies[min(len(latencies)-1,round((len(latencies)-1)*p))]
    return {"attempts":count,"successes":successes,"failures":count-successes,
            "latency_ms":{"p50":percentile(.50),"p95":percentile(.95),"max":latencies[-1]},
            "sample_errors":failures[:5]}


def run_same_resource_stage(stage_id, targets, count):
    started=utc_now()
    results=[]
    for i,resource in enumerate(targets,1):
        print(f"[{stage_id} {i}/{len(targets)}] {count} requests -> {resource['name']}")
        results.append({"resource_id":resource["resource_id"],"name":resource["name"],
                        **burst(resource,count)})
    return {"id":stage_id,"scenario":"same-resource concurrent requests",
            "requests_per_resource":count,"started_at":started,"completed_at":utc_now(),
            "resources":results}


def run_distributed_stage(targets):
    started=utc_now()
    with concurrent.futures.ThreadPoolExecutor(max_workers=min(100,len(targets))) as pool:
        raw=list(pool.map(lambda resource: http_request(resource["endpoint"]),targets))
    resources=[]
    for resource,(status,elapsed,error) in zip(targets,raw):
        resources.append({"resource_id":resource["resource_id"],"name":resource["name"],
                          "http_status":status,"elapsed_ms":elapsed,"error":error})
    successes=sum(1 for result in resources if 200 <= result["http_status"] < 400)
    return {"id":"L4","scenario":"one concurrent request per distinct resource_id",
            "started_at":started,"completed_at":utc_now(),"attempts":len(resources),
            "successes":successes,"failures":len(resources)-successes,"resources":resources}


def run_sustained_stage(targets, minutes, interval_seconds):
    started=utc_now()
    deadline=time.monotonic() + minutes*60
    rounds=[]
    while True:
        round_started=utc_now()
        with concurrent.futures.ThreadPoolExecutor(max_workers=min(100,len(targets))) as pool:
            raw=list(pool.map(lambda resource: http_request(resource["endpoint"]),targets))
        successes=sum(1 for status,_,_ in raw if 200 <= status < 400)
        rounds.append({"started_at":round_started,"completed_at":utc_now(),
                       "attempts":len(raw),"successes":successes,
                       "failures":len(raw)-successes})
        remaining=deadline-time.monotonic()
        if remaining <= 0:
            break
        time.sleep(min(interval_seconds,remaining))
    return {"id":"L5","scenario":"unchanged sustained reporting window",
            "duration_minutes":minutes,"interval_seconds":interval_seconds,
            "started_at":started,"completed_at":utc_now(),"rounds":rounds}


def run_full_load_suite(targets, *, sustained_minutes, sustained_interval):
    stages=[]
    for stage_id,count in FULL_LOAD_STAGES:
        stages.append(run_same_resource_stage(stage_id,targets,count))
    stages.append(run_distributed_stage(targets))
    busy=[resource for resource in targets if resource.get("variant")=="busy"]
    stages.append(run_sustained_stage(busy or targets,sustained_minutes,sustained_interval))
    return stages

def add_expected_revision(resource, identity):
    expected=resource.setdefault("expected_resource_ids",[resource["resource_id"]])
    if identity["resource_id"] not in expected:
        expected.append(identity["resource_id"])
    resource.setdefault("observed_revisions",[]).append(identity)

def gcp_revision_stages(project, region, targets, run_id):
    target=next((r for r in targets if r["id"]=="SI-02" and r["runtime"]=="python" and r["variant"]=="busy"),None)
    if not target:
        return [{"id":"L6","scenario":"controlled revision cold-start pressure","status":"NOT MEASURED","reason":"representative SI-02 Python busy target missing"},
                {"id":"L7","scenario":"two active revisions with split traffic","status":"NOT MEASURED","reason":"representative SI-02 Python busy target missing"}]
    revision_a=target["deployment_id"]
    suffix=f"load-{run_id[-6:].lower()}"[:15]
    started=utc_now()
    run(["gcloud","run","services","update",target["name"],f"--project={project}",f"--region={region}",
         "--concurrency=1","--min-instances=0","--max-instances=100",f"--revision-suffix={suffix}",
         "--container=datadog-sidecar",f"--update-env-vars=DD_VERSION={suffix}"])
    revision_b=f"{target['name']}-{suffix}"
    identity_b=gcp_service_identity(project,region,target["name"],revision_b)
    add_expected_revision(target,identity_b)
    pressure=burst({**target,"endpoint":identity_b["endpoint"]},100)
    l6={"id":"L6","scenario":"fresh revision, concurrency=1, 100-request cold-start pressure",
        "started_at":started,"completed_at":utc_now(),"provider":"gcp","resource":target["name"],
        "parent_resource_id":target["parent_resource_id"],"revision_a":revision_a,
        "revision_b":identity_b["deployment_id"],"resource_id_b":identity_b["resource_id"],
        "provider_instance_starts":"REQUIRES LOG EVIDENCE",**pressure}
    l7_started=utc_now()
    run(["gcloud","run","services","update-traffic",target["name"],f"--project={project}",f"--region={region}",
         f"--set-tags=old={revision_a},new={identity_b['deployment_id']}",
         f"--to-revisions={revision_a}=10,{identity_b['deployment_id']}=90"])
    traffic=json.loads(run(["gcloud","run","services","describe",target["name"],f"--project={project}",f"--region={region}",
                            "--format=json(status.traffic)"],capture=True))["status"]["traffic"]
    tagged={entry.get("tag"):entry.get("url") for entry in traffic if entry.get("tag") and entry.get("url")}
    old_result=burst({**target,"endpoint":tagged["old"]},10)
    new_result=burst({**target,"endpoint":tagged["new"]},10)
    split_result=burst(target,100)
    l7={"id":"L7","scenario":"two active revisions, 10/90 service traffic plus direct tagged revision probes",
        "started_at":l7_started,"completed_at":utc_now(),"provider":"gcp","resource":target["name"],
        "parent_resource_id":target["parent_resource_id"],"revision_a":revision_a,
        "revision_b":identity_b["deployment_id"],"traffic":traffic,
        "attempts":split_result["attempts"]+old_result["attempts"]+new_result["attempts"],
        "successes":split_result["successes"]+old_result["successes"]+new_result["successes"],
        "failures":split_result["failures"]+old_result["failures"]+new_result["failures"],
        "service_traffic":split_result,"revision_a_direct":old_result,"revision_b_direct":new_result}
    return [l6,l7]

def gcp_scaling_stages(project, region, targets, run_id, maxima):
    target=next((r for r in targets if r["id"]=="SI-02" and r["runtime"]=="python" and r["variant"]=="busy"),None)
    if not target:
        return [{"id":"L8","scenario":"minimum-instance report fan-out","status":"NOT MEASURED","reason":"representative SI-02 Python busy target missing"},
                {"id":"L9","scenario":"maximum-instance scale-out ceilings","status":"NOT MEASURED","reason":"representative SI-02 Python busy target missing"}]

    def configure_revision(label, minimum, maximum):
        suffix=f"{label}-{run_id[-4:].lower()}"[:15]
        started_at=utc_now()
        run(["gcloud","run","services","update",target["name"],f"--project={project}",f"--region={region}",
             "--concurrency=1",f"--min-instances={minimum}",f"--max-instances={maximum}",
             f"--revision-suffix={suffix}","--container=datadog-sidecar",
             f"--update-env-vars=DD_VERSION={suffix}"])
        revision=f"{target['name']}-{suffix}"
        identity=gcp_service_identity(project,region,target["name"],revision)
        add_expected_revision(target,identity)
        run(["gcloud","run","services","update-traffic",target["name"],f"--project={project}",f"--region={region}",
             f"--to-revisions={identity['deployment_id']}=100"])
        identity["started_at"]=started_at
        identity["configured_at"]=utc_now()
        return identity

    minimum_cases=[]
    l8_started=utc_now()
    for minimum in (0,5,100):
        identity=configure_revision(f"min{minimum}",minimum,100)
        probe=burst({**target,"endpoint":identity["endpoint"]},1)
        minimum_cases.append({"minimum_instances":minimum,"maximum_instances":100,
                              "resource_id":identity["resource_id"],"deployment_id":identity["deployment_id"],
                              "started_at":identity["started_at"],"completed_at":utc_now(),
                              "provider_instance_starts":"REQUIRES PROVIDER/STARTUP LOG EVIDENCE",**probe})
    l8={"id":"L8","scenario":"minimum-instance report fan-out (0, 5, 100)",
        "started_at":l8_started,"completed_at":utc_now(),"provider":"gcp","resource":target["name"],
        "cases":minimum_cases,"resources":minimum_cases}

    maximum_cases=[]
    l9_started=utc_now()
    for maximum in maxima:
        identity=configure_revision(f"max{maximum}",0,maximum)
        pressure=burst({**target,"endpoint":identity["endpoint"]},100)
        maximum_cases.append({"minimum_instances":0,"maximum_instances":maximum,
                              "resource_id":identity["resource_id"],"deployment_id":identity["deployment_id"],
                              "started_at":identity["started_at"],"completed_at":utc_now(),
                              "provider_instance_starts":"REQUIRES PROVIDER/STARTUP LOG EVIDENCE",
                              "note":"100 requests validate fan-out and identity, not attainment of the configured maximum",
                              **pressure})
    l9={"id":"L9","scenario":"maximum-instance configuration boundaries with 100-request pressure",
        "started_at":l9_started,"completed_at":utc_now(),"provider":"gcp","resource":target["name"],
        "cases":maximum_cases,"resources":maximum_cases}
    return [l8,l9]

def azure_revision_identity(app_id, revision, fqdn=None):
    resource_id=f"{app_id.rstrip('/')}/revisions/{revision}".lower()
    result={"resource_id":resource_id,"parent_resource_id":app_id.lower(),
            "deployment_id":revision,"expected_resource_ids":[resource_id]}
    if fqdn:
        result["endpoint"]="https://"+fqdn
    return result

def azure_revision_stages(resource_group, targets, run_id):
    target=next((r for r in targets if r["id"]=="SI-06" and r["runtime"]=="python" and r["variant"]=="busy"),None)
    if not target:
        return [{"id":"L6","scenario":"controlled revision cold-start pressure","status":"NOT MEASURED","reason":"representative SI-06 Python busy target missing"},
                {"id":"L7","scenario":"two active revisions with split traffic","status":"NOT MEASURED","reason":"representative SI-06 Python busy target missing"}]
    revision_a=target["deployment_id"]
    suffix=f"load{run_id[-6:].lower()}"[:10]
    started=utc_now()
    revision_b=run(["az","containerapp","revision","copy","--resource-group",resource_group,"--name",target["name"],
                    "--from-revision",revision_a,"--container-name","datadog-sidecar",
                    "--set-env-vars",f"DD_VERSION={suffix}","--revision-suffix",suffix,
                    "--min-replicas","0","--max-replicas","100","--scale-rule-name","http",
                    "--scale-rule-type","http","--scale-rule-http-concurrency","1",
                    "--query","properties.latestRevisionName","-o","tsv"],capture=True)
    revisions=json.loads(run(["az","containerapp","revision","list","--resource-group",resource_group,"--name",target["name"],"-o","json"],capture=True))
    by_name={item["name"]:item for item in revisions}
    identity_b=azure_revision_identity(target["parent_resource_id"],revision_b,by_name[revision_b]["properties"].get("fqdn"))
    add_expected_revision(target,identity_b)
    pressure=burst({**target,"endpoint":identity_b["endpoint"]},100)
    l6={"id":"L6","scenario":"fresh revision, HTTP concurrency=1, 100-request cold-start pressure",
        "started_at":started,"completed_at":utc_now(),"provider":"azure","resource":target["name"],
        "parent_resource_id":target["parent_resource_id"],"revision_a":revision_a,"revision_b":revision_b,
        "resource_id_b":identity_b["resource_id"],"provider_instance_starts":"REQUIRES REVISION/LOG EVIDENCE",**pressure}
    l7_started=utc_now()
    run(["az","containerapp","ingress","traffic","set","--resource-group",resource_group,"--name",target["name"],
         "--revision-weight",f"{revision_a}=10",f"{revision_b}=90"])
    old_endpoint="https://"+by_name[revision_a]["properties"]["fqdn"]
    new_endpoint=identity_b["endpoint"]
    old_result=burst({**target,"endpoint":old_endpoint},10)
    new_result=burst({**target,"endpoint":new_endpoint},10)
    split_result=burst(target,100)
    l7={"id":"L7","scenario":"two active revisions, 10/90 service traffic plus direct revision probes",
        "started_at":l7_started,"completed_at":utc_now(),"provider":"azure","resource":target["name"],
        "parent_resource_id":target["parent_resource_id"],"revision_a":revision_a,"revision_b":revision_b,
        "traffic_weights":{revision_a:10,revision_b:90},
        "attempts":split_result["attempts"]+old_result["attempts"]+new_result["attempts"],
        "successes":split_result["successes"]+old_result["successes"]+new_result["successes"],
        "failures":split_result["failures"]+old_result["failures"]+new_result["failures"],
        "service_traffic":split_result,"revision_a_direct":old_result,"revision_b_direct":new_result}
    return [l6,l7]

def azure_scaling_stages(resource_group, targets, run_id, maxima):
    target=next((r for r in targets if r["id"]=="SI-06" and r["runtime"]=="python" and r["variant"]=="busy"),None)
    if not target:
        return [{"id":"L8","scenario":"minimum-replica report fan-out","status":"NOT MEASURED","reason":"representative SI-06 Python busy target missing"},
                {"id":"L9","scenario":"maximum-replica scale-out ceilings","status":"NOT MEASURED","reason":"representative SI-06 Python busy target missing"}]

    def copy_revision(label, minimum, maximum):
        suffix=f"{label}{run_id[-4:].lower()}"[:10]
        started_at=utc_now()
        revision=run(["az","containerapp","revision","copy","--resource-group",resource_group,"--name",target["name"],
                      "--from-revision",target["deployment_id"],"--container-name","datadog-sidecar",
                      "--set-env-vars",f"DD_VERSION={suffix}","--revision-suffix",suffix,
                      "--min-replicas",str(minimum),"--max-replicas",str(maximum),"--scale-rule-name","http",
                      "--scale-rule-type","http","--scale-rule-http-concurrency","1",
                      "--query","properties.latestRevisionName","-o","tsv"],capture=True)
        revisions=json.loads(run(["az","containerapp","revision","list","--resource-group",resource_group,"--name",target["name"],"-o","json"],capture=True))
        observed=next(item for item in revisions if item["name"]==revision)
        identity=azure_revision_identity(target["parent_resource_id"],revision,observed["properties"].get("fqdn"))
        identity["started_at"]=started_at
        identity["configured_at"]=utc_now()
        add_expected_revision(target,identity)
        return identity

    minimum_cases=[]
    l8_started=utc_now()
    for minimum in (0,5,100):
        identity=copy_revision(f"min{minimum}",minimum,100)
        probe=burst({**target,"endpoint":identity["endpoint"]},1)
        minimum_cases.append({"minimum_instances":minimum,"maximum_instances":100,
                              "resource_id":identity["resource_id"],"deployment_id":identity["deployment_id"],
                              "started_at":identity["started_at"],"completed_at":utc_now(),
                              "provider_instance_starts":"REQUIRES PROVIDER/STARTUP LOG EVIDENCE",**probe})
    l8={"id":"L8","scenario":"minimum-replica report fan-out (0, 5, 100)",
        "started_at":l8_started,"completed_at":utc_now(),"provider":"azure","resource":target["name"],
        "cases":minimum_cases,"resources":minimum_cases}

    maximum_cases=[]
    l9_started=utc_now()
    for maximum in maxima:
        identity=copy_revision(f"max{maximum}",0,maximum)
        pressure=burst({**target,"endpoint":identity["endpoint"]},100)
        maximum_cases.append({"minimum_instances":0,"maximum_instances":maximum,
                              "resource_id":identity["resource_id"],"deployment_id":identity["deployment_id"],
                              "started_at":identity["started_at"],"completed_at":utc_now(),
                              "provider_instance_starts":"REQUIRES PROVIDER/STARTUP LOG EVIDENCE",
                              "note":"100 requests validate fan-out and identity, not attainment of the configured maximum",
                              **pressure})
    l9={"id":"L9","scenario":"maximum-replica configuration boundaries with 100-request pressure",
        "started_at":l9_started,"completed_at":utc_now(),"provider":"azure","resource":target["name"],
        "cases":maximum_cases,"resources":maximum_cases}
    return [l8,l9]

def azure_params(path, values):
    body={"$schema":"https://schema.management.azure.com/schemas/2019-04-01/deploymentParameters.json#",
          "contentVersion":"1.0.0.0","parameters":{k:{"value":v} for k,v in values.items()}}
    path.write_text(json.dumps(body,indent=2)); path.chmod(0o600)

def azure_deploy(template, resource_group, deployment, values, run_dir):
    params=run_dir/f"{deployment}-parameters.json"
    azure_params(params,values)
    output=run(["az","deployment","group","create","--resource-group",resource_group,
                "--name",deployment,"--template-file",str(ROOT/"azure"/template),
                "--parameters",f"@{params}","--query","properties.outputs","-o","json"],capture=True)
    return json.loads(output)

def build_agent_azure(registry, run_id):
    image=f"{registry}/svls9604/serverless-init:{run_id}-v3"
    sha=git_sha(AGENT_REPO)
    release=json.loads((AGENT_REPO/"release.json").read_text())
    version=f"{release['current_milestone']}-dev"
    if not remote_image_exists(image):
        run(["docker","buildx","build","--platform=linux/amd64","--push",
             "--build-arg",f"GIT_COMMIT={sha[:12]}","--build-arg",f"AGENT_VERSION={version}",
             "--build-arg",f"SERVERLESS_INIT_VERSION={version}",
             "-f",str(AGENT_REPO/"scripts/serverless-deploy/Dockerfile.serverless-init"),
             "-t",image,str(AGENT_REPO)])
    digest=image_digest(image)
    return f"{registry}/svls9604/serverless-init@{digest}",sha

def source_zip(runtime, name, run_dir):
    source=run_dir/f"source-{name}"; source.mkdir(exist_ok=True)
    if runtime == "node":
        (source/"package.json").write_text(json.dumps({"name":name,"version":"1.0.0","scripts":{"start":"node app.js"}}))
        (source/"app.js").write_text("const http=require('http');http.createServer((q,r)=>r.end('Hello World!')).listen(process.env.PORT||8080,'0.0.0.0');\n")
    elif runtime == "python":
        (source/"requirements.txt").write_text("")
        (source/"app.py").write_text("from http.server import BaseHTTPRequestHandler,HTTPServer\nclass H(BaseHTTPRequestHandler):\n def do_GET(self): self.send_response(200);self.end_headers();self.wfile.write(b'Hello World!')\nHTTPServer(('0.0.0.0',int(__import__('os').environ.get('PORT','8000'))),H).serve_forever()\n")
    elif runtime == "dotnet":
        (source/"app.csproj").write_text('<Project Sdk="Microsoft.NET.Sdk.Web"><PropertyGroup><TargetFramework>net8.0</TargetFramework><ImplicitUsings>enable</ImplicitUsings></PropertyGroup></Project>')
        (source/"Program.cs").write_text('var b=WebApplication.CreateBuilder(args);var a=b.Build();a.MapGet("/",()=>"Hello World!");a.Run();\n')
    archive=run_dir/f"{name}.zip"
    with zipfile.ZipFile(archive,"w",zipfile.ZIP_DEFLATED) as z:
        for child in source.iterdir(): z.write(child,child.name)
    return archive

def compat_azure_zip(name, run_dir):
    package=build_compat_package(run_dir)
    source=run_dir/f"source-{name}"; source.mkdir(exist_ok=True)
    (source/"package.tgz").write_bytes(package.read_bytes())
    (source/"package.json").write_text(json.dumps({"name":name,"version":"1.0.0","main":"index.js","dependencies":{"@azure/functions":"4.7.2","@datadog/serverless-compat":"file:package.tgz","dd-trace":"5.45.0"}}))
    (source/"host.json").write_text(json.dumps({"version":"2.0"}))
    (source/"index.js").write_text("require('@datadog/serverless-compat/init');const{app}=require('@azure/functions');app.http('main',{methods:['GET'],authLevel:'anonymous',handler:async()=>({body:'Hello World!'})});\n")
    archive=run_dir/f"{name}.zip"
    with zipfile.ZipFile(archive,"w",zipfile.ZIP_DEFLATED) as z:
        for child in source.iterdir(): z.write(child,child.name)
    return archive

def deploy_azure_resource(args, r, name, images, agent_image, acr, run_id, run_dir):
    common={"name":name,"agentImage":agent_image,"registryServer":acr["server"],
            "registryUsername":acr["username"],"registryPassword":acr["password"],
            "ddApiKey":os.environ["DD_API_KEY"],"runtime":r["runtime"],"runId":run_id}
    deployment=f"d-{name}"[:64]
    if r["id"] in ("SI-05","SI-06"):
        values={**common,"appEnvId":args.azure_containerapp_env_id,
                "appImage":images[f"{r['runtime']}:{'init' if r['id']=='SI-05' else 'plain'}"],
                "sidecar":r["id"]=="SI-06","minReplicas":1 if r["variant"]=="busy" else 0}
        out=azure_deploy("container-app.bicep",args.azure_containerapp_resource_group,deployment,values,run_dir)
        identity=azure_revision_identity(out["resourceId"]["value"],out["latestRevisionName"]["value"],out["fqdn"]["value"])
        return {"name":name,**identity}
    if r["id"] in ("SI-07","SI-08"):
        plan=args.azure_container_plan_id if r["id"]=="SI-07" else args.azure_sidecar_plan_id
        values={**common,"servicePlanId":plan,
                "appImage":images[f"{r['runtime']}:{'init' if r['id']=='SI-07' else 'plain'}"],
                "sidecar":r["id"]=="SI-08","alwaysOn":r["variant"]=="busy"}
        out=azure_deploy("web-app-container.bicep",args.azure_resource_group,deployment,values,run_dir)
        return {"name":name,"endpoint":"https://"+out["hostname"]["value"],"resource_id":out["resourceId"]["value"]}
    if r["id"]=="SI-09":
        values={**common,"servicePlanId":args.azure_code_plan_id,"alwaysOn":r["variant"]=="busy"}
        out=azure_deploy("web-app-code.bicep",args.azure_resource_group,deployment,values,run_dir)
        package=source_zip(r["runtime"],name,run_dir)
        try:
            run(["az","webapp","deploy","--resource-group",args.azure_resource_group,"--name",name,
                 "--src-path",str(package),"--type","zip","--clean","true","--async","true"])
        except RuntimeError as e:
            print(f"Warning: az webapp deploy exited non-zero ({e}); verifying via health check",flush=True)
        endpoint="https://"+out["hostname"]["value"]
        print(f"Waiting for {name} to become healthy...",flush=True)
        for _ in range(72):
            try:
                with urllib.request.urlopen(endpoint,timeout=10) as resp:
                    if resp.status < 500:
                        print(f"{name} healthy (HTTP {resp.status})",flush=True)
                        break
            except Exception:
                pass
            time.sleep(10)
        return {"name":name,"endpoint":endpoint,"resource_id":out["resourceId"]["value"]}
    storage=("sv"+hashlib.sha256(name.encode()).hexdigest()[:20])[:24]
    values={"name":name,"storageName":storage,"ddApiKey":os.environ["DD_API_KEY"],"runId":run_id}
    out=azure_deploy("function.bicep",args.azure_function_resource_group,deployment,values,run_dir)
    package=compat_azure_zip(name,run_dir)
    run(["az","functionapp","deployment","source","config-zip","--resource-group",args.azure_function_resource_group,
         "--name",name,"--src",str(package),"--build-remote","true"])
    return {"name":name,"endpoint":"https://"+out["hostname"]["value"]+"/api/main","resource_id":out["resourceId"]["value"]}

def baseline_stage(resources, started_at):
    successes=sum(1 for resource in resources
                  if resource.get("baseline",{}).get("status") in ("ok","executed"))
    return {"id":"L0","scenario":"one baseline request or execution per deployed resource",
            "started_at":started_at,"completed_at":utc_now(),"attempts":len(resources),
            "successes":successes,"failures":len(resources)-successes}


def run_azure(args, resources, run_id, run_dir):
    run(["az","acr","login","--name",args.azure_acr])
    credential=json.loads(run(["az","acr","credential","show","--name",args.azure_acr,"-o","json"],capture=True))
    acr={"server":args.azure_registry,"username":credential["username"],"password":credential["passwords"][0]["value"]}
    os.environ["SVLS9604_ACR_PASSWORD"]=acr["password"]
    agent_image,agent_sha=build_agent_azure(args.azure_registry,run_id)
    images=build_runtime_images(f"{args.azure_registry}/svls9604",run_id,agent_image)
    manifest_path=run_dir/"run-manifest.json"
    existing={}
    if manifest_path.exists():
        previous=json.loads(manifest_path.read_text())
        if previous.get("profile") in ("azure","azure-sanity") and previous.get("agent_image")==agent_image:
            candidates=[item for item in previous.get("resources",[])
                        if item.get("agent_image")==agent_image and item.get("endpoint")]
            with concurrent.futures.ThreadPoolExecutor(max_workers=min(20,len(candidates) or 1)) as pool:
                checks=list(pool.map(lambda item: http_request(item["endpoint"]),candidates))
            for item,(status,_,error) in zip(candidates,checks):
                if 200 <= status < 500:
                    existing[item["name"]]=item
                else:
                    print(f"Azure resume will redeploy unhealthy {item['name']} (HTTP {status}: {error})")
    deployed=[]
    for i,r in enumerate(resources,1):
        name=name_for(run_id,r)
        if name in existing:
            print(f"[{i}/{len(resources)}] reusing deployed {r['id']} {r['runtime']} {r['variant']} as {name}")
            deployed.append(existing[name])
            continue
        print(f"[{i}/{len(resources)}] deploying {r['id']} {r['runtime']} {r['variant']} as {name}")
        try:
            observed=deploy_azure_resource(args,r,name,images,agent_image,acr,run_id,run_dir)
        except Exception as exc:
            print(f"  WARNING: deploy failed for {name}: {exc}", flush=True)
            continue
        deployed.append({**r,**observed,"agent_image":agent_image})
        write_manifest(manifest_path,run_id=run_id,profile=args.profile,agent_sha=agent_sha,agent_image=agent_image,resources=deployed)
    baseline_started=utc_now()
    for resource in deployed:
        result=subprocess.run(["curl","-fsS","--max-time","60",resource["endpoint"]],text=True,capture_output=True)
        resource["baseline"]={"status":"ok" if result.returncode==0 else "failed","http_body":result.stdout[:200],"error":result.stderr[:300]}
    stages=[baseline_stage(deployed,baseline_started)]
    if args.suite == "full" and not args.skip_burst:
        targets=[r for r in deployed if r["id"].startswith("SI-") and r.get("endpoint")]
        stages.extend(run_full_load_suite(targets,sustained_minutes=args.sustained_minutes,
                                          sustained_interval=args.sustained_interval))
        stages.extend(azure_revision_stages(args.azure_containerapp_resource_group,targets,run_id))
        if args.scaling_matrix:
            stages.extend(azure_scaling_stages(args.azure_containerapp_resource_group,targets,run_id,args.scaling_maxima))
        compat_targets=[r for r in deployed if r["id"].startswith("SC-") and r.get("endpoint")]
        for stage_id,count in [("SC-L1",10),("SC-L2",50),("SC-L3",100)]:
            stages.append(run_same_resource_stage(stage_id,compat_targets,count))
    write_manifest(manifest_path,run_id=run_id,profile=args.profile,agent_sha=agent_sha,
                   agent_image=agent_image,resources=deployed,stages=stages,suite=args.suite)
    return deployed

def run_gcp(args, resources, run_id, run_dir):
    project=args.project or run(["gcloud","config","get-value","project"],capture=True)
    region=args.region
    registry=ensure_registry(project,region)
    agent_image,agent_sha=build_agent(project,region,registry,run_id)
    images=build_runtime_images(registry,run_id,agent_image)
    manifest_path=run_dir/"run-manifest.json"
    existing={}
    if manifest_path.exists():
        previous=json.loads(manifest_path.read_text())
        # Revision-scoped identity is required for Cloud Run and Gen2 functions.
        # Never resume a manifest produced by the earlier service-scoped runner.
        valid=[]
        for x in previous.get("resources",[]):
            if x.get("agent_image") != agent_image:
                continue
            if x["id"] not in ("SI-01","SI-02","SI-04"):
                valid.append(x)
            elif "/revisions/" in x.get("resource_id","") and x.get("parent_resource_id"):
                valid.append(x)
        existing={x["name"]:x for x in valid}
    deployed=[]
    for i,r in enumerate(resources,1):
        name=name_for(run_id,r)
        if name in existing:
            print(f"[{i}/{len(resources)}] reusing deployed {r['id']} {r['runtime']} {r['variant']} as {name}")
            deployed.append(existing[name]); continue
        print(f"[{i}/{len(resources)}] deploying {r['id']} {r['runtime']} {r['variant']} as {name}")
        try:
            if r["id"]=="SI-03": observed=deploy_job(project,region,r,name,images)
            elif r["id"]=="SC-02": observed=deploy_compat_gcp(project,region,name,run_dir)
            else: observed=deploy_service(project,region,r,name,images,agent_image,run_dir)
        except Exception as exc:
            print(f"  WARNING: deploy failed for {name}: {exc}", flush=True)
            continue
        deployed.append({**r,**observed,"agent_image":agent_image})
        write_manifest(manifest_path,run_id=run_id,profile="gcp",project=project,region=region,agent_sha=agent_sha,agent_image=agent_image,resources=deployed)
    baseline_started=utc_now()
    for resource in deployed:
        resource["baseline"]=trigger(resource,project,region)
    stages=[baseline_stage(deployed,baseline_started)]
    if args.suite == "full" and not args.skip_burst:
        targets=[r for r in deployed if r["id"].startswith("SI-") and r.get("endpoint")]
        stages.extend(run_full_load_suite(targets,sustained_minutes=args.sustained_minutes,
                                          sustained_interval=args.sustained_interval))
        stages.extend(gcp_revision_stages(project,region,targets,run_id))
        if args.scaling_matrix:
            stages.extend(gcp_scaling_stages(project,region,targets,run_id,args.scaling_maxima))
        compat_targets=[r for r in deployed if r["id"].startswith("SC-") and r.get("endpoint")]
        for stage_id,count in [("SC-L1",10),("SC-L2",50),("SC-L3",100)]:
            stages.append(run_same_resource_stage(stage_id,compat_targets,count))
    write_manifest(manifest_path,run_id=run_id,profile="gcp",project=project,region=region,
                   agent_sha=agent_sha,agent_image=agent_image,resources=deployed,
                   stages=stages,suite=args.suite)
    return deployed

def main():
    global RUN_ENV, RUN_STARTED_AT
    parser=argparse.ArgumentParser()
    parser.add_argument("--profile",choices=["gcp","azure","gcp-sanity","azure-sanity"],required=True)
    parser.add_argument("--project",default=os.environ.get("GCP_PROJECT","datadog-serverless-gcp-demo"))
    parser.add_argument("--region",default=os.environ.get("GCP_REGION","us-central1"))
    parser.add_argument("--azure-resource-group",default=os.environ.get("AZURE_RESOURCE_GROUP","dd-serverless-test-aas"))
    parser.add_argument("--azure-containerapp-resource-group",default=os.environ.get("AZURE_CONTAINERAPP_RESOURCE_GROUP","dd-serverless-test-aca"))
    parser.add_argument("--azure-function-resource-group",default=os.environ.get("AZURE_FUNCTION_RESOURCE_GROUP","dd-serverless-test-aca"))
    parser.add_argument("--azure-acr",default=os.environ.get("AZURE_ACR","ddsvlstestaca"))
    parser.add_argument("--azure-registry",default=os.environ.get("AZURE_REGISTRY","ddsvlstestaca.azurecr.io"))
    parser.add_argument("--azure-containerapp-env-id",default=os.environ.get("AZURE_CONTAINERAPP_ENV_ID","/subscriptions/1dd25961-a5c7-45bf-a5ba-c1475d365cc7/resourceGroups/dd-serverless-test-aca/providers/Microsoft.App/managedEnvironments/dd-serverless-env"))
    parser.add_argument("--azure-container-plan-id",default=os.environ.get("AZURE_CONTAINER_PLAN_ID","/subscriptions/1dd25961-a5c7-45bf-a5ba-c1475d365cc7/resourceGroups/dd-serverless-test-aas/providers/Microsoft.Web/serverfarms/dd-test-plan-container"))
    parser.add_argument("--azure-sidecar-plan-id",default=os.environ.get("AZURE_SIDECAR_PLAN_ID","/subscriptions/1dd25961-a5c7-45bf-a5ba-c1475d365cc7/resourceGroups/dd-serverless-test-aas/providers/Microsoft.Web/serverfarms/dd-test-plan-sidecar"))
    parser.add_argument("--azure-code-plan-id",default=os.environ.get("AZURE_CODE_PLAN_ID","/subscriptions/1dd25961-a5c7-45bf-a5ba-c1475d365cc7/resourceGroups/dd-serverless-test-aas/providers/Microsoft.Web/serverfarms/dd-test-plan-linux-code"))
    parser.add_argument("--run-id")
    parser.add_argument("--suite",choices=["baseline","full"],default="full",
                        help="full runs L0-L5 plus revision cold-start pressure (L6) and split traffic (L7)")
    parser.add_argument("--sustained-minutes",type=float,default=15,
                        help="duration of the L5 unchanged-report window")
    parser.add_argument("--sustained-interval",type=float,default=60,
                        help="seconds between L5 trigger rounds")
    parser.add_argument("--scaling-matrix",action="store_true",
                        help="add L8/L9 minimum and maximum instance/replica boundary cases")
    parser.add_argument("--scaling-maxima",default="100,1000,4000",
                        help="comma-separated L9 maximum instance/replica settings")
    parser.add_argument("--plan",action="store_true")
    parser.add_argument("--yes",action="store_true")
    parser.add_argument("--skip-burst",action="store_true",help="compatibility alias for --suite baseline")
    args=parser.parse_args()
    args.scaling_maxima=tuple(int(value) for value in args.scaling_maxima.split(",") if value)
    if args.skip_burst:
        args.suite="baseline"
    resources=expand(args.profile)
    run_id=args.run_id or dt.datetime.now(dt.timezone.utc).strftime("%m%d%H%M")+secrets.token_hex(2)
    RUN_ENV=f"svls9604-{run_id.lower()}"
    RUN_STARTED_AT=dt.datetime.now(dt.timezone.utc).isoformat()
    print(f"Profile: {args.profile}\nRun ID: {run_id}\nSuite: {args.suite}\nExpected resources: {len(resources)}")
    for r in resources:
        print(f"  {r['id']:5} {r['workload_type']:28} {r.get('deployment_model','compat'):12} {r['runtime']:7} {r['variant']}")
    if args.plan:
        return
    preflight(args)
    if not args.yes:
        if input(f"Create {len(resources)} resources? [y/N] ").strip().lower() != "y":
            raise SystemExit("cancelled")
    run_dir=pathlib.Path(os.environ.get("RESULTS_DIR",f"/tmp/svls9604-{run_id}")); run_dir.mkdir(parents=True,exist_ok=True)
    run_dir.chmod(0o700)
    failure_exit=0
    if args.profile in ("gcp", "gcp-sanity"):
        deployed=run_gcp(args,resources,run_id,run_dir)
        failed=[r for r in deployed if r.get("baseline",{}).get("status") not in ("ok","executed")]
        print(f"GCP profile deployed {len(deployed)}/{len(resources)}; baseline failures={len(failed)}")
        print(f"Evidence: {run_dir}")
        if failed: failure_exit=2
    else:
        deployed=run_azure(args,resources,run_id,run_dir)
        failed=[r for r in deployed if r.get("baseline",{}).get("status") != "ok"]
        print(f"Azure profile deployed {len(deployed)}/{len(resources)}; baseline failures={len(failed)}")
        print(f"Evidence: {run_dir}")
        if failed: failure_exit=2

    manifest_path=run_dir/"run-manifest.json"
    manifest=json.loads(manifest_path.read_text())
    manifest["dd_env"]=RUN_ENV
    manifest["started_at"]=RUN_STARTED_AT
    manifest["completed_at"]=utc_now()
    manifest["candidate_commits"]={
        "serverless_components":git_sha(COMPONENTS_REPO),
        "datadog_agent":git_sha(AGENT_REPO),
        "datadog_serverless_compat_js":git_sha(COMPAT_JS_REPO),
    }
    manifest_path.write_text(json.dumps(manifest,indent=2))
    run([sys.executable,str(ROOT/"report.py"),"--manifest",str(manifest_path)])
    for stage in manifest.get("load_stages",[]):
        _,_,stage_failures=aggregate_stage(stage)
        if stage_failures:
            failure_exit=2
    if failure_exit:
        raise SystemExit(failure_exit)

if __name__=="__main__":
    main()
