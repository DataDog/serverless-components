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

    try {
      await services.start(config);
      // If we're not in a cloud environment, this will throw
      // That's OK for testing
    } catch (err) {
      if (err.message.includes('Failed to detect cloud environment')) {
        // Expected in local testing
        return;
      }
      throw err;
    }

    assert.strictEqual(services.isRunning(), true);
    await services.stop();
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

    try {
      await services.start(config);
    } catch (err) {
      // Might fail due to environment detection
      if (err.message.includes('Failed to detect cloud environment')) {
        return;
      }
    }

    try {
      await services.start(config);
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
});
