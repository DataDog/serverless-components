/*
 * Example C program demonstrating the Datadog Agent Native FFI interface
 *
 * Compile:
 *   gcc -o example example.c -L../target/debug -ldatadog_agent_native -lpthread -ldl -lm
 *
 * Run (macOS):
 *   DYLD_LIBRARY_PATH=../target/debug ./example
 *
 * Run (Linux):
 *   LD_LIBRARY_PATH=../target/debug ./example
 */

#include <stdio.h>
#include <stdlib.h>
#include "../datadog_agent_native.h"

int main(int argc, char *argv[]) {
    printf("Datadog Agent Native Example (C)\n");
    printf("Version: %s\n\n", datadog_agent_version());

    // Get API key from environment or use placeholder
    const char* api_key = getenv("DD_API_KEY");
    if (!api_key) {
        api_key = "test-api-key-placeholder";
        printf("⚠ No DD_API_KEY environment variable set, using placeholder\n");
    }

    // Create configuration options struct
    DatadogAgentOptions options = {
        .api_key = api_key,
        .site = "datadoghq.com",
        .service = "example-c-app",
        .env = "development",
        .version = "1.0.0",
        .appsec_enabled = 0,           // Disable AppSec
        .remote_config_enabled = 1,    // Enable Remote Config
        .log_level = 3,                // Info level
    };

    printf("\nConfiguration:\n");
    printf("- Site: %s\n", options.site);
    printf("- Service: %s\n", options.service);
    printf("- Environment: %s\n", options.env);
    printf("- Version: %s\n", options.version);
    printf("- AppSec: %s\n", options.appsec_enabled ? "enabled" : "disabled");
    printf("- Remote Config: %s\n", options.remote_config_enabled ? "enabled" : "disabled");

    // Create and start agent
    printf("\nStarting Datadog Agent...\n");
    DatadogAgent* agent = datadog_agent_start(&options);
    if (!agent) {
        fprintf(stderr, "ERROR: Failed to start agent\n");
        return 1;
    }
    printf("✓ Agent started successfully\n");

    printf("\nAgent is running!\n");
    printf("- Hostname will be auto-detected\n");
    printf("- Event bus is processing events\n");
    printf("- Press Ctrl+C to shutdown\n\n");

    // Wait for shutdown signal
    int shutdown_reason = datadog_agent_wait_for_shutdown(agent);
    printf("\n\nShutdown requested (reason: %d)\n", shutdown_reason);

    // Stop and cleanup
    printf("Stopping agent...\n");
    DatadogError error = datadog_agent_stop(agent);
    if (error != Ok) {
        fprintf(stderr, "WARNING: Shutdown encountered errors: %d\n", error);
        return 1;
    }

    printf("✓ Agent stopped cleanly\n");
    printf("\nExample completed successfully\n");
    return 0;
}
