#!/usr/bin/env bash
# Dispatch and observe one repository-local GitHub release workflow with a
# repository-scoped dd-octo-sts token. Tokens are never shared across calls.

set -euo pipefail

readonly API_VERSION="2026-03-10"
readonly API_ROOT="https://api.github.com"
readonly POLL_INTERVAL_SECONDS="30"
readonly TOKEN_REFRESH_SECONDS="2700"
readonly RUN_TIMEOUT_SECONDS="14400"

token=""
token_issued_at=0
run_id=""

usage() {
    cat >&2 <<'USAGE'
usage:
  github-release-control.sh create-tag --repo OWNER/REPO --policy POLICY --tag TAG --commit SHA
  github-release-control.sh ensure-branch --repo OWNER/REPO --policy POLICY --branch BRANCH --commit SHA
  github-release-control.sh require-published-release --repo OWNER/REPO --policy POLICY --tag TAG --commit SHA
  github-release-control.sh resolve-ref --repo OWNER/REPO --policy POLICY --ref REF
  github-release-control.sh record-go-results --repo OWNER/REPO --policy POLICY --tag TAG \
      --branch BRANCH --result-file PATH
  github-release-control.sh dispatch --repo OWNER/REPO --policy POLICY --workflow FILE --ref REF \
      --commit SHA --operation-id ID [--input KEY=VALUE ...] [--result-file PATH]
USAGE
    exit 2
}

require_value() {
    local name="$1" value="${2:-}"
    if [[ -z "$value" ]]; then
        echo "missing required value: $name" >&2
        exit 2
    fi
}

mint_token() {
    if [[ -n "$token" ]]; then
        dd-octo-sts revoke -t "$token" >/dev/null 2>&1 || true
    fi
    { set +x; } 2>/dev/null
    token="$(dd-octo-sts token --scope "$repo" --policy "$policy")"
    if [[ -z "$token" ]]; then
        echo "dd-octo-sts returned an empty token for $repo" >&2
        exit 1
    fi
    token_issued_at="$(date +%s)"
}

refresh_token_if_needed() {
    local now
    now="$(date +%s)"
    if (( now - token_issued_at >= TOKEN_REFRESH_SECONDS )); then
        mint_token
    fi
}

cleanup() {
    local status=$?
    if [[ -n "$token" ]]; then
        dd-octo-sts revoke -t "$token" >/dev/null 2>&1 || true
        token=""
    fi
    exit "$status"
}
trap cleanup EXIT INT TERM

github_api() {
    local method="$1" path="$2"
    shift 2
    refresh_token_if_needed
    curl --fail-with-body --silent --show-error \
        --request "$method" \
        --header "Accept: application/vnd.github+json" \
        --header "Authorization: Bearer $token" \
        --header "X-GitHub-Api-Version: $API_VERSION" \
        "$@" "$API_ROOT$path"
}

resolve_commit() {
    local ref="$1" encoded_ref response resolved
    encoded_ref="$(jq -rn --arg value "$ref" '$value|@uri')"
    response="$(github_api GET "/repos/$repo/commits/$encoded_ref")"
    resolved="$(jq -er '.sha' <<<"$response")"
    if [[ ! "$resolved" =~ ^[0-9a-f]{40}$ ]]; then
        echo "GitHub returned an invalid commit SHA for $repo:$ref" >&2
        exit 1
    fi
    printf '%s\n' "$resolved"
}

create_or_verify_tag() {
    local encoded_tag response status existing
    encoded_tag="$(jq -rn --arg value "$tag" '$value|@uri')"

    set +e
    response="$(github_api GET "/repos/$repo/git/ref/tags/$encoded_tag" 2>&1)"
    status=$?
    set -e
    if (( status == 0 )); then
        existing="$(resolve_commit "$tag")"
        if [[ "$existing" != "$commit" ]]; then
            echo "refusing to move $repo tag $tag from $existing to $commit" >&2
            exit 1
        fi
        echo "Reusing $repo tag $tag at $commit"
        return
    fi
    if ! grep -q '"status": "404"\|Not Found' <<<"$response"; then
        printf '%s\n' "$response" >&2
        exit "$status"
    fi

    if [[ "$(resolve_commit "$commit")" != "$commit" ]]; then
        echo "commit $commit does not resolve exactly in $repo" >&2
        exit 1
    fi

    local payload
    payload="$(jq -cn --arg ref "refs/tags/$tag" --arg sha "$commit" '{ref:$ref, sha:$sha}')"
    set +e
    response="$(github_api POST "/repos/$repo/git/refs" --header 'Content-Type: application/json' --data "$payload" 2>&1)"
    status=$?
    set -e
    if (( status != 0 )); then
        # The POST may have succeeded even if its response was lost. Reconcile
        # the immutable tag before deciding that the operation failed.
        existing="$(resolve_commit "$tag" 2>/dev/null || true)"
        if [[ "$existing" != "$commit" ]]; then
            printf '%s\n' "$response" >&2
            exit "$status"
        fi
    fi
    echo "Created $repo tag $tag at $commit"
}

ensure_branch() {
    local encoded_branch response status existing payload
    encoded_branch="$(jq -rn --arg value "$branch" '$value|@uri')"
    set +e
    response="$(github_api GET "/repos/$repo/git/ref/heads/$encoded_branch" 2>&1)"
    status=$?
    set -e
    if (( status == 0 )); then
        resolve_commit "$branch"
        return
    fi
    if ! grep -q '"status": "404"\|Not Found' <<<"$response"; then
        printf '%s\n' "$response" >&2
        exit "$status"
    fi
    if [[ "$(resolve_commit "$commit")" != "$commit" ]]; then
        echo "commit $commit does not resolve exactly in $repo" >&2
        exit 1
    fi
    payload="$(jq -cn --arg ref "refs/heads/$branch" --arg sha "$commit" '{ref:$ref, sha:$sha}')"
    set +e
    response="$(github_api POST "/repos/$repo/git/refs" --header 'Content-Type: application/json' --data "$payload" 2>&1)"
    status=$?
    set -e
    existing="$(resolve_commit "$branch" 2>/dev/null || true)"
    if (( status != 0 )) && [[ "$existing" != "$commit" ]]; then
        printf '%s\n' "$response" >&2
        exit "$status"
    fi
    echo "Created $repo branch $branch at $commit" >&2
    printf '%s\n' "${existing:-$commit}"
}

record_go_results() {
    local encoded_tag release_response pulls_response pull_count release_url pull_url
    encoded_tag="$(jq -rn --arg value "$tag" '$value|@uri')"
    release_response="$(github_api GET "/repos/$repo/releases/tags/$encoded_tag")"
    release_url="$(jq -er '.html_url' <<<"$release_response")"
    pulls_response="$(github_api GET "/repos/$repo/pulls?state=all&head=DataDog%3A$(jq -rn --arg value "$branch" '$value|@uri')&per_page=100")"
    pull_count="$(jq -er 'length' <<<"$pulls_response")"
    if [[ "$pull_count" != "1" ]]; then
        echo "expected one reconciliation PR for $repo:$branch, found $pull_count" >&2
        exit 1
    fi
    pull_url="$(jq -er '.[0].html_url' <<<"$pulls_response")"
    printf 'Go Release: %s\nGo reconciliation PR: %s\n' "$release_url" "$pull_url"
    printf 'GITHUB_RELEASE_URL=%s\nGITHUB_RECONCILIATION_PR_URL=%s\n' "$release_url" "$pull_url" >>"$result_file"
}

require_published_release() {
    local encoded_tag response resolved asset_count
    resolved="$(resolve_commit "$tag")"
    if [[ "$resolved" != "$commit" ]]; then
        echo "$repo tag $tag points to $resolved instead of $commit" >&2
        exit 1
    fi
    encoded_tag="$(jq -rn --arg value "$tag" '$value|@uri')"
    response="$(github_api GET "/repos/$repo/releases/tags/$encoded_tag")"
    asset_count="$(jq -er '[.assets[] | select(.name == "datadog-serverless-compat.zip")] | length' <<<"$response")"
    if [[ "$(jq -er '.tag_name' <<<"$response")" != "$tag" ]] ||
       [[ "$(jq -er '.draft' <<<"$response")" != "false" ]] ||
       [[ "$(jq -er '.prerelease' <<<"$response")" != "false" ]] ||
       [[ "$asset_count" != "1" ]]; then
        echo "$repo release $tag is not a published non-prerelease core release with exactly one expected asset" >&2
        exit 1
    fi
    echo "Published core release: $(jq -er '.html_url' <<<"$response")"
}

dispatch_and_wait() {
    local inputs_json='{}' pair key value payload response path event head_sha conclusion started_at now
    for pair in "${workflow_inputs[@]}"; do
        key="${pair%%=*}"
        value="${pair#*=}"
        if [[ -z "$key" || "$pair" != *=* ]]; then
            echo "invalid workflow input: $pair" >&2
            exit 2
        fi
        inputs_json="$(jq -cn --argjson inputs "$inputs_json" --arg key "$key" --arg value "$value" '$inputs + {($key):$value}')"
    done
    payload="$(jq -cn --arg ref "$ref" --argjson inputs "$inputs_json" '{ref:$ref, inputs:$inputs, return_run_details:true}')"

    response="$(github_api POST "/repos/$repo/actions/workflows/$workflow/dispatches" \
        --header 'Content-Type: application/json' --data "$payload")"
    run_id="$(jq -er '.workflow_run_id' <<<"$response")"
    local run_url
    run_url="$(jq -er '.workflow_run_html_url // .html_url' <<<"$response")"
    echo "GitHub run: $run_url"
    if [[ -n "$result_file" ]]; then
        mkdir -p "$(dirname "$result_file")"
        printf 'GITHUB_RUN_ID=%s\nGITHUB_RUN_URL=%s\n' "$run_id" "$run_url" >"$result_file"
    fi

    started_at="$(date +%s)"
    while :; do
        response="$(github_api GET "/repos/$repo/actions/runs/$run_id")"
        path="$(jq -er '.path' <<<"$response")"
        path="${path%%@*}"
        event="$(jq -er '.event' <<<"$response")"
        head_sha="$(jq -er '.head_sha' <<<"$response")"
        if [[ "$path" != ".github/workflows/$workflow" || "$event" != "workflow_dispatch" || "$head_sha" != "$commit" ]]; then
            echo "run $run_id does not match workflow/event/commit: $path $event $head_sha" >&2
            exit 1
        fi

        if [[ "$(jq -er '.status' <<<"$response")" == "completed" ]]; then
            conclusion="$(jq -er '.conclusion' <<<"$response")"
            if [[ "$conclusion" != "success" ]]; then
                echo "GitHub run $run_url completed with $conclusion" >&2
                exit 1
            fi
            echo "GitHub run succeeded: $run_url"
            return
        fi

        now="$(date +%s)"
        if (( now - started_at >= RUN_TIMEOUT_SECONDS )); then
            echo "timed out waiting for GitHub run $run_url; the run was not cancelled" >&2
            exit 1
        fi
        sleep "$POLL_INTERVAL_SECONDS"
    done
}

command="${1:-}"
[[ -n "$command" ]] || usage
shift
repo=""
policy=""
tag=""
branch=""
commit=""
workflow=""
ref=""
operation_id=""
result_file=""
workflow_inputs=()
while (( $# )); do
    case "$1" in
        --repo) repo="${2:-}"; shift 2 ;;
        --policy) policy="${2:-}"; shift 2 ;;
        --tag) tag="${2:-}"; shift 2 ;;
        --branch) branch="${2:-}"; shift 2 ;;
        --commit) commit="${2:-}"; shift 2 ;;
        --workflow) workflow="${2:-}"; shift 2 ;;
        --ref) ref="${2:-}"; shift 2 ;;
        --operation-id) operation_id="${2:-}"; shift 2 ;;
        --input) workflow_inputs+=("${2:-}"); shift 2 ;;
        --result-file) result_file="${2:-}"; shift 2 ;;
        *) usage ;;
    esac
done

require_value repo "$repo"
require_value policy "$policy"
command -v dd-octo-sts >/dev/null
command -v curl >/dev/null
command -v jq >/dev/null
mint_token

case "$command" in
    create-tag)
        require_value tag "$tag"
        require_value commit "$commit"
        [[ "$commit" =~ ^[0-9a-f]{40}$ ]] || { echo "commit must be a full lowercase SHA" >&2; exit 2; }
        create_or_verify_tag
        ;;
    ensure-branch)
        require_value branch "$branch"
        require_value commit "$commit"
        [[ "$commit" =~ ^[0-9a-f]{40}$ ]] || { echo "commit must be a full lowercase SHA" >&2; exit 2; }
        ensure_branch
        ;;
    require-published-release)
        require_value tag "$tag"
        require_value commit "$commit"
        [[ "$commit" =~ ^[0-9a-f]{40}$ ]] || { echo "commit must be a full lowercase SHA" >&2; exit 2; }
        require_published_release
        ;;
    record-go-results)
        require_value tag "$tag"
        require_value branch "$branch"
        require_value result-file "$result_file"
        record_go_results
        ;;
    resolve-ref)
        require_value ref "$ref"
        resolve_commit "$ref"
        ;;
    dispatch)
        require_value workflow "$workflow"
        require_value ref "$ref"
        require_value commit "$commit"
        require_value operation-id "$operation_id"
        [[ "$commit" =~ ^[0-9a-f]{40}$ ]] || { echo "commit must be a full lowercase SHA" >&2; exit 2; }
        [[ "$operation_id" =~ ^[0-9A-Za-z][0-9A-Za-z._-]{0,127}$ ]] || { echo "invalid operation ID" >&2; exit 2; }
        dispatch_and_wait
        ;;
    *) usage ;;
esac
