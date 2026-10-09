#!/usr/bin/env sh

set -eu

semver_re='^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?(\+[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?$'
sha_re='^[0-9a-f]{40}$'

require_match() {
    name="$1"
    value="$2"
    expression="$3"
    if [ -z "$value" ] || ! printf '%s\n' "$value" | grep -Eq "$expression"; then
        echo "$name has an invalid or missing value" >&2
        exit 1
    fi
}

require_bool() {
    name="$1"
    value="$2"
    if [ "$value" != true ] && [ "$value" != false ]; then
        echo "$name must be true or false" >&2
        exit 1
    fi
}

case "${RELEASE_FLOW:-draft}" in
    draft|publish|release) ;;
    *) echo "RELEASE_FLOW must be draft, publish, or release" >&2; exit 1 ;;
esac

require_match CORE_VERSION "${CORE_VERSION:-}" "$semver_re"
require_match CORE_COMMIT "${CORE_COMMIT:-}" "$sha_re"
require_match PYTHON_VERSION "${PYTHON_VERSION:-}" "$semver_re"
require_match PYTHON_COMMIT "${PYTHON_COMMIT:-}" "$sha_re"
require_match DOTNET_VERSION "${DOTNET_VERSION:-}" "$semver_re"
require_match DOTNET_COMMIT "${DOTNET_COMMIT:-}" "$sha_re"
require_match GO_VERSION "${GO_VERSION:-}" "$semver_re"
require_match GO_COMMIT "${GO_COMMIT:-}" "$sha_re"
require_match JAVA_VERSION "${JAVA_VERSION:-}" "$semver_re"
require_match JAVA_COMMIT "${JAVA_COMMIT:-}" "$sha_re"
require_match JAVASCRIPT_VERSION "${JAVASCRIPT_VERSION:-}" "$semver_re"
require_match JAVASCRIPT_COMMIT "${JAVASCRIPT_COMMIT:-}" "$sha_re"

require_bool RELEASE_PYTHON "${RELEASE_PYTHON:-true}"
require_bool RELEASE_DOTNET "${RELEASE_DOTNET:-true}"
require_bool RELEASE_GO "${RELEASE_GO:-true}"
require_bool RELEASE_JAVA "${RELEASE_JAVA:-true}"
require_bool RELEASE_JAVASCRIPT "${RELEASE_JAVASCRIPT:-true}"

if [ "${CI_COMMIT_BRANCH:-}" != "${CI_DEFAULT_BRANCH:-main}" ] ||
   [ -n "${CI_COMMIT_TAG:-}" ] ||
   [ "${CI_COMMIT_REF_PROTECTED:-}" != "true" ]; then
    echo "release orchestration must render from the protected default branch" >&2
    exit 1
fi

printf 'release flow %s accepted; operation ddci-%s\n' "${RELEASE_FLOW:-draft}" "${CI_PIPELINE_ID:?}"
