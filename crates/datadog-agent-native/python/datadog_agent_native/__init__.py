# Unless explicitly stated otherwise all files in this repository are licensed under the Apache 2 License.
# This product includes software developed at Datadog (https://www.datadoghq.com/). Copyright 2017 Datadog, Inc.

"""
Datadog Agent Native - Python bindings for the native Datadog Agent.

This package provides a Python interface to the native Rust-based Datadog Agent
for serverless and containerized environments.
"""

from .native_agent import (
    DatadogAgentConfig,
    DatadogAgentException,
    DatadogError,
    LogLevel,
    NativeAgent,
    OperationalMode,
    ShutdownReason,
    create_instance,
    get_instance,
)

__all__ = [
    "DatadogAgentConfig",
    "DatadogAgentException",
    "DatadogError",
    "LogLevel",
    "NativeAgent",
    "OperationalMode",
    "ShutdownReason",
    "create_instance",
    "get_instance",
]

__version__ = "0.1.0"
