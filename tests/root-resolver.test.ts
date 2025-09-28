import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

import { resolveRootPath } from '../src/root-resolver.js';

async function run() {
  const scratch = fs.mkdtempSync(path.join(os.tmpdir(), 'index-mcp-root-'));

  const metaBase = path.join(scratch, 'meta-workspace');
  fs.mkdirSync(metaBase, { recursive: true });
  const metaResolved = resolveRootPath('.', { meta: { cwd: metaBase } });
  assert.equal(metaResolved, metaBase);

  const nestedBase = path.join(scratch, 'nested-workspace');
  fs.mkdirSync(path.join(nestedBase, 'docs'), { recursive: true });
  const nestedResolved = resolveRootPath('docs', {
    meta: {
      workspace: {
        path: nestedBase
      }
    }
  });
  assert.equal(nestedResolved, path.join(nestedBase, 'docs'));

  const headerBase = path.join(scratch, 'header-workspace');
  fs.mkdirSync(headerBase, { recursive: true });
  const headerResolved = resolveRootPath('.', {
    headers: {
      'x-mcp-cwd': `file://${headerBase}`
    }
  });
  assert.equal(headerResolved, headerBase);

  const envBase = path.join(scratch, 'env-workspace');
  fs.mkdirSync(envBase, { recursive: true });
  const envResolved = resolveRootPath('.', {
    env: { ...process.env, MCP_CALLER_CWD: envBase }
  });
  assert.equal(envResolved, envBase);

  const fileUrl = path.join(scratch, 'direct-url');
  fs.mkdirSync(fileUrl, { recursive: true });
  const directResolved = resolveRootPath(`file://${fileUrl}`);
  assert.equal(directResolved, fileUrl);

  const cwdResolved = resolveRootPath(undefined, {});
  assert.equal(cwdResolved, path.resolve('.'));
}

run()
  .catch((error) => {
    console.error(error);
    process.exitCode = 1;
  });
