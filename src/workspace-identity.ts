import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';

import type { RootResolutionContext } from './root-resolver.js';
import { createLogger } from './logger.js';

interface UnknownRecord {
  [key: string]: unknown;
}

interface ProcessEnv {
  [key: string]: string | undefined;
}

export interface WorkspaceIdentityComponent {
  readonly value: string;
  readonly source: string;
}

export interface WorkspaceIdentity {
  readonly components: WorkspaceIdentityComponent[];
}

const HEADER_IDENTITY_KEYS = [
  'x-workspace-id',
  'x-workspace-slug',
  'x-workspace-name',
  'x-repo-id',
  'x-repo-slug',
  'x-repo-name',
  'x-repo-url',
  'x-repository',
  'x-repository-id',
  'x-repository-name',
  'x-repository-url',
  'x-github-repository',
  'x-project-id',
  'x-project-key',
  'x-project-path'
];

const ENV_IDENTITY_KEYS = [
  'MCP_WORKSPACE_ID',
  'MCP_WORKSPACE_SLUG',
  'MCP_WORKSPACE_NAME',
  'MCP_PROJECT_ID',
  'MCP_PROJECT_SLUG',
  'WORKSPACE_ID',
  'WORKSPACE_SLUG',
  'WORKSPACE_NAME',
  'PROJECT_ID',
  'PROJECT_SLUG',
  'PROJECT_KEY',
  'PROJECT_PATH',
  'REPOSITORY',
  'REPOSITORY_ID',
  'REPOSITORY_NAME',
  'REPOSITORY_SLUG',
  'REPOSITORY_URL',
  'GITHUB_REPOSITORY',
  'GIT_REPOSITORY',
  'BITBUCKET_REPO_SLUG',
  'CI_PROJECT_ID',
  'CI_PROJECT_PATH',
  'CI_REPOSITORY_URL'
];

const META_IDENTITY_KEY_PATTERN = /(workspace|repo|repository|project)(?:[_-]?(id|slug|name|url|path))?$/i;
const MAX_COMPONENT_LENGTH = 512;
const gitRemoteCache = new Map<string, string | null>();

const log = createLogger('workspace-identity');

function sanitizeValue(value: string | undefined): string | undefined {
  if (typeof value !== 'string') {
    return undefined;
  }
  const trimmed = value.trim();
  if (!trimmed || trimmed.length > MAX_COMPONENT_LENGTH) {
    return undefined;
  }
  return trimmed;
}

function addComponent(
  seen: Set<string>,
  components: WorkspaceIdentityComponent[],
  source: string,
  value: string | undefined
) {
  const sanitized = sanitizeValue(value);
  if (!sanitized) {
    return;
  }
  const key = `${source}::${sanitized}`;
  if (seen.has(key)) {
    return;
  }
  seen.add(key);
  components.push({ source, value: sanitized });
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
        .filter((item): item is string => typeof item === 'string' && item.trim().length > 0)
        .map((item) => item.trim());
    }
  } catch {
    // Fall through to delimiter parsing.
  }

  return trimmed
    .split(/[\n;,]/)
    .map((segment) => segment.trim())
    .filter((segment) => segment.length > 0);
}

function collectFromHeaders(headers: Record<string, string> | undefined): string[] {
  if (!headers) {
    return [];
  }
  const results: string[] = [];
  for (const key of HEADER_IDENTITY_KEYS) {
    const value = headers[key];
    if (typeof value === 'string' && value) {
      results.push(...expandScalarList(value));
    }
  }
  return results;
}

function collectFromEnv(env: ProcessEnv | undefined): string[] {
  const source = env ?? process.env;
  const results: string[] = [];
  for (const key of ENV_IDENTITY_KEYS) {
    const value = source[key];
    if (typeof value === 'string' && value) {
      results.push(...expandScalarList(value));
    }
  }
  return results;
}

function collectFromMeta(meta: UnknownRecord | undefined): string[] {
  if (!meta) {
    return [];
  }

  const visited = new Set<unknown>();
  const results = new Set<string>();

  const pushCandidate = (value: unknown) => {
    if (typeof value === 'string') {
      const sanitized = sanitizeValue(value);
      if (sanitized) {
        results.add(sanitized);
      }
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

    for (const [key, entryValue] of Object.entries(value as UnknownRecord)) {
      if (!META_IDENTITY_KEY_PATTERN.test(key)) {
        if (typeof entryValue === 'object' && entryValue !== null && depth > 1) {
          traverse(entryValue, depth - 1);
        }
        continue;
      }

      if (typeof entryValue === 'string') {
        pushCandidate(entryValue);
      } else if (Array.isArray(entryValue)) {
        for (const item of entryValue) {
          if (typeof item === 'string') {
            pushCandidate(item);
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

function detectGitRemote(root: string): string | null {
  const cached = gitRemoteCache.get(root);
  if (cached !== undefined) {
    return cached;
  }

  try {
    const stdout = execFileSync('git', ['config', '--get', 'remote.origin.url'], {
      cwd: root,
      encoding: 'utf8',
      stdio: ['ignore', 'pipe', 'ignore']
    });
    const remote = sanitizeValue(stdout);
    gitRemoteCache.set(root, remote ?? null);
    return remote ?? null;
  } catch (error) {
    gitRemoteCache.set(root, null);
    if (error && typeof error === 'object' && 'code' in error) {
      const code = (error as { code?: string }).code;
      if (code !== 'ENOENT') {
        log.debug({ root, code }, 'git remote lookup failed');
      }
    }
    return null;
  }
}

function detectGitWorktree(root: string): string | null {
  try {
    const gitPath = path.join(root, '.git');
    const stats = fs.statSync(gitPath);
    if (stats.isFile()) {
      const contents = fs.readFileSync(gitPath, 'utf8');
      const match = /gitdir:\s*(.+)/i.exec(contents);
      if (match?.[1]) {
        const gitDir = sanitizeValue(match[1]);
        if (gitDir) {
          return path.resolve(root, gitDir);
        }
      }
    } else if (stats.isDirectory()) {
      return gitPath;
    }
  } catch {
    // ignore errors
  }
  return null;
}

export function resolveWorkspaceIdentity(
  root: string,
  context: RootResolutionContext = {}
): WorkspaceIdentity {
  const absoluteRoot = path.resolve(root);
  const components: WorkspaceIdentityComponent[] = [];
  const seen = new Set<string>();

  addComponent(seen, components, 'root', absoluteRoot);

  for (const value of collectFromHeaders(context.headers)) {
    addComponent(seen, components, 'header', value);
  }

  for (const value of collectFromEnv(context.env)) {
    addComponent(seen, components, 'env', value);
  }

  for (const value of collectFromMeta(context.meta)) {
    addComponent(seen, components, 'meta', value);
  }

  const gitRemote = detectGitRemote(absoluteRoot);
  if (gitRemote) {
    addComponent(seen, components, 'git-remote', gitRemote);
  }

  const gitWorktree = detectGitWorktree(absoluteRoot);
  if (gitWorktree) {
    addComponent(seen, components, 'gitdir', gitWorktree);
  }

  return { components };
}
