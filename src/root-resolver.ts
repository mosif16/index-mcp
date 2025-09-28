import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

type UnknownRecord = Record<string, unknown>;

type ProcessEnv = Record<string, string | undefined>;

const PATH_KEY_PATTERN = /(cwd|workingdir|workingdirectory|workspace|project|root|path|directory)$/i;
const URI_KEY_PATTERN = /(uri|url)$/i;
const HEADER_CANDIDATES = [
  'x-mcp-cwd',
  'x-mcp-root',
  'x-mcp-root-uri',
  'x-workspace-cwd',
  'x-workspace-root',
  'x-workspace-root-uri',
  'x-codex-cwd',
  'x-codex-root'
];
const ENV_CANDIDATES = [
  'MCP_CALLER_CWD',
  'MCP_WORKSPACE_ROOT',
  'MCP_WORKSPACE',
  'MCP_PROJECT_ROOT',
  'MCP_PROJECT_PATH',
  'CALLER_CWD',
  'CODEx_WORKSPACE_ROOT',
  'CODEX_WORKSPACE_ROOT',
  'CODEX_CWD',
  'PWD',
  'INIT_CWD',
  'ORIGINAL_PWD',
  'PROJECT_ROOT',
  'PROJECT_PATH',
  'WORKSPACE_ROOT',
  'WORKSPACE_PATH',
  'WORKSPACE_DIR',
  'GITHUB_WORKSPACE'
];

export interface RootResolutionContext {
  meta?: UnknownRecord;
  headers?: Record<string, string>;
  env?: ProcessEnv;
}

function expandHome(input: string): string {
  if (input === '~') {
    return os.homedir();
  }
  if (input.startsWith('~/')) {
    return path.join(os.homedir(), input.slice(2));
  }
  return input;
}

function looksLikePath(candidate: string): boolean {
  return (
    candidate.includes('/') ||
    candidate.includes('\\') ||
    candidate.startsWith('file://') ||
    candidate.startsWith('~')
  );
}

function normalizeCandidate(candidate: string): string | undefined {
  if (!candidate) {
    return undefined;
  }
  let normalized = candidate.trim();
  if (!normalized) {
    return undefined;
  }
  if (normalized.startsWith('file://')) {
    try {
      normalized = fileURLToPath(normalized);
    } catch {
      return undefined;
    }
  }
  normalized = expandHome(normalized);
  if (path.isAbsolute(normalized)) {
    return path.normalize(normalized);
  }
  return undefined;
}

function expandScalarList(value: string): string[] {
  const trimmed = value.trim();
  if (!trimmed) {
    return [];
  }

  try {
    const parsed = JSON.parse(trimmed);
    if (Array.isArray(parsed)) {
      return parsed
        .filter((item): item is string => typeof item === 'string' && item.trim() !== '')
        .map((item) => item.trim());
    }
  } catch {
    // fall back to delimiter parsing
  }

  return trimmed
    .split(/[\n;,]/)
    .map((segment) => segment.trim())
    .filter(Boolean);
}

function collectFromMeta(meta: UnknownRecord | undefined): string[] {
  if (!meta) {
    return [];
  }

  const visited = new Set<unknown>();
  const results = new Set<string>();

  const pushCandidate = (value: string | undefined) => {
    if (value) {
      results.add(value);
    }
  };

  const traverse = (value: unknown, depth: number) => {
    if (depth <= 0 || value === null || typeof value !== 'object') {
      return;
    }
    if (visited.has(value)) {
      return;
    }
    visited.add(value);

    const entries = Object.entries(value as UnknownRecord);
    for (const [key, entryValue] of entries) {
      if (typeof entryValue === 'string') {
        const keyMatches = PATH_KEY_PATTERN.test(key);
        const uriHint = URI_KEY_PATTERN.test(key);
        const shouldAttempt =
          keyMatches ||
          (uriHint && entryValue.startsWith('file://')) ||
          (!keyMatches && !uriHint && looksLikePath(entryValue));

        if (shouldAttempt) {
          pushCandidate(normalizeCandidate(entryValue));
        }
      } else if (Array.isArray(entryValue)) {
        for (const item of entryValue) {
          if (typeof item === 'string') {
            if (PATH_KEY_PATTERN.test(key) || looksLikePath(item)) {
              pushCandidate(normalizeCandidate(item));
            }
          } else if (item && typeof item === 'object') {
            traverse(item, depth - 1);
          }
        }
      } else if (entryValue && typeof entryValue === 'object') {
        traverse(entryValue, depth - 1);
      }
    }
  };

  traverse(meta, 3);
  return Array.from(results);
}

function collectFromHeaders(headers: Record<string, string> | undefined): string[] {
  if (!headers) {
    return [];
  }
  const results = new Set<string>();
  for (const key of HEADER_CANDIDATES) {
    const value = headers[key];
    if (typeof value === 'string') {
      for (const candidate of expandScalarList(value)) {
        const normalized = normalizeCandidate(candidate);
        if (normalized) {
          results.add(normalized);
        }
      }
    }
  }
  return Array.from(results);
}

function collectFromEnv(env: ProcessEnv): string[] {
  const results = new Set<string>();
  for (const key of ENV_CANDIDATES) {
    const value = env[key];
    if (typeof value === 'string') {
      for (const candidate of expandScalarList(value)) {
        const normalized = normalizeCandidate(candidate);
        if (normalized) {
          results.add(normalized);
        }
      }
    }
  }
  return Array.from(results);
}

function directoryExists(candidate: string): boolean {
  try {
    return fs.statSync(candidate).isDirectory();
  } catch {
    return false;
  }
}

function resolveBaseDirectory(context: RootResolutionContext): string | undefined {
  const env = context.env ?? process.env;
  const candidates = new Set<string>([
    ...collectFromMeta(context.meta),
    ...collectFromHeaders(context.headers),
    ...collectFromEnv(env)
  ]);

  candidates.add(process.cwd());

  for (const candidate of candidates) {
    if (candidate && directoryExists(candidate)) {
      return candidate;
    }
  }

  return undefined;
}

function normalizeRootValue(value: string): string {
  if (value.startsWith('file://')) {
    try {
      return path.normalize(fileURLToPath(value));
    } catch {
      // fall through
    }
  }
  const expanded = expandHome(value);
  return path.normalize(expanded);
}

export function resolveRootPath(root: string | undefined, context: RootResolutionContext = {}): string {
  const provided = typeof root === 'string' ? root.trim() : '';

  if (!provided) {
    const base = resolveBaseDirectory(context);
    if (base) {
      return base;
    }

    const fallback = path.resolve('.');
    if (directoryExists(fallback)) {
      return fallback;
    }

    throw new Error(
      '[index-mcp] Unable to determine a workspace directory. Provide a root argument or pass workspace metadata via headers/environment.'
    );
  }

  const normalizedRoot = normalizeRootValue(provided);
  if (path.isAbsolute(normalizedRoot)) {
    if (!directoryExists(normalizedRoot)) {
      throw new Error(
        `[index-mcp] Workspace path '${normalizedRoot}' does not exist or is not accessible. Provide a valid root or configure workspace metadata.`
      );
    }
    return normalizedRoot;
  }

  const base = resolveBaseDirectory(context);
  const resolved = base ? path.resolve(base, normalizedRoot) : path.resolve(normalizedRoot);

  if (!directoryExists(resolved)) {
    throw new Error(
      `[index-mcp] Workspace path '${resolved}' does not exist or is not accessible. Provide a valid root or configure workspace metadata.`
    );
  }

  return resolved;
}
