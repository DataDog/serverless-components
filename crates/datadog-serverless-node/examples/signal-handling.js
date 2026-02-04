const { DatadogServices } = require('../index.js');

// Create services instance
const services = new DatadogServices();

// Configuration
const config = {
  apiKey: process.env.DD_API_KEY || 'test-key',
  site: process.env.DD_SITE || 'datadoghq.com',
  dogstatsdPort: 8125,
  useDogstatsd: process.env.DD_USE_DOGSTATSD !== 'false',
  logLevel: process.env.DD_LOG_LEVEL || 'info'
};

// Signal handler for graceful shutdown
function shutdown(signal) {
  console.log(`\nReceived ${signal}, shutting down gracefully...`);

  if (services.isRunning()) {
    services.stop();
    console.log('Datadog services stopped');
  }

  process.exit(0);
}

// Register signal handlers
process.on('SIGTERM', () => shutdown('SIGTERM'));
process.on('SIGINT', () => shutdown('SIGINT'));
process.on('SIGQUIT', () => shutdown('SIGQUIT'));

// Start services with status callback
services.start(config, (status) => {
  console.log('Service status:', status.status);
});

console.log('Datadog services starting...');
console.log('Press Ctrl+C to trigger graceful shutdown');
console.log('Send SIGTERM with: kill -TERM', process.pid);
console.log('Send SIGQUIT with: kill -QUIT', process.pid);

// Keep process alive
setInterval(() => {
  // Simulate some work
}, 1000);
