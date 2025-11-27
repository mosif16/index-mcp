import crypto from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

import { DEFAULT_DB_FILENAME } from './constants.js';
import { createLogger } from './logger.js';
import type { WorkspaceIdentity, WorkspaceIdentityComponent } from './workspace-identity.js';

export type IndexStorageSource =
  | 'INDEX_MCP_DB'
  | 'INDEX_MCP_DB_DIR'
  | 'home'
  | 'tmp'
  | 'workspace';

export interface IndexStorageResolution {
  readonly databasePath: string;
  readonly storageDirectory: string;
  readonly source: IndexStorageSource;
  readonly rootHash: string;
  readonly identityHash: string;
  readonly identityComponents: WorkspaceIdentityComponent[];
}

interface DirectoryCandidate {
  readonly path: string;
  readonly source: Exclude<IndexStorageSource, 'INDEX_MCP_DB' | 'workspace'>;
}

const log = createLogger('index-storage');

const INDEX_MANIFEST_FILENAME = 'index-manifest.json';

const recordedWarnings = new Set<string>();

function warnOnce(key: string, message: string, context: Record<string, unknown>): void {
  if (recordedWarnings.has(key)) {
    return;
  }
  recordedWarnings.add(key);
  log.warn(context, message);
}

function toAbsolutePath(candidate: string): string {
  return path.isAbsolute(candidate) ? candidate : path.resolve(candidate);
}

function sanitizeDatabaseName(candidate: string | undefined): string {
  const trimmed = candidate?.trim();
  if (!trimmed) {
    return DEFAULT_DB_FILENAME;
  }
  const basename = path.basename(trimmed);
  if (!basename || basename === '.' || basename === '..') {
    return DEFAULT_DB_FILENAME;
  }
  return basename;
}

function ensureDirectory(directoryPath: string, context: Record<string, unknown>): boolean {
  try {
    const stats = fs.statSync(directoryPath);
    if (!stats.isDirectory()) {
      warnOnce(
        `${directoryPath}:not-directory`,
        '[index-mcp] Index storage path exists but is not a directory; skipping candidate.',
        { ...context, directory: directoryPath }
      );
      return false;
    }
  } catch (error) {
    const maybeErrno = error as NodeJS.ErrnoException;
    if (maybeErrno?.code && maybeErrno.code !== 'ENOENT') {
      warnOnce(
        `${directoryPath}:stat:${maybeErrno.code}`,
        '[index-mcp] Unable to inspect index storage directory; skipping candidate.',
        { ...context, directory: directoryPath, error: maybeErrno.message, code: maybeErrno.code }
      );
      return false;
    }
  }

  try {
    fs.mkdirSync(directoryPath, { recursive: true });
  } catch (error) {
    const maybeErrno = error as NodeJS.ErrnoException;
    warnOnce(
      `${directoryPath}:mkdir:${maybeErrno?.code ?? 'unknown'}`,
      '[index-mcp] Unable to create index storage directory; skipping candidate.',
      { ...context, directory: directoryPath, error: maybeErrno?.message, code: maybeErrno?.code }
    );
    return false;
  }

  try {
    fs.accessSync(directoryPath, fs.constants.W_OK);
  } catch (error) {
    const maybeErrno = error as NodeJS.ErrnoException;
    warnOnce(
      `${directoryPath}:access:${maybeErrno?.code ?? 'unknown'}`,
      '[index-mcp] Index storage directory is not writable; skipping candidate.',
      { ...context, directory: directoryPath, error: maybeErrno?.message, code: maybeErrno?.code }
    );
    return false;
  }

  return true;
}

function computeRootHash(root: string): string {
  return crypto.createHash('sha256').update(root).digest('hex').slice(0, 16);
}

function coalesceIdentityComponents(
  absoluteRoot: string,
  identity: WorkspaceIdentity | undefined
): WorkspaceIdentityComponent[] {
  const components: WorkspaceIdentityComponent[] = [];
  const seenValues = new Set<string>();

  const push = (component: WorkspaceIdentityComponent) => {
    const normalizedValue = component.value.trim();
    if (!normalizedValue || normalizedValue.length === 0) {
      return;
    }
    if (seenValues.has(normalizedValue)) {
      return;
    }
    seenValues.add(normalizedValue);
    components.push({ source: component.source, value: normalizedValue });
  };

  push({ source: 'root', value: absoluteRoot });

  if (identity) {
    for (const component of identity.components) {
      if (!component || typeof component.value !== 'string') {
        continue;
      }
      const source = component.source || 'unknown';
      push({ source, value: component.value });
    }
  }

  return components;
}

function computeIdentityHash(
  components: WorkspaceIdentityComponent[],
  rootHash: string
): string {
  if (components.length <= 1) {
    return rootHash;
  }

  const serialized = components.map((component) => `${component.source}:${component.value}`).join('\n');
  return crypto.createHash('sha256').update(serialized).digest('hex').slice(0, 16);
}

interface IndexManifest {
  readonly version: 1;
  readonly root: string;
  readonly rootHash: string;
  readonly identityHash: string;
  readonly components: WorkspaceIdentityComponent[];
  readonly updatedAt: string;
}

function readManifest(filePath: string): IndexManifest | null {
  try {
    const raw = fs.readFileSync(filePath, 'utf8');
    const parsed = JSON.parse(raw) as IndexManifest;
    if (parsed && parsed.version === 1 && Array.isArray(parsed.components)) {
      return parsed;
    }
  } catch {
    // ignore malformed or missing manifest files
  }
  return null;
}

function writeManifest(directory: string, manifest: IndexManifest): void {
  const manifestPath = path.join(directory, INDEX_MANIFEST_FILENAME);
  const existing = readManifest(manifestPath);
  if (existing) {
    const unchanged =
      existing.identityHash === manifest.identityHash &&
      existing.root === manifest.root &&
      existing.rootHash === manifest.rootHash &&
      existing.components.length === manifest.components.length &&
      existing.components.every((component, index) => {
        const candidate = manifest.components[index];
        return component.source === candidate.source && component.value === candidate.value;
      });
    if (unchanged) {
      return;
    }
  }

  try {
    fs.writeFileSync(manifestPath, JSON.stringify(manifest, null, 2));
  } catch (error) {
    log.warn({ directory, err: error }, '[index-mcp] Failed to write index storage manifest.');
  }
}

function getDirectoryCandidates(): DirectoryCandidate[] {
  const candidates: DirectoryCandidate[] = [];
  const explicitDir = process.env.INDEX_MCP_DB_DIR?.trim();
  if (explicitDir) {
    candidates.push({ source: 'INDEX_MCP_DB_DIR', path: toAbsolutePath(explicitDir) });
  }

  const homeDir = os.homedir();
  if (homeDir && homeDir.trim()) {
    candidates.push({ source: 'home', path: path.join(homeDir, '.index-mcp', 'indexes') });
  }

  candidates.push({ source: 'tmp', path: path.join(os.tmpdir(), 'index-mcp', 'indexes') });

  return candidates;
}

export function resolveIndexDatabasePath(
  root: string,
  databaseName?: string,
  identity?: WorkspaceIdentity
): IndexStorageResolution {
  const absoluteRoot = path.resolve(root);
  const normalizedName = sanitizeDatabaseName(databaseName);
  const rootHash = computeRootHash(absoluteRoot);
  const identityComponents = coalesceIdentityComponents(absoluteRoot, identity);
  const identityHash = computeIdentityHash(identityComponents, rootHash);

  const explicitPath = process.env.INDEX_MCP_DB?.trim();
  if (explicitPath) {
    const resolvedExplicit = toAbsolutePath(explicitPath);
    const parentDirectory = path.dirname(resolvedExplicit);
    const context = { source: 'INDEX_MCP_DB' as const };
    if (!ensureDirectory(parentDirectory, context)) {
      throw new Error(
        `[index-mcp] INDEX_MCP_DB=${resolvedExplicit} is not usable; parent directory cannot be prepared.`
      );
    }
    return {
      databasePath: resolvedExplicit,
      storageDirectory: parentDirectory,
      source: 'INDEX_MCP_DB',
      rootHash: 'explicit',
      identityHash,
      identityComponents
    };
  }

  const candidates = getDirectoryCandidates();

  for (const candidate of candidates) {
    const context = { source: candidate.source };
    if (!ensureDirectory(candidate.path, context)) {
      continue;
    }
    const rootDirectory = path.join(candidate.path, rootHash);
    if (!ensureDirectory(rootDirectory, { ...context, storageDirectory: rootDirectory })) {
      continue;
    }
    const storageDirectory =
      identityHash === rootHash ? rootDirectory : path.join(rootDirectory, identityHash);
    if (!ensureDirectory(storageDirectory, { ...context, storageDirectory })) {
      continue;
    }
    const databasePath = path.join(storageDirectory, normalizedName);
    writeManifest(storageDirectory, {
      version: 1,
      root: absoluteRoot,
      rootHash,
      identityHash,
      components: identityComponents,
      updatedAt: new Date().toISOString()
    });
    return {
      databasePath,
      storageDirectory,
      source: candidate.source,
      rootHash,
      identityHash,
      identityComponents
    };
  }

  const fallbackPath = path.join(absoluteRoot, normalizedName);
  warnOnce(
    `${absoluteRoot}:fallback`,
    '[index-mcp] Falling back to storing index inside workspace root; all storage candidates failed.',
    { root: absoluteRoot, databasePath: fallbackPath }
  );
  return {
    databasePath: fallbackPath,
    storageDirectory: absoluteRoot,
    source: 'workspace',
    rootHash,
    identityHash,
    identityComponents
  };
}
