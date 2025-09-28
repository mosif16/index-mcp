import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

interface ErrnoException extends Error {
  code?: string;
  errno?: number;
  path?: string;
  syscall?: string;
}

export type LogDirectorySource = 'INDEX_MCP_LOG_DIR' | 'LOG_DIR' | 'default' | 'tmp';

export type LoggerDiagnosticCode =
  | 'log_directory_not_directory'
  | 'log_directory_stat_failed'
  | 'log_directory_creation_failed'
  | 'log_directory_unwritable'
  | 'log_directory_fallback_applied'
  | 'log_console_forced';

export interface LoggerSetupDiagnostic {
  readonly level: 'warn' | 'error';
  readonly code: LoggerDiagnosticCode;
  readonly message: string;
  readonly context?: Record<string, unknown>;
}

export interface LoggerConfiguration {
  readonly logDirectory: string | null;
  readonly logFileName: string;
  readonly logLevel: string;
  readonly logToConsole: boolean;
  readonly consoleStreamFd: number;
  readonly fileLoggingEnabled: boolean;
  readonly selectedDirectorySource: LogDirectorySource | null;
  readonly diagnostics: LoggerSetupDiagnostic[];
}

export interface LoggerResolutionOverrides {
  readonly defaultLogDirectory?: string;
  readonly tmpLogDirectory?: string;
}

const LOGGER_PREFIX = '[index-mcp]';

function resolveCandidatePath(value: string): string {
  const trimmed = value.trim();
  return trimmed ? path.resolve(trimmed) : trimmed;
}

function toDiagnosticContext(error: unknown): Record<string, unknown> {
  if (error && typeof error === 'object') {
    const asErrno = error as ErrnoException;
    return {
      error: asErrno.message ?? String(error),
      code: asErrno.code
    };
  }

  return { error: String(error) };
}

function recordStatFailure(
  diagnostics: LoggerSetupDiagnostic[],
  source: LogDirectorySource,
  candidatePath: string,
  error: unknown
) {
  diagnostics.push({
    level: 'error',
    code: 'log_directory_stat_failed',
    message: `${LOGGER_PREFIX} Unable to inspect log directory ${candidatePath}.`,
    context: { path: candidatePath, source, ...toDiagnosticContext(error) }
  });
}

function recordNotDirectory(
  diagnostics: LoggerSetupDiagnostic[],
  source: LogDirectorySource,
  candidatePath: string
) {
  diagnostics.push({
    level: 'error',
    code: 'log_directory_not_directory',
    message: `${LOGGER_PREFIX} Log path ${candidatePath} exists but is not a directory.`,
    context: { path: candidatePath, source }
  });
}

function recordCreationFailure(
  diagnostics: LoggerSetupDiagnostic[],
  source: LogDirectorySource,
  candidatePath: string,
  error: unknown
) {
  diagnostics.push({
    level: 'error',
    code: 'log_directory_creation_failed',
    message: `${LOGGER_PREFIX} Unable to create log directory ${candidatePath}.`,
    context: { path: candidatePath, source, ...toDiagnosticContext(error) }
  });
}

function recordUnwritable(
  diagnostics: LoggerSetupDiagnostic[],
  source: LogDirectorySource,
  candidatePath: string,
  error: unknown
) {
  diagnostics.push({
    level: 'error',
    code: 'log_directory_unwritable',
    message: `${LOGGER_PREFIX} Log directory ${candidatePath} is not writable.`,
    context: { path: candidatePath, source, ...toDiagnosticContext(error) }
  });
}

function prepareDirectory(
  candidatePath: string,
  source: LogDirectorySource,
  diagnostics: LoggerSetupDiagnostic[]
): boolean {
  try {
    const stats = fs.statSync(candidatePath);
    if (!stats.isDirectory()) {
      recordNotDirectory(diagnostics, source, candidatePath);
      return false;
    }
  } catch (error) {
    const maybeErrno = error as ErrnoException;
    if (maybeErrno?.code !== 'ENOENT') {
      recordStatFailure(diagnostics, source, candidatePath, error);
      return false;
    }
  }

  try {
    fs.mkdirSync(candidatePath, { recursive: true });
  } catch (error) {
    recordCreationFailure(diagnostics, source, candidatePath, error);
    return false;
  }

  try {
    fs.accessSync(candidatePath, fs.constants.W_OK);
  } catch (error) {
    recordUnwritable(diagnostics, source, candidatePath, error);
    return false;
  }

  return true;
}

export function resolveLoggerConfiguration(
  env: Record<string, string | undefined>,
  overrides?: LoggerResolutionOverrides
): LoggerConfiguration {
  const diagnostics: LoggerSetupDiagnostic[] = [];

  const logFileName = env.INDEX_MCP_LOG_FILE ?? env.LOG_FILE ?? 'server.log';
  const logLevel = env.INDEX_MCP_LOG_LEVEL ?? env.LOG_LEVEL ?? 'info';
  const requestedConsoleLogging = env.INDEX_MCP_LOG_CONSOLE === 'true';
  const consoleStreamFd = env.INDEX_MCP_LOG_CONSOLE_STREAM === 'stderr' ? 2 : 1;

  const defaultLogDirectory =
    overrides?.defaultLogDirectory ?? path.join(os.homedir(), '.index-mcp', 'logs');
  const tmpLogDirectory = overrides?.tmpLogDirectory ?? path.join(os.tmpdir(), 'index-mcp', 'logs');

  const candidateInputs: Array<{ path?: string; source: LogDirectorySource }> = [
    { path: env.INDEX_MCP_LOG_DIR, source: 'INDEX_MCP_LOG_DIR' },
    { path: env.LOG_DIR, source: 'LOG_DIR' },
    { path: defaultLogDirectory, source: 'default' },
    { path: tmpLogDirectory, source: 'tmp' }
  ];

  const candidates = candidateInputs
    .filter((candidate) => typeof candidate.path === 'string' && candidate.path.trim().length > 0)
    .map((candidate) => ({
      source: candidate.source,
      path: resolveCandidatePath(candidate.path!)
    }));

  let selectedDirectory: { source: LogDirectorySource; path: string } | null = null;
  const failedCustomSources = new Set<LogDirectorySource>();

  for (const candidate of candidates) {
    if (prepareDirectory(candidate.path, candidate.source, diagnostics)) {
      selectedDirectory = candidate;
      break;
    }

    if (candidate.source === 'INDEX_MCP_LOG_DIR' || candidate.source === 'LOG_DIR') {
      failedCustomSources.add(candidate.source);
    }
  }

  const fileLoggingEnabled = Boolean(selectedDirectory);
  const effectiveConsoleLogging = requestedConsoleLogging || !fileLoggingEnabled;

  if (!fileLoggingEnabled) {
    diagnostics.push({
      level: 'error',
      code: 'log_console_forced',
      message:
        `${LOGGER_PREFIX} Falling back to console logging because no writable log directory was found.`,
      context: { attemptedSources: Array.from(failedCustomSources) }
    });
  } else if (
    failedCustomSources.size > 0 &&
    selectedDirectory &&
    selectedDirectory.source !== 'INDEX_MCP_LOG_DIR'
  ) {
    const fallbackDirectory = selectedDirectory;
    diagnostics.push({
      level: 'warn',
      code: 'log_directory_fallback_applied',
      message: `${LOGGER_PREFIX} Using fallback log directory ${fallbackDirectory.path}.`,
      context: {
        fallbackSource: fallbackDirectory.source,
        failedSources: Array.from(failedCustomSources)
      }
    });
  }

  return {
    logDirectory: selectedDirectory?.path ?? null,
    logFileName,
    logLevel,
    logToConsole: effectiveConsoleLogging,
    consoleStreamFd,
    fileLoggingEnabled,
    selectedDirectorySource: selectedDirectory?.source ?? null,
    diagnostics
  };
}
