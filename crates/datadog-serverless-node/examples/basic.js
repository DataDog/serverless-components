/**
 * Basic example of using @datadog/serverless-node
 *
 * This example demonstrates:
 * - Creating a DatadogServices instance
 * - Configuring services
 * - Starting services with status callbacks
 * - Checking service status
 * - Graceful shutdown
 *
 * To run this example:
 * 1. Build the native module: npm run build
 * 2. Set your API key: export DD_API_KEY=your-api-key
 * 3. Run: node examples/basic.js
 */

const { DatadogServices } = require('../index.js');

// Create a new DatadogServices instance
console.log('Creating DatadogServices instance...');
const services = new DatadogServices();

// Register cleanup hook for automatic shutdown
services.registerCleanupHook();
console.log('Cleanup hook registered for automatic shutdown on exit');

// Configuration
const config = {
  // API key from environment variable (or hardcode for testing)
  apiKey: process.env.DD_API_KEY || 'test-api-key',

  // Datadog site (use datadoghq.eu for EU, etc.)
  site: process.env.DD_SITE || 'datadoghq.com',

  // DogStatsD port
  dogstatsdPort: parseInt(process.env.DD_DOGSTATSD_PORT || '8125', 10),

  // Enable/disable DogStatsD metrics
  useDogstatsd: process.env.DD_USE_DOGSTATSD !== 'false',

  // Optional metric namespace prefix
  metricNamespace: process.env.DD_METRIC_NAMESPACE || 'example',

  // Optional HTTPS proxy
  httpsProxy: process.env.HTTPS_PROXY,

  // Log level: trace, debug, info, warn, error
  logLevel: process.env.DD_LOG_LEVEL || 'info'
};

console.log('Configuration:', {
  ...config,
  apiKey: config.apiKey ? '***' + config.apiKey.slice(-4) : 'not set'
});

// Status callback to monitor service lifecycle
const statusCallback = (status) => {
  const timestamp = new Date().toISOString();
  console.log(`[${timestamp}] Service status changed: ${status.status}`);

  switch (status.status) {
    case 'starting':
      console.log('  → Services are initializing...');
      break;
    case 'running':
      console.log('  → Services are now active and collecting data');
      break;
    case 'stopping':
      console.log('  → Services are shutting down...');
      break;
    case 'stopped':
      console.log('  → Services have been stopped');
      break;
  }
};

// Start the services
console.log('\nStarting Datadog services...');
try {
  services.start(config, statusCallback);
  console.log('Start command issued successfully');
} catch (error) {
  console.error('Failed to start services:', error.message);
  process.exit(1);
}

// Check if services are running
setTimeout(() => {
  if (services.isRunning()) {
    console.log('\n✓ Services are running');
    console.log('  DogStatsD is listening on port', config.dogstatsdPort);
    console.log('  You can now send metrics to localhost:' + config.dogstatsdPort);
  } else {
    console.log('\n⚠ Services are not running (expected in non-cloud environments)');
  }
}, 2000);

// Simulate some work
console.log('\nSimulating application workload for 5 seconds...');
console.log('(In a real application, this is where your business logic runs)');

// Demonstrate error handling
setTimeout(() => {
  console.log('\n--- Testing Error Handling ---');

  // Try to start again (should fail)
  try {
    services.start(config);
    console.log('ERROR: Should not be able to start twice!');
  } catch (error) {
    console.log('✓ Correctly prevented double start:', error.message);
  }
}, 3000);

// Graceful shutdown after 5 seconds
setTimeout(() => {
  console.log('\n--- Initiating Graceful Shutdown ---');

  if (services.isRunning()) {
    console.log('Stopping services...');
    try {
      services.stop();
      console.log('Stop command issued successfully');
    } catch (error) {
      console.error('Failed to stop services:', error.message);
    }
  } else {
    console.log('Services were not running, no cleanup needed');
  }

  // Wait a moment for async shutdown to complete
  setTimeout(() => {
    console.log('\n--- Example Complete ---');
    console.log('Final status:', services.isRunning() ? 'running' : 'stopped');
    console.log('\nThis example demonstrated:');
    console.log('  ✓ Creating a DatadogServices instance');
    console.log('  ✓ Configuring services with environment variables');
    console.log('  ✓ Starting services with status callbacks');
    console.log('  ✓ Checking service status');
    console.log('  ✓ Error handling (double start prevention)');
    console.log('  ✓ Graceful shutdown');
    console.log('\nFor production use:');
    console.log('  - Set DD_API_KEY environment variable');
    console.log('  - Initialize once at application startup');
    console.log('  - Stop on SIGTERM/SIGINT for graceful shutdown');
    console.log('  - See README.md for more examples');

    process.exit(0);
  }, 1000);
}, 5000);

// Handle process signals for graceful shutdown
process.on('SIGTERM', () => {
  console.log('\nReceived SIGTERM signal');
  if (services.isRunning()) {
    console.log('Stopping services...');
    services.stop();
  }
  process.exit(0);
});

process.on('SIGINT', () => {
  console.log('\nReceived SIGINT signal');
  if (services.isRunning()) {
    console.log('Stopping services...');
    services.stop();
  }
  process.exit(0);
});

// Handle uncaught errors
process.on('uncaughtException', (error) => {
  console.error('\nUncaught exception:', error);
  if (services.isRunning()) {
    console.log('Stopping services due to error...');
    try {
      services.stop();
    } catch (stopError) {
      console.error('Failed to stop services:', stopError);
    }
  }
  process.exit(1);
});
