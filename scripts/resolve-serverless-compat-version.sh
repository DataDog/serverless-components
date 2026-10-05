#!/usr/bin/env bash
# Copyright 2026-Present Datadog, Inc. https://www.datadoghq.com/
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

input_version="${1:-}"
if [[ "${GITHUB_REF:-}" == refs/tags/datadog-serverless-compat/v* ]]; then
  version="${GITHUB_REF#refs/tags/datadog-serverless-compat/v}"
else
  version="$input_version"
fi

if [[ -z "$version" ]]; then
  exit 0
fi

if [[ ! "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$ ]]; then
  echo "Invalid Serverless Compat version: $version" >&2
  exit 1
fi

printf '%s\n' "$version"
