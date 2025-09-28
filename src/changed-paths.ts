import path from 'node:path';

import type { RootResolutionContext } from './root-resolver.js';

type UnknownRecord = Record<string, unknown>;

const HEADER_KEYS = ['x-mcp-changed-paths', 'x-workspace-changed-paths', 'x-codex-changed-paths'];
const ENV_KEYS = [
  'MCP_CHANGED_PATHS',
  'MCP_TARGET_PATHS',
  'MCP_CHANGED_FILES',
  'WORKSPACE_CHANGED_PATHS',
  'CODEX_CHANGED_PATHS',
  'CHANGED_FILES'
];
const META_KEY_PATTERN = /(change|modified|staged|unstaged|diff|files?|paths?)/i;
const EMBEDDED_PATH_KEYS = ['path', 'relativePath', 'file', 'filePath'];

function parseStringList(value: string): string[] {
  const trimmed = value.trim();
  if (!trimmed) {
    return [];
  }

  try {
    const parsed = JSON.parse(trimmed);
    if (Array.isArray(parsed)) {
      return parsed.filter((item): item is string => typeof item === 'string' && item.trim() !== '');
    }
  } catch {
    // ignore – treat as plain text fallback
  }

  if (trimmed.includes('\n')) {
    return trimmed
      .split(/\r?\n/)
      .map((segment) => segment.trim())
      .filter(Boolean);
  }

  if (trimmed.includes(';')) {
    return trimmed
      .split(';')
      .map((segment) => segment.trim())
      .filter(Boolean);
  }

  return [trimmed];
}

function collectFromHeaders(headers: Record<string, string> | undefined): string[] {
  if (!headers) {
    return [];
  }

  const results: string[] = [];
  for (const key of HEADER_KEYS) {
    const value = headers[key];
    if (typeof value === 'string') {
      results.push(...parseStringList(value));
    }
  }
  return results;
}

function collectFromEnv(env: Record<string, string | undefined> | undefined): string[] {
  const source = env ?? process.env;
  const results: string[] = [];

  for (const key of ENV_KEYS) {
    const value = source[key];
    if (typeof value === 'string') {
      results.push(...parseStringList(value));
    }
  }

  return results;
}

function extractFromRecord(record: UnknownRecord): string[] {
  const results: string[] = [];
  for (const key of EMBEDDED_PATH_KEYS) {
    const candidate = record[key];
    if (typeof candidate === 'string' && candidate.trim() !== '') {
      results.push(candidate);
    }
  }
  return results;
}

function collectFromMeta(meta: UnknownRecord | undefined, depth = 4): string[] {
  if (!meta || depth <= 0) {
    return [];
  }

  const seen = new Set<unknown>();
  const results: string[] = [];

  const visit = (value: unknown, keyHint: string | undefined, remainingDepth: number) => {
    if (remainingDepth < 0 || value === null || value === undefined) {
      return;
    }

    if (typeof value === 'string') {
      if (!keyHint || META_KEY_PATTERN.test(keyHint)) {
        results.push(...parseStringList(value));
      }
      return;
    }

    if (Array.isArray(value)) {
      for (const item of value) {
        visit(item, keyHint, remainingDepth - 1);
      }
      return;
    }

    if (typeof value === 'object') {
      if (seen.has(value)) {
        return;
      }
      seen.add(value);

      const record = value as UnknownRecord;
      if (keyHint && META_KEY_PATTERN.test(keyHint)) {
        results.push(...extractFromRecord(record));
      }

      for (const [childKey, childValue] of Object.entries(record)) {
        const nextKey = typeof childKey === 'string' ? childKey : undefined;
        visit(childValue, nextKey, remainingDepth - 1);
      }
    }
  };

  visit(meta, undefined, depth);
  return results;
}

function normalizeCandidates(root: string, candidates: string[]): string[] {
  if (!candidates.length) {
    return [];
  }

  const absoluteRoot = path.resolve(root);
  const normalizedRoot = absoluteRoot.endsWith(path.sep) ? absoluteRoot : `${absoluteRoot}${path.sep}`;
  const results = new Set<string>();

  for (const raw of candidates) {
    if (typeof raw !== 'string') {
      continue;
    }
    const trimmed = raw.trim().replace(/^['"]|['"]$/g, '');
    if (!trimmed) {
      continue;
    }

    const absoluteCandidate = path.isAbsolute(trimmed)
      ? path.normalize(trimmed)
      : path.resolve(absoluteRoot, trimmed);

    if (
      absoluteCandidate !== absoluteRoot &&
      !absoluteCandidate.startsWith(normalizedRoot)
    ) {
      continue;
    }

    const relative = path.relative(absoluteRoot, absoluteCandidate);
    if (!relative) {
      continue;
    }
    const posixRelative = relative.split(path.sep).join('/');
    results.add(posixRelative);
  }

  return Array.from(results);
}

function sanitizeProvidedPaths(paths: string[] | undefined): string[] {
  if (!paths || paths.length === 0) {
    return [];
  }
  const normalized = new Set<string>();
  for (const entry of paths) {
    if (typeof entry !== 'string') {
      continue;
    }
    const trimmed = entry.trim();
    if (!trimmed) {
      continue;
    }
    normalized.add(trimmed);
  }
  return Array.from(normalized);
}

export function resolveIngestPaths(
  root: string,
  context: RootResolutionContext,
  providedPaths?: string[]
): string[] {
  const sanitizedProvided = sanitizeProvidedPaths(providedPaths);
  if (sanitizedProvided.length > 0) {
    return sanitizedProvided;
  }

  const metaPaths = collectFromMeta(context.meta as UnknownRecord | undefined);
  const headerPaths = collectFromHeaders(context.headers);
  const envPaths = collectFromEnv(context.env);

  const normalized = normalizeCandidates(root, [...metaPaths, ...headerPaths, ...envPaths]);
  return normalized;
}
