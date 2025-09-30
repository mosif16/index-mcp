import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

interface ErrnoException extends Error {
  code?: string;
}

export type EnvironmentDiagnosticCode =
  | 'model_cache_stat_failed'
  | 'model_cache_not_directory'
  | 'model_cache_creation_failed'
  | 'model_cache_unwritable'
  | 'model_cache_unavailable';

export interface EnvironmentDiagnostic {
  readonly level: 'warn' | 'error';
  readonly code: EnvironmentDiagnosticCode;
  readonly message: string;
  readonly context?: Record<string, unknown>;
}

export type ModelCacheSource =
  | 'FASTEMBED_CACHE_DIR'
  | 'INDEX_MCP_MODEL_CACHE_DIR'
  | 'default'
  | 'tmp';

interface DirectoryCandidate {
  readonly path: string;
  readonly source: ModelCacheSource;
}

const diagnostics: EnvironmentDiagnostic[] = [];
const recordedFailures = new Set<string>();

let modelCache: { directory: string; source: ModelCacheSource } | null = null;

function toAbsolutePath(candidate: string): string {
  return path.isAbsolute(candidate) ? candidate : path.resolve(candidate);
}

function recordDiagnostic(diagnostic: EnvironmentDiagnostic): void {
  const key = [diagnostic.code, diagnostic.context?.source, diagnostic.context?.path]
    .filter(Boolean)
    .join('::');

  if (diagnostic.level === 'error' && key) {
    if (recordedFailures.has(key)) {
      return;
    }
    recordedFailures.add(key);
  }

  diagnostics.push(diagnostic);
}

function toDiagnosticContext(candidate: DirectoryCandidate, error: unknown): Record<string, unknown> {
  const context: Record<string, unknown> = {
    source: candidate.source,
    path: candidate.path
  };

  if (error && typeof error === 'object') {
    const err = error as ErrnoException;
    if (typeof err.code === 'string') {
      context.code = err.code;
    }
    if (typeof err.message === 'string') {
      context.error = err.message;
    }
  } else if (error) {
    context.error = String(error);
  }

  return context;
}

function prepareDirectory(candidate: DirectoryCandidate): boolean {
  try {
    const stats = fs.statSync(candidate.path);
    if (!stats.isDirectory()) {
      recordDiagnostic({
        level: 'error',
        code: 'model_cache_not_directory',
        message: `[index-mcp] Model cache path ${candidate.path} exists but is not a directory.`,
        context: { source: candidate.source, path: candidate.path }
      });
      return false;
    }
  } catch (error) {
    const maybeErrno = error as ErrnoException;
    if (maybeErrno?.code && maybeErrno.code !== 'ENOENT') {
      recordDiagnostic({
        level: 'error',
        code: 'model_cache_stat_failed',
        message: `[index-mcp] Unable to inspect model cache directory ${candidate.path}.`,
        context: toDiagnosticContext(candidate, error)
      });
      return false;
    }
  }

  try {
    fs.mkdirSync(candidate.path, { recursive: true });
  } catch (error) {
    recordDiagnostic({
      level: 'error',
      code: 'model_cache_creation_failed',
      message: `[index-mcp] Unable to create model cache directory ${candidate.path}.`,
      context: toDiagnosticContext(candidate, error)
    });
    return false;
  }

  try {
    fs.accessSync(candidate.path, fs.constants.W_OK);
  } catch (error) {
    recordDiagnostic({
      level: 'error',
      code: 'model_cache_unwritable',
      message: `[index-mcp] Model cache directory ${candidate.path} is not writable.`,
      context: toDiagnosticContext(candidate, error)
    });
    return false;
  }

  return true;
}

function getDefaultCandidates(): DirectoryCandidate[] {
  const candidates: DirectoryCandidate[] = [];
  const envPath = process.env.INDEX_MCP_MODEL_CACHE_DIR?.trim();
  if (envPath) {
    candidates.push({
      source: 'INDEX_MCP_MODEL_CACHE_DIR',
      path: toAbsolutePath(envPath)
    });
  }

  const homeDir = os.homedir();
  if (homeDir && homeDir.trim()) {
    candidates.push({
      source: 'default',
      path: path.join(homeDir, '.index-mcp', 'models')
    });
  }

  candidates.push({
    source: 'tmp',
    path: path.join(os.tmpdir(), 'index-mcp', 'models')
  });

  return candidates;
}

function setResolvedModelCache(candidate: DirectoryCandidate): void {
  modelCache = {
    directory: candidate.path,
    source: candidate.source
  };
  process.env.FASTEMBED_CACHE_DIR = candidate.path;
  if (!process.env.INDEX_MCP_MODEL_CACHE_DIR) {
    process.env.INDEX_MCP_MODEL_CACHE_DIR = candidate.path;
  }
}

export function ensureModelCacheDirectory(): void {
  if (modelCache) {
    return;
  }

  const fastembedEnv = process.env.FASTEMBED_CACHE_DIR?.trim();
  if (fastembedEnv) {
    const candidate: DirectoryCandidate = {
      source: 'FASTEMBED_CACHE_DIR',
      path: toAbsolutePath(fastembedEnv)
    };

    if (prepareDirectory(candidate)) {
      setResolvedModelCache(candidate);
      return;
    }

    // Fall through to defaults if the explicit FASTEMBED cache directory is unusable.
  }

  const candidates = getDefaultCandidates();

  for (const candidate of candidates) {
    if (prepareDirectory(candidate)) {
      setResolvedModelCache(candidate);
      return;
    }
  }

  recordDiagnostic({
    level: 'error',
    code: 'model_cache_unavailable',
    message:
      '[index-mcp] Unable to configure a writable embedding model cache directory. The embedding pipeline may create per-process caches in the working directory.',
    context: { candidates: candidates.map((candidate) => candidate.path) }
  });
}

export function getModelCacheConfiguration(): { directory: string | null; source: ModelCacheSource | null } {
  ensureModelCacheDirectory();
  return modelCache
    ? { directory: modelCache.directory, source: modelCache.source }
    : { directory: null, source: null };
}

export function getEnvironmentDiagnostics(): EnvironmentDiagnostic[] {
  return diagnostics.map((diagnostic) => ({ ...diagnostic }));
}

export function getBudgetTokens(): number {
  const budgetEnv = process.env.INDEX_MCP_BUDGET_TOKENS?.trim();
  if (budgetEnv) {
    const parsed = parseInt(budgetEnv, 10);
    if (!isNaN(parsed) && parsed > 0) {
      return parsed;
    }
  }
  return 3000; // Default budget
}
