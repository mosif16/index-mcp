import assert from 'node:assert/strict';
import path from 'node:path';

import { resolveIngestPaths } from '../src/changed-paths.js';
import type { RootResolutionContext } from '../src/root-resolver.js';

async function run() {
  const root = path.join(path.sep, 'tmp', 'repo-test');

  // Provided paths take precedence and are sanitized.
  const provided = resolveIngestPaths(root, {}, [' src/index.ts ', 'src/index.ts', '']);
  assert.deepEqual(provided, ['src/index.ts']);

  // Environment derived paths are normalized relative to the root.
  const envContext: RootResolutionContext = {
    env: { MCP_CHANGED_PATHS: 'src/a.ts\nlib/b.ts' }
  };
  const envPaths = resolveIngestPaths(root, envContext);
  assert.deepEqual(envPaths.sort(), ['lib/b.ts', 'src/a.ts']);

  // Meta objects with absolute paths are trimmed to the workspace and deduplicated.
  const absoluteFile = path.join(root, 'lib', 'native.rs');
  const metaContext: RootResolutionContext = {
    meta: {
      workspace: {
        diff: {
          changedFiles: [absoluteFile, '/etc/passwd']
        }
      }
    }
  };
  const metaPaths = resolveIngestPaths(root, metaContext);
  assert.deepEqual(metaPaths, ['lib/native.rs']);

  // Headers contribute when no other signals are present.
  const headerContext: RootResolutionContext = {
    headers: {
      'x-mcp-changed-paths': 'docs/README.md;src/server.ts'
    }
  };
  const headerPaths = resolveIngestPaths(root, headerContext);
  assert.deepEqual(headerPaths.sort(), ['docs/README.md', 'src/server.ts']);
}

run().catch((error) => {
  console.error(error);
  process.exitCode = 1;
});
