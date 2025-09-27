import assert from 'node:assert/strict';
import path from 'node:path';

import { resolveRootPath } from '../src/root-resolver.js';

async function run() {
  const metaBase = path.join(path.sep, 'tmp', 'meta-workspace');
  const metaResolved = resolveRootPath('.', { meta: { cwd: metaBase } });
  assert.equal(metaResolved, metaBase);

  const nestedBase = path.join(path.sep, 'tmp', 'nested-workspace');
  const nestedResolved = resolveRootPath('docs', {
    meta: {
      workspace: {
        path: nestedBase
      }
    }
  });
  assert.equal(nestedResolved, path.join(nestedBase, 'docs'));

  const headerBase = path.join(path.sep, 'tmp', 'header-workspace');
  const headerResolved = resolveRootPath('.', {
    headers: {
      'x-mcp-cwd': `file://${headerBase}`
    }
  });
  assert.equal(headerResolved, headerBase);

  const envBase = path.join(path.sep, 'tmp', 'env-workspace');
  const envResolved = resolveRootPath('.', {
    env: { ...process.env, MCP_CALLER_CWD: envBase }
  });
  assert.equal(envResolved, envBase);

  const fileUrl = path.join(path.sep, 'tmp', 'direct-url');
  const directResolved = resolveRootPath(`file://${fileUrl}`);
  assert.equal(directResolved, fileUrl);
}

run()
  .catch((error) => {
    console.error(error);
    process.exitCode = 1;
  });

