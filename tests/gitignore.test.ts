import assert from 'node:assert/strict';
import os from 'node:os';
import path from 'node:path';
import { promises as fs } from 'node:fs';

import { ingestCodebase } from '../src/ingest.js';

import Database from 'better-sqlite3';

async function createTempWorkspace(prefix: string): Promise<string> {
  const workspace = await fs.mkdtemp(path.join(os.tmpdir(), prefix));
  return workspace;
}

async function run() {
  const workspace = await createTempWorkspace('gitignore-ingest-');
  const databaseName = 'test-index.sqlite';

  const rootGitIgnore = ['*.log', '!keep.log', 'logs/'].join('\n');
  await fs.writeFile(path.join(workspace, '.gitignore'), `${rootGitIgnore}\n`);

  const srcDir = path.join(workspace, 'src');
  await fs.mkdir(srcDir, { recursive: true });

  const nestedGitIgnore = ['*.tmp', '!special.tmp', 'build/'].join('\n');
  await fs.writeFile(path.join(srcDir, '.gitignore'), `${nestedGitIgnore}\n`);

  await fs.writeFile(path.join(workspace, 'README.md'), '# sample\n');
  await fs.writeFile(path.join(workspace, 'keep.log'), 'keep me\n');
  await fs.writeFile(path.join(workspace, 'app.log'), 'ignore me\n');

  const logsDir = path.join(workspace, 'logs');
  await fs.mkdir(logsDir, { recursive: true });
  await fs.writeFile(path.join(logsDir, 'trace.log'), 'should be ignored\n');

  await fs.writeFile(path.join(srcDir, 'included.ts'), 'console.log("included");\n');
  await fs.writeFile(path.join(srcDir, 'ignored.tmp'), 'ignore\n');
  await fs.writeFile(path.join(srcDir, 'special.tmp'), 'bring back\n');

  const nestedDir = path.join(srcDir, 'nested');
  await fs.mkdir(nestedDir, { recursive: true });
  await fs.writeFile(path.join(nestedDir, 'app.tmp'), 'still ignored\n');

  const buildDir = path.join(srcDir, 'build');
  await fs.mkdir(buildDir, { recursive: true });
  await fs.writeFile(path.join(buildDir, 'output.js'), 'ignored build\n');

  const result = await ingestCodebase({
    root: workspace,
    databaseName,
    storeFileContent: false,
    embedding: { enabled: false },
    graph: { enabled: false }
  });

  assert.equal(result.root, workspace);

  const db = new Database(path.join(workspace, databaseName));
  const rows = db.prepare('SELECT path FROM files ORDER BY path').all() as { path: string }[];
  db.close();

  const paths = rows.map((row) => row.path);

  assert.deepEqual(paths, [
    'README.md',
    'keep.log',
    'src/included.ts',
    'src/special.tmp'
  ]);
}

run()
  .catch((error) => {
    console.error(error);
    process.exitCode = 1;
  });
