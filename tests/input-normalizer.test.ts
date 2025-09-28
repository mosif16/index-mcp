import assert from 'node:assert/strict';

import { normalizeLookupArgs } from '../src/input-normalizer.js';

async function run() {
  const searchInput = normalizeLookupArgs({
    path: '.',
    intent: 'SEARCH',
    search_query: ' find createUser ',
    max_results: '5',
    symbol_selector: 'FooBar',
    graph_target: { identifier: 'Thing', type: 'class' }
  });

  assert.equal(searchInput.root, '.');
  assert.equal(searchInput.mode, 'search');
  assert.equal(searchInput.query, 'find createUser');
  assert.equal(searchInput.limit, 5);
  assert.deepEqual(searchInput.symbol, { name: 'FooBar' });
  assert.deepEqual(searchInput.node, { name: 'Thing', kind: 'class' });

  const bundleInput = normalizeLookupArgs({
    root: '.',
    file_path: ' src/service.ts ',
    focus_symbol: { identifier: 'handler', type: 'function', file_path: 'src/service.ts' },
    neighbor_limit: '9'
  });

  assert.equal(bundleInput.file, 'src/service.ts');
  assert.equal(bundleInput.maxNeighbors, 9);
  assert.deepEqual(bundleInput.symbol, {
    name: 'handler',
    kind: 'function',
    path: 'src/service.ts'
  });
}

run().catch((error) => {
  console.error(error);
  process.exitCode = 1;
});
