import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import type { ProcessEnv } from 'node:process';

type UnknownRecord = Record<string, unknown>;

const PATH_KEY_PATTERN = /(cwd|workingdir|workingdirectory|workspace|project|root|path|directory)$/i;
const HEADER_CANDIDATES = ['x-mcp-cwd', 'x-mcp-root', 'x-workspace-cwd', 'x-workspace-root'];
const ENV_CANDIDATES = [
  'MCP_CALLER_CWD',
  'MCP_WORKSPACE_ROOT',
  'MCP_WORKSPACE',
  'MCP_PROJECT_ROOT',
  'MCP_PROJECT_PATH',
  'CALLER_CWD',
  'CODEx_WORKSPACE_ROOT',
  'CODEX_WORKSPACE_ROOT',
  'CODEX_CWD'
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
  return candidate.includes('/') || candidate.includes('\\') || candidate.startsWith('file://') || candidate.startsWith('~');
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

function collectFromMeta(meta: UnknownRecord | undefined): string[] {
  if (!meta) {
    return [];
  }
  const visited = new Set<unknown>();
  const candidates: string[] = [];

  const traverse = (value: unknown, depth: number) => {
    if (depth <= 0 || !value || typeof value !== 'object') {
      return;
    }
    if (visited.has(value)) {
      return;
    }
    visited.add(value);
    const entries = Object.entries(value as UnknownRecord);
    for (const [key, entryValue] of entries) {
      if (typeof entryValue === 'string') {
        if (PATH_KEY_PATTERN.test(key) && looksLikePath(entryValue)) {
          const normalized = normalizeCandidate(entryValue);
          if (normalized) {
            candidates.push(normalized);
          }
        }
      } else if (Array.isArray(entryValue)) {
        for (const item of entryValue) {
          if (typeof item === 'string') {
            if (PATH_KEY_PATTERN.test(key) && looksLikePath(item)) {
              const normalized = normalizeCandidate(item);
              if (normalized) {
                candidates.push(normalized);
              }
            }
          } else if (typeof item === 'object' && item !== null) {
            traverse(item, depth - 1);
          }
        }
      } else if (entryValue && typeof entryValue === 'object') {
        traverse(entryValue, depth - 1);
      }
    }
  };

  traverse(meta, 3);
  return candidates;
}

function collectFromHeaders(headers: Record<string, string> | undefined): string[] {
  if (!headers) {
    return [];
  }
  const candidates: string[] = [];
  for (const key of HEADER_CANDIDATES) {
    const value = headers[key];
    if (typeof value === 'string') {
      const normalized = normalizeCandidate(value);
      if (normalized) {
        candidates.push(normalized);
      }
    }
  }
  return candidates;
}

function collectFromEnv(env: ProcessEnv): string[] {
  const candidates: string[] = [];
  for (const key of ENV_CANDIDATES) {
    const value = env[key];
    if (typeof value === 'string') {
      const normalized = normalizeCandidate(value);
      if (normalized) {
        candidates.push(normalized);
      }
    }
  }
  return candidates;
}

function resolveBaseDirectory(context: RootResolutionContext): string | undefined {
  const env = context.env ?? process.env;
  const sources = [
    ...collectFromMeta(context.meta),
    ...collectFromHeaders(context.headers),
    ...collectFromEnv(env)
  ];
  return sources.find(Boolean);
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

export function resolveRootPath(root: string, context: RootResolutionContext = {}): string {
  if (!root) {
    return path.resolve('.');
  }

  const normalizedRoot = normalizeRootValue(root);
  if (path.isAbsolute(normalizedRoot)) {
    return normalizedRoot;
  }

  const base = resolveBaseDirectory(context);
  if (base) {
    return path.resolve(base, normalizedRoot);
  }

  return path.resolve(normalizedRoot);
}
