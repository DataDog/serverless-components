This directory stores prebuilt native libraries that ship with the Python wheel.

Each platform/architecture combination gets its own subdirectory named
"<platform>-<arch>" (for example, "darwin-arm64" or "linux-x86_64"). The
packaging script populates these directories before invoking `python -m build` so
that the resulting wheel bundles the correct shared object (dylib/so/dll).

At runtime the SDK selects the best match for the host platform. If no matching
binary is present, use `python/scripts/build_wheel.py` to rebuild the wheel or
set the DATADOG_AGENT_NATIVE_LIBRARY_PATH environment variable to point to a
compatible shared library.
