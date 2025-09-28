import assert from 'node:assert/strict';

import { registerCleanupTask, runCleanup } from '../src/cleanup.js';
import { __testing as embeddingTesting } from '../src/embedding.js';

async function run() {
  const executionOrder: string[] = [];
  registerCleanupTask(() => {
    executionOrder.push('first');
  });
  registerCleanupTask(() => {
    executionOrder.push('second');
  });

  await runCleanup();
  assert.deepEqual(executionOrder, ['second', 'first']);

  let factoryCalls = 0;
  embeddingTesting.setPipelineFactory(async () => {
    factoryCalls += 1;
    return async () => new Float32Array([1]);
  });

  await embeddingTesting.getEmbeddingPipeline('test-model');
  assert.equal(factoryCalls, 1);

  await runCleanup();

  await embeddingTesting.getEmbeddingPipeline('test-model');
  assert.equal(factoryCalls, 2);

  embeddingTesting.resetPipelineFactory();
  await runCleanup();
}

run().catch((error) => {
  console.error(error);
  process.exitCode = 1;
});
