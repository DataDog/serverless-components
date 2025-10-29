# Unless explicitly stated otherwise all files in this repository are licensed under the Apache 2 License.
# This product includes software developed at Datadog (https://www.datadoghq.com/). Copyright 2017 Datadog, Inc.

"""
Python wrapper for the Datadog Agent Native library.

This module provides a Python interface to the native Rust-based Datadog Agent
using ctypes for FFI (Foreign Function Interface).
"""

import ctypes
import logging
import os
import platform
from ctypes import POINTER, Structure, c_char_p, c_int, c_void_p
from enum import IntEnum
from pathlib import Path
from typing import Iterable, Optional

logger = logging.getLogger(__name__)

_LIBRARY_FILENAMES = {
    "darwin": "libdatadog_agent_native.dylib",
    "linux": "libdatadog_agent_native.so",
    "windows": "datadog_agent_native.dll",
}

_SYSTEM_ALIASES = {
    "darwin": "darwin",
    "macos": "darwin",
    "mac os": "darwin",
    "mac os x": "darwin",
    "linux": "linux",
    "windows": "windows",
    "win32": "windows",
}

_ARCH_ALIASES = {
    "x86_64": "x86_64",
    "amd64": "x86_64",
    "arm64": "arm64",
    "aarch64": "arm64",
}


def _normalized_system_name(raw: Optional[str] = None) -> str:
    system = (raw or platform.system() or "").lower()
    return _SYSTEM_ALIASES.get(system, system)


def _normalized_arch_name(raw: Optional[str] = None) -> str:
    arch = (raw or platform.machine() or "").lower()
    return _ARCH_ALIASES.get(arch, arch)


def _library_filename(normalized_system: str) -> str:
    try:
        return _LIBRARY_FILENAMES[normalized_system]
    except KeyError as exc:
        raise DatadogAgentException(
            f"Unsupported platform: {platform.system()} ({platform.machine()})"
        ) from exc


def _library_candidate_paths(lib_name: str, normalized_system: str) -> Iterable[Path]:
    package_dir = Path(__file__).resolve().parent
    bin_root = package_dir / "bin"
    normalized_arch = _normalized_arch_name()

    env_override = os.environ.get("DATADOG_AGENT_NATIVE_LIBRARY_PATH")
    if env_override:
        override_path = Path(env_override).expanduser()
        if override_path.is_dir():
            yield override_path / lib_name
        else:
            yield override_path

    if normalized_arch:
        yield bin_root / f"{normalized_system}-{normalized_arch}" / lib_name

    yield bin_root / normalized_system / lib_name
    yield bin_root / lib_name
    yield package_dir / lib_name
    yield package_dir.parent / lib_name


class LogLevel(IntEnum):
    """Log levels for the agent."""
    ERROR = 0
    WARN = 2
    INFO = 3
    DEBUG = 4
    TRACE = 5


class OperationalMode(IntEnum):
    """Operational modes for the agent."""
    HTTP_FIXED_PORT = 0      # Traditional mode with fixed HTTP ports (default 8126)
    HTTP_EPHEMERAL_PORT = 1  # HTTP server with OS-assigned ports (port 0)
    HTTP_UDS = 2             # Unix Domain Socket mode (Unix only)


class ShutdownReason(IntEnum):
    """Shutdown reasons."""
    GRACEFUL_SHUTDOWN = 0
    USER_INTERRUPT = 1
    FATAL_ERROR = 2
    TIMEOUT = 3


class DatadogError(IntEnum):
    """Error codes from native FFI."""
    OK = 0
    NULL_POINTER = 1
    INVALID_STRING = 2
    CONFIG_ERROR = 3
    INIT_ERROR = 4
    STARTUP_ERROR = 5
    SHUTDOWN_ERROR = 6
    RUNTIME_ERROR = 7
    INVALID_DATA_FORMAT = 8
    SUBMISSION_ERROR = 9
    NOT_AVAILABLE = 10
    UNKNOWN_ERROR = 99


class DatadogAgentException(Exception):
    """Exception thrown by Datadog Agent operations."""
    pass


class DatadogAgentOptions(Structure):
    """Native FFI options structure (matches Rust struct)."""
    _fields_ = [
        ("api_key", c_char_p),
        ("site", c_char_p),
        ("service", c_char_p),
        ("env", c_char_p),
        ("version", c_char_p),
        ("appsec_enabled", c_int),
        ("remote_config_enabled", c_int),
        ("log_level", c_int),
        ("operational_mode", c_int),
        ("trace_agent_port", c_int),
        ("dogstatsd_enabled", c_int),
        ("dogstatsd_port", c_int),
        ("trace_agent_uds_permissions", c_int),
    ]


class DatadogAgentStartResult(Structure):
    """Result returned from datadog_agent_start."""
    _fields_ = [
        ("agent", c_void_p),
        ("error", c_int),
        ("version", c_char_p),
        ("bound_port", c_int),
        ("dogstatsd_port", c_int),
        ("uds_path", c_char_p),
    ]


class DatadogAgentConfig:
    """Configuration for the Datadog Agent."""

    def __init__(
        self,
        api_key: Optional[str] = None,
        site: Optional[str] = None,
        service: Optional[str] = None,
        environment: Optional[str] = None,
        version: Optional[str] = None,
        appsec_enabled: bool = False,
        remote_config_enabled: bool = True,
        log_level: LogLevel = LogLevel.INFO,
        operational_mode: OperationalMode = OperationalMode.HTTP_EPHEMERAL_PORT,
        trace_agent_port: Optional[int] = 0,
        dogstatsd_enabled: bool = False,
        dogstatsd_port: Optional[int] = 0,
        trace_agent_uds_permissions: Optional[int] = None,
    ):
        """
        Initialize Datadog Agent configuration.

        Args:
            api_key: Datadog API key (if None, will be read from DD_API_KEY env var)
            site: Datadog site (e.g., "datadoghq.com", "datadoghq.eu")
            service: Service name for Unified Service Tagging
            environment: Environment name (e.g., "production", "staging")
            version: Version for Unified Service Tagging
            appsec_enabled: Enable Application Security (AppSec/WAF)
            remote_config_enabled: Enable Remote Configuration
            log_level: Log level for the agent
            operational_mode: Operational mode for the agent
            trace_agent_port: Trace agent port (None=default 8126, 0=ephemeral, >0=specific)
            dogstatsd_enabled: Enable DogStatsD UDP server
            dogstatsd_port: DogStatsD UDP port (None=default 8125, 0=ephemeral, >0=specific)
            trace_agent_uds_permissions: Unix Domain Socket permissions (Unix only)
        """
        self.api_key = api_key
        self.site = site
        self.service = service
        self.environment = environment
        self.version = version
        self.appsec_enabled = appsec_enabled
        self.remote_config_enabled = remote_config_enabled
        self.log_level = log_level
        self.operational_mode = operational_mode
        self.trace_agent_port = trace_agent_port
        self.dogstatsd_enabled = dogstatsd_enabled
        self.dogstatsd_port = dogstatsd_port
        self.trace_agent_uds_permissions = trace_agent_uds_permissions

    def to_native_options(self) -> DatadogAgentOptions:
        """Convert to native FFI options structure.

        Note: The returned structure contains pointers to encoded strings that are
        managed by Python's garbage collector. The caller should keep the structure
        alive during the FFI call to prevent premature deallocation.
        """
        return DatadogAgentOptions(
            api_key=self.api_key.encode('utf-8') if self.api_key else None,
            site=self.site.encode('utf-8') if self.site else None,
            service=self.service.encode('utf-8') if self.service else None,
            env=self.environment.encode('utf-8') if self.environment else None,
            version=self.version.encode('utf-8') if self.version else None,
            appsec_enabled=1 if self.appsec_enabled else 0,
            remote_config_enabled=1 if self.remote_config_enabled else 0,
            log_level=int(self.log_level),
            operational_mode=int(self.operational_mode),
            trace_agent_port=self.trace_agent_port if self.trace_agent_port is not None else -1,
            dogstatsd_enabled=1 if self.dogstatsd_enabled else 0,
            dogstatsd_port=self.dogstatsd_port if self.dogstatsd_port is not None else -1,
            trace_agent_uds_permissions=self.trace_agent_uds_permissions if self.trace_agent_uds_permissions is not None else -1,
        )


class NativeAgent:
    """
    Python wrapper for the Datadog Agent Native library.

    This class provides a Python interface to the native Rust-based Datadog Agent.
    """

    _lib = None
    _lib_path = None

    def __init__(self, config: DatadogAgentConfig):
        """
        Initialize a new instance of the NativeAgent.

        Args:
            config: Configuration options for the agent

        Raises:
            DatadogAgentException: If agent initialization fails
        """
        if config is None:
            raise ValueError("config cannot be None")

        # Load the native library if not already loaded
        if NativeAgent._lib is None:
            NativeAgent._lib = self._load_library()

        # Convert config to native options
        native_options = config.to_native_options()

        # Call native start function
        result = NativeAgent._lib.datadog_agent_start(native_options)

        # Check for errors
        if result.error != DatadogError.OK or not result.agent:
            raise DatadogAgentException(
                f"Failed to start Datadog agent: {DatadogError(result.error).name}. "
                f"Check API key and configuration."
            )

        self._agent_handle = result.agent
        self._disposed = False

        # Extract version string
        self.version = result.version.decode('utf-8') if result.version else "unknown"

        # Extract bound port
        self.bound_port = result.bound_port if result.bound_port > 0 else None

        # Extract DogStatsD port
        self.dogstatsd_port = result.dogstatsd_port if result.dogstatsd_port > 0 else None

        # Extract UDS path
        self.uds_path = result.uds_path.decode('utf-8') if result.uds_path else None

    @staticmethod
    def _load_library():
        """Load the native library."""
        normalized_system = _normalized_system_name()
        lib_name = _library_filename(normalized_system)

        attempted_paths = []
        load_errors = []

        for candidate in _library_candidate_paths(lib_name, normalized_system):
            candidate_path = candidate.expanduser().resolve()
            attempted_paths.append(candidate_path)

            if not candidate_path.exists():
                continue

            try:
                lib = ctypes.CDLL(str(candidate_path))
                NativeAgent._lib_path = str(candidate_path)
                NativeAgent._configure_library(lib)
                return lib
            except OSError as exc:
                load_errors.append(f"{candidate_path}: {exc}")

        try:
            lib = ctypes.CDLL(lib_name)
            NativeAgent._lib_path = lib_name
            NativeAgent._configure_library(lib)
            return lib
        except OSError as exc:
            attempted = ", ".join(str(path) for path in attempted_paths) or "<no bundled paths>"
            message = (
                f"Failed to load native library {lib_name}. "
                f"Checked: {attempted}. "
                "Set DATADOG_AGENT_NATIVE_LIBRARY_PATH to the compiled library or rebuild the platform-specific wheel."
            )
            if load_errors:
                message += f" Loader errors: {'; '.join(load_errors)}."
            raise DatadogAgentException(message) from exc

    @staticmethod
    def _configure_library(lib):
        """Configure return and argument types for native functions."""
        lib.datadog_agent_start.argtypes = [DatadogAgentOptions]
        lib.datadog_agent_start.restype = DatadogAgentStartResult

        lib.datadog_agent_stop.argtypes = [c_void_p]
        lib.datadog_agent_stop.restype = c_int

        lib.datadog_agent_wait_for_shutdown.argtypes = [c_void_p]
        lib.datadog_agent_wait_for_shutdown.restype = c_int

    def wait_for_shutdown(self) -> ShutdownReason:
        """
        Wait for a shutdown signal (blocking).
        Returns when Ctrl+C, SIGTERM, or programmatic shutdown is triggered.

        Returns:
            Shutdown reason code

        Raises:
            DatadogAgentException: If agent is disposed
        """
        self._check_disposed()
        reason = NativeAgent._lib.datadog_agent_wait_for_shutdown(self._agent_handle)
        return ShutdownReason(reason)

    def stop(self):
        """Stop the agent and free all resources."""
        if self._disposed:
            return

        if self._agent_handle is not None:
            NativeAgent._lib.datadog_agent_stop(self._agent_handle)
            self._agent_handle = None

        self._disposed = True

    def _check_disposed(self):
        """Check if the agent has been disposed."""
        if self._disposed:
            raise DatadogAgentException("NativeAgent has been disposed")

    def __enter__(self):
        """Context manager entry."""
        return self

    def __exit__(self, exc_type, exc_val, exc_tb):
        """Context manager exit."""
        self.stop()

    def __del__(self):
        """Destructor."""
        self.stop()


# Singleton instance support
_instance: Optional[NativeAgent] = None


def create_instance(config: Optional[DatadogAgentConfig] = None) -> Optional[NativeAgent]:
    """
    Create a singleton instance of NativeAgent.

    Args:
        config: Configuration for the agent. If None, uses defaults with AppSec and DogStatsD enabled.

    Returns:
        NativeAgent instance or None if creation fails
    """
    global _instance

    try:
        if config is None:
            config = DatadogAgentConfig(
                appsec_enabled=True,
                log_level=LogLevel.DEBUG,
                dogstatsd_enabled=True,
            )

        _instance = NativeAgent(config)

        logger.info(f"Native Agent Version: {_instance.version}")
        logger.info(f"Native Trace Agent Port: {_instance.bound_port}")
        logger.info(f"Native UDP Port: {_instance.dogstatsd_port}")
        if _instance.uds_path:
            logger.info(f"Native UDS Path: {_instance.uds_path}")

        return _instance
    except Exception as e:
        logger.error(f"Native Agent Error: {e}")
        return None


def get_instance() -> NativeAgent:
    """
    Get the singleton instance of NativeAgent.

    Returns:
        The singleton NativeAgent instance

    Raises:
        RuntimeError: If the instance has not been created
    """
    if _instance is None:
        raise RuntimeError("NativeAgent instance not initialized. Call create_instance() first.")
    return _instance
