import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

import { resolveLoggerConfiguration } from '../src/logger-config.js';

async function run() {
  const tempRoot = fs.mkdtempSync(path.join(os.tmpdir(), 'logger-config-'));

  const invalidDir = path.join(tempRoot, 'not-a-dir');
  fs.writeFileSync(invalidDir, 'blocked');

  const defaultLogDir = path.join(tempRoot, 'default-logs');
  const tmpLogDir = path.join(tempRoot, 'tmp-logs');

  const configWithFallback = resolveLoggerConfiguration(
    { INDEX_MCP_LOG_DIR: invalidDir },
    { defaultLogDirectory: defaultLogDir, tmpLogDirectory: tmpLogDir }
  );

  assert.equal(configWithFallback.fileLoggingEnabled, true, 'expected fallback log directory to be enabled');
  assert.equal(configWithFallback.logDirectory, path.resolve(defaultLogDir));
  assert.equal(configWithFallback.selectedDirectorySource, 'default');

  const diagnosticCodes = configWithFallback.diagnostics.map((diag) => diag.code);
  assert(diagnosticCodes.includes('log_directory_not_directory'));
  assert(diagnosticCodes.includes('log_directory_fallback_applied'));

  const defaultLogDirStats = fs.statSync(defaultLogDir);
  assert(defaultLogDirStats.isDirectory(), 'fallback directory should be created');

  const blockedDefault = path.join(tempRoot, 'blocked-default');
  const blockedTmp = path.join(tempRoot, 'blocked-tmp');
  fs.writeFileSync(blockedDefault, '');
  fs.writeFileSync(blockedTmp, '');

  const configConsoleOnly = resolveLoggerConfiguration(
    { INDEX_MCP_LOG_DIR: invalidDir, LOG_DIR: blockedDefault },
    { defaultLogDirectory: blockedDefault, tmpLogDirectory: blockedTmp }
  );

  assert.equal(configConsoleOnly.fileLoggingEnabled, false);
  assert.equal(configConsoleOnly.logDirectory, null);
  assert.equal(configConsoleOnly.logToConsole, true);

  const consoleDiagCodes = configConsoleOnly.diagnostics.map((diag) => diag.code);
  assert(consoleDiagCodes.includes('log_console_forced'));
  assert(consoleDiagCodes.includes('log_directory_not_directory'));
}

run().catch((error) => {
  console.error(error);
  process.exitCode = 1;
});
