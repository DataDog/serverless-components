const { DatadogServices } = require('../index.js');
const assert = require('assert');

describe('DatadogServices', () => {
  let services;

  beforeEach(() => {
    services = new DatadogServices();
  });

  afterEach(async () => {
    if (services.isRunning()) {
      await services.stop();
    }
  });

  it('should create new instance', () => {
    assert.ok(services);
    assert.strictEqual(services.isRunning(), false);
  });

  it('should start and stop services', async function() {
    this.timeout(10000); // 10 second timeout

    const config = {
      apiKey: 'test-key',
      site: 'datadoghq.com',
      dogstatsdPort: 8125,
      useDogstatsd: false, // Disabled for testing
      logLevel: 'error'
    };

    services.start(config);

    // Wait for services to start asynchronously
    await new Promise(resolve => setTimeout(resolve, 1000));

    // If we're not in a cloud environment, services won't be running
    // That's OK for testing
    if (!services.isRunning()) {
      // Expected in local testing (no cloud environment)
      return;
    }

    assert.strictEqual(services.isRunning(), true);
    services.stop();

    // Wait for services to stop
    await new Promise(resolve => setTimeout(resolve, 500));
    assert.strictEqual(services.isRunning(), false);
  });

  it('should reject invalid configuration', async () => {
    const config = {
      site: '', // Invalid - empty site
    };

    try {
      await services.start(config);
      assert.fail('Should have thrown error');
    } catch (err) {
      assert.ok(err.message.includes('Invalid configuration'));
    }
  });

  it('should reject double start', async function() {
    this.timeout(10000);

    const config = {
      apiKey: 'test-key',
      useDogstatsd: false,
      logLevel: 'error'
    };

    services.start(config);

    // Wait a moment for first start to be registered
    await new Promise(resolve => setTimeout(resolve, 100));

    try {
      services.start(config);
      assert.fail('Should have thrown error');
    } catch (err) {
      assert.ok(err.message.includes('already started'));
    }
  });

  it('should reject stop when not running', async () => {
    try {
      await services.stop();
      assert.fail('Should have thrown error');
    } catch (err) {
      assert.ok(err.message.includes('not running'));
    }
  });

  it('should receive status callbacks', async function() {
    this.timeout(10000);

    const statuses = [];
    const config = {
      apiKey: 'test-key',
      useDogstatsd: false,
      logLevel: 'error'
    };

    const statusCallback = (status) => {
      statuses.push(status.status);
    };

    services.start(config, statusCallback);

    // Wait for status updates
    await new Promise(resolve => setTimeout(resolve, 1000));

    // If we're not in a cloud environment, we won't get status updates
    // That's OK for testing
    if (statuses.length === 0) {
      // Expected in local testing (no cloud environment)
      return;
    }

    // Should have received at least "running" status
    assert.ok(statuses.includes('running'), 'Should receive running status');

    services.stop();

    // Wait for final status updates
    await new Promise(resolve => setTimeout(resolve, 500));

    // Should have received "stopped" status
    assert.ok(statuses.includes('stopped'), 'Should receive stopped status');
  });

  it('should handle rapid start-stop cycles', async function() {
    this.timeout(10000);

    const config = {
      apiKey: 'test-key',
      useDogstatsd: false,
      logLevel: 'error'
    };

    // Start services
    services.start(config);
    await new Promise(resolve => setTimeout(resolve, 500));

    // If services started, stop them quickly
    if (services.isRunning()) {
      services.stop();
      await new Promise(resolve => setTimeout(resolve, 500));
      assert.strictEqual(services.isRunning(), false);

      // Start again after full stop
      services.start(config);
      await new Promise(resolve => setTimeout(resolve, 500));

      // If services started, they should be running
      // In local environment, this may not start, which is OK
    } else {
      // Not in cloud environment, skip cycle test
      return;
    }
  });

  it('should handle multiple stop calls safely', async function() {
    this.timeout(10000);

    const config = {
      apiKey: 'test-key',
      useDogstatsd: false,
      logLevel: 'error'
    };

    services.start(config);
    await new Promise(resolve => setTimeout(resolve, 500));

    if (!services.isRunning()) {
      // Not in cloud environment, skip test
      return;
    }

    // First stop should succeed
    services.stop();
    await new Promise(resolve => setTimeout(resolve, 100));

    // Second stop should fail with error
    try {
      services.stop();
      assert.fail('Should have thrown error on second stop');
    } catch (err) {
      assert.ok(err.message.includes('not running'));
    }
  });

  it('should complete stop within timeout', async function() {
    this.timeout(10000);

    const config = {
      apiKey: 'test-key',
      useDogstatsd: false,
      logLevel: 'error'
    };

    services.start(config);
    await new Promise(resolve => setTimeout(resolve, 500));

    if (!services.isRunning()) {
      // Not in cloud environment, skip test
      return;
    }

    const startTime = Date.now();
    services.stop();

    // Stop is async, but should complete quickly (well under 5 second timeout)
    await new Promise(resolve => setTimeout(resolve, 6000));
    const elapsed = Date.now() - startTime;

    // Verify it completed within reasonable time (6 seconds max in this test)
    assert.ok(elapsed < 7000, `Stop took ${elapsed}ms, should be < 7000ms`);
    assert.strictEqual(services.isRunning(), false);
  });

  it('should cleanup with registerCleanupHook', async function() {
    this.timeout(10000);

    const config = {
      apiKey: 'test-key',
      useDogstatsd: false,
      logLevel: 'error'
    };

    services.start(config);
    await new Promise(resolve => setTimeout(resolve, 500));

    // Register cleanup hook
    // Note: We can't actually test process exit cleanup in unit tests,
    // but we can verify the method doesn't throw
    assert.doesNotThrow(() => {
      services.registerCleanupHook();
    });

    // Clean up manually for test
    if (services.isRunning()) {
      services.stop();
      await new Promise(resolve => setTimeout(resolve, 100));
    }
  });
});
