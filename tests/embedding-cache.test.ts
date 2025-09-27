import assert from 'node:assert/strict';
import type { FeatureExtractionPipeline } from '@xenova/transformers';

import { __testing } from '../src/embedding.js';

async function run() {
  const model = 'test-model';
  let factoryCalls = 0;

  const successfulPipeline = Object.assign(
    async () => new Float32Array([1]),
    {}
  ) as FeatureExtractionPipeline;

  __testing.setPipelineFactory(async (task, requestedModel) => {
    assert.equal(task, 'feature-extraction');
    assert.equal(requestedModel, model);
    factoryCalls += 1;
    if (factoryCalls === 1) {
      throw new Error('transient failure');
    }
    return successfulPipeline;
  });

  await assert.rejects(__testing.getEmbeddingPipeline(model), /transient failure/);
  assert.equal(factoryCalls, 1);

  const recoveredPipeline = await __testing.getEmbeddingPipeline(model);
  const embedding = await recoveredPipeline('ignored');
  assert.equal(factoryCalls, 2);
  assert(embedding instanceof Float32Array);
  assert.equal(embedding.length, 1);

  const cachedPipeline = await __testing.getEmbeddingPipeline(model);
  assert.strictEqual(cachedPipeline, recoveredPipeline);
  assert.equal(factoryCalls, 2);
}

run()
  .catch((error) => {
    console.error(error);
    process.exitCode = 1;
  })
  .finally(() => {
    __testing.resetPipelineFactory();
  });
