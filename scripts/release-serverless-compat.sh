#!/usr/bin/env bash
# Authenticated launcher for the repository's Full DDCI on-demand release rule.

set -euo pipefail

usage() {
    cat >&2 <<'USAGE'
usage: scripts/release-serverless-compat.sh [--release-flow draft|publish|release]
  --core-version VERSION --core-commit SHA
  --python-version VERSION --python-commit SHA
  --dotnet-version VERSION --dotnet-commit SHA
  --go-version VERSION --go-commit SHA
  --java-version VERSION --java-commit SHA
  --javascript-version VERSION --javascript-commit SHA
  [--release-python true|false] [--release-dotnet true|false]
  [--release-go true|false] [--release-java true|false]
  [--release-javascript true|false]
USAGE
    exit 2
}

release_flow=draft
release_python=true
release_dotnet=true
release_go=true
release_java=true
release_javascript=true
core_version=""; core_commit=""
python_version=""; python_commit=""
dotnet_version=""; dotnet_commit=""
go_version=""; go_commit=""
java_version=""; java_commit=""
javascript_version=""; javascript_commit=""

while (( $# )); do
    case "$1" in
        --release-flow) release_flow="${2:-}"; shift 2 ;;
        --core-version) core_version="${2:-}"; shift 2 ;;
        --core-commit) core_commit="${2:-}"; shift 2 ;;
        --python-version) python_version="${2:-}"; shift 2 ;;
        --python-commit) python_commit="${2:-}"; shift 2 ;;
        --dotnet-version) dotnet_version="${2:-}"; shift 2 ;;
        --dotnet-commit) dotnet_commit="${2:-}"; shift 2 ;;
        --go-version) go_version="${2:-}"; shift 2 ;;
        --go-commit) go_commit="${2:-}"; shift 2 ;;
        --java-version) java_version="${2:-}"; shift 2 ;;
        --java-commit) java_commit="${2:-}"; shift 2 ;;
        --javascript-version) javascript_version="${2:-}"; shift 2 ;;
        --javascript-commit) javascript_commit="${2:-}"; shift 2 ;;
        --release-python) release_python="${2:-}"; shift 2 ;;
        --release-dotnet) release_dotnet="${2:-}"; shift 2 ;;
        --release-go) release_go="${2:-}"; shift 2 ;;
        --release-java) release_java="${2:-}"; shift 2 ;;
        --release-javascript) release_javascript="${2:-}"; shift 2 ;;
        *) usage ;;
    esac
done

semver_re='^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?(\+[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?$'
sha_re='^[0-9a-f]{40}$'
validate_match() {
    local name="$1" value="$2" expression="$3"
    if [[ ! "$value" =~ $expression ]]; then
        echo "$name has an invalid or missing value" >&2
        exit 2
    fi
}
validate_bool() {
    local name="$1" value="$2"
    if [[ "$value" != true && "$value" != false ]]; then
        echo "$name must be true or false" >&2
        exit 2
    fi
}

case "$release_flow" in draft|publish|release) ;; *) usage ;; esac
validate_match core-version "$core_version" "$semver_re"
validate_match core-commit "$core_commit" "$sha_re"
validate_match python-version "$python_version" "$semver_re"
validate_match python-commit "$python_commit" "$sha_re"
validate_match dotnet-version "$dotnet_version" "$semver_re"
validate_match dotnet-commit "$dotnet_commit" "$sha_re"
validate_match go-version "$go_version" "$semver_re"
validate_match go-commit "$go_commit" "$sha_re"
validate_match java-version "$java_version" "$semver_re"
validate_match java-commit "$java_commit" "$sha_re"
validate_match javascript-version "$javascript_version" "$semver_re"
validate_match javascript-commit "$javascript_commit" "$sha_re"
validate_bool release-python "$release_python"
validate_bool release-dotnet "$release_dotnet"
validate_bool release-go "$release_go"
validate_bool release-java "$release_java"
validate_bool release-javascript "$release_javascript"

command -v ddr >/dev/null || { echo "ddr is required; authenticate it before launching" >&2; exit 1; }

render_env="RELEASE_FLOW=$release_flow"
render_env+="|CORE_VERSION=$core_version|CORE_COMMIT=$core_commit"
render_env+="|PYTHON_VERSION=$python_version|PYTHON_COMMIT=$python_commit|RELEASE_PYTHON=$release_python"
render_env+="|DOTNET_VERSION=$dotnet_version|DOTNET_COMMIT=$dotnet_commit|RELEASE_DOTNET=$release_dotnet"
render_env+="|GO_VERSION=$go_version|GO_COMMIT=$go_commit|RELEASE_GO=$release_go"
render_env+="|JAVA_VERSION=$java_version|JAVA_COMMIT=$java_commit|RELEASE_JAVA=$release_java"
render_env+="|JAVASCRIPT_VERSION=$javascript_version|JAVASCRIPT_COMMIT=$javascript_commit|RELEASE_JAVASCRIPT=$release_javascript"

# ddr authenticates the caller and uses Change Orchestrator's on-demand API.
exec ddr devflow trigger-ci \
    --ref main \
    -v DYNAMIC_BUILD_RENDER_RULES=serverless-compat-release \
    -v "DYNAMIC_BUILD_RENDER_TARGET_ENV=$render_env"
