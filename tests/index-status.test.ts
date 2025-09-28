import assert from 'node:assert/strict';
import os from 'node:os';
import path from 'node:path';
import { promises as fs } from 'node:fs';

import { ingestCodebase } from '../src/ingest.js';
import { getIndexStatus } from '../src/status.js';

async function createWorkspace(): Promise<string> {
  return fs.mkdtemp(path.join(os.tmpdir(), 'index-status-'));
}

async function run() {
  const workspace = await createWorkspace();
  const databaseName = 'index-status.sqlite';
  await fs.writeFile(path.join(workspace, 'README.md'), '# status test\n');

  const initialStatus = await getIndexStatus({ root: workspace, databaseName });
  assert.equal(initialStatus.databaseExists, false);
  assert.equal(initialStatus.totalFiles, 0);
  assert.equal(initialStatus.recentIngestions.length, 0);

  await ingestCodebase({
    root: workspace,
    databaseName,
    storeFileContent: false,
    embedding: { enabled: false },
    graph: { enabled: false }
  });

  const firstStatus = await getIndexStatus({ root: workspace, databaseName });
  assert.equal(firstStatus.databaseExists, true);
  assert.ok(firstStatus.databaseSizeBytes && firstStatus.databaseSizeBytes > 0);
  assert.equal(firstStatus.totalFiles, 1);
  assert.equal(firstStatus.totalChunks, 0);
  assert.deepEqual(firstStatus.embeddingModels, []);
  assert.equal(firstStatus.totalGraphNodes, 0);
  assert.equal(firstStatus.totalGraphEdges, 0);
  assert.ok(firstStatus.latestIngestion);
  assert.equal(firstStatus.latestIngestion?.fileCount, 1);
  assert.equal(firstStatus.recentIngestions.length, 1);

  const srcDir = path.join(workspace, 'src');
  await fs.mkdir(srcDir, { recursive: true });
  await fs.writeFile(path.join(srcDir, 'main.ts'), 'console.log("hello");\n');

  await ingestCodebase({
    root: workspace,
    databaseName,
    storeFileContent: false,
    embedding: { enabled: false },
    graph: { enabled: false }
  });

  const secondStatus = await getIndexStatus({ root: workspace, databaseName, historyLimit: 2 });
  assert.equal(secondStatus.databaseExists, true);
  assert.equal(secondStatus.totalFiles, 2);
  assert.equal(secondStatus.recentIngestions.length, 2);
  assert.ok(secondStatus.latestIngestion);
  assert.equal(secondStatus.latestIngestion?.fileCount, 2);
  assert.equal(secondStatus.recentIngestions[1]?.fileCount, 1);
}

run().catch((error) => {
  console.error(error);
  process.exitCode = 1;
});
