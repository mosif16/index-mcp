import assert from 'node:assert/strict';
import os from 'node:os';
import path from 'node:path';
import { promises as fs } from 'node:fs';

import { ingestCodebase } from '../src/ingest.js';
import { getContextBundle } from '../src/context-bundle.js';

async function setupWorkspace(): Promise<{ root: string; databaseName: string }> {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), 'context-bundle-'));
  const srcDir = path.join(root, 'src');
  await fs.mkdir(srcDir, { recursive: true });
  const modulePath = path.join(srcDir, 'module.ts');
  await fs.writeFile(
    modulePath,
    [
      'export function foo(value: number) {',
      '  bar(value + 1);',
      '  return value * 2;',
      '}',
      '',
      'export function bar(next: number) {',
      '  console.log(next);',
      '}',
      ''
    ].join('\n')
  );
  return { root, databaseName: 'context-bundle.sqlite' };
}

async function run() {
  const { root, databaseName } = await setupWorkspace();

  await ingestCodebase({
    root,
    databaseName,
    // Disable embeddings so the bundle must fall back to stored content snippets.
    embedding: { enabled: false },
    graph: { enabled: true }
  });

  const bundle = await getContextBundle({
    root,
    databaseName,
    file: 'src/module.ts',
    symbol: { name: 'foo', kind: 'function' },
    maxSnippets: 2,
    maxNeighbors: 6
  });

  assert.equal(bundle.file.path, 'src/module.ts');
  assert(bundle.definitions.length > 0);
  assert(bundle.snippets.length > 0);
  assert.equal(bundle.snippets[0]?.source, 'content');
  assert(bundle.focusDefinition, 'Expected symbol alias to resolve focus definition');
  assert.equal(bundle.focusDefinition?.name, 'foo');
  assert.equal(bundle.focusDefinition?.kind, 'function');
  assert(bundle.related.length >= 1, 'Expected at least one related graph edge');
  assert(bundle.related.some((edge) => edge.neighbor.name === 'bar'));
  assert.equal(bundle.warnings.length, 0);
  assert(bundle.latestIngestion, 'Expected latest ingestion metadata to be populated');
}

run().catch((error) => {
  console.error(error);
  process.exitCode = 1;
});
