import assert from 'node:assert/strict';

import { embedTexts, clearEmbeddingPipelineCache, __testing } from '../src/embedding.js';

async function run() {
  let callCount = 0;

  __testing.setEmbeddingProvider(async (texts) => {
    callCount += 1;
    if (callCount === 1) {
      throw new Error('transient failure');
    }
    return texts.map(() => new Float32Array([callCount]));
  });

  await assert.rejects(embedTexts(['hello']), /transient failure/);
  assert.equal(callCount, 1);

  const recovered = await embedTexts(['hello']);
  assert.equal(callCount, 2);
  assert.equal(recovered.length, 1);
  assert.ok(recovered[0] instanceof Float32Array);
  assert.equal(recovered[0][0], 2);

  __testing.reset();
  assert.equal(__testing.getOverrideProvider(), null);

  __testing.setEmbeddingProvider(async (texts) => texts.map(() => new Float32Array([42])));
  const overrideResult = await embedTexts(['world']);
  assert.equal(overrideResult[0][0], 42);

  clearEmbeddingPipelineCache();
}

run().catch((error) => {
  console.error(error);
  process.exitCode = 1;
});
