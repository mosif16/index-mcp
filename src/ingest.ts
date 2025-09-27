import Database from 'better-sqlite3';
import type { Database as DatabaseInstance } from 'better-sqlite3';
import fg from 'fast-glob';
import ignore, { type Ignore } from 'ignore';
import { promises as fs } from 'node:fs';
import crypto from 'node:crypto';
import path from 'node:path';
import { pathToFileURL } from 'node:url';

const DEFAULT_DB_FILENAME = '.mcp-index.sqlite';
const DEFAULT_INCLUDE = ['**/*'];
const DEFAULT_EXCLUDE = [
  '**/.git/**',
  '**/.svn/**',
  '**/.hg/**',
  '**/.mcp-index.sqlite',
  '**/node_modules/**',
  '**/vendor/**',
  '**/dist/**',
  '**/build/**'
];
const DEFAULT_MAX_FILE_SIZE_BYTES = 512 * 1024; // 512 KiB

export interface IngestOptions {
  root: string;
  include?: string[];
  exclude?: string[];
  databaseName?: string;
  maxFileSizeBytes?: number;
  storeFileContent?: boolean;
  contentSanitizer?: ContentSanitizerSpec;
}

export interface ContentSanitizerSpec {
  module: string;
  exportName?: string;
  options?: unknown;
}

interface SanitizerPayload {
  path: string;
  absolutePath: string;
  root: string;
  content: string;
}

type SanitizerModuleFunction = (
  payload: SanitizerPayload,
  options?: unknown
) => string | null | undefined | Promise<string | null | undefined>;

type ContentSanitizer = (payload: SanitizerPayload) => Promise<string | null>;

export interface SkippedFile {
  path: string;
  reason: 'file-too-large' | 'read-error';
  size?: number;
  message?: string;
}

export interface IngestResult {
  root: string;
  databasePath: string;
  databaseSizeBytes: number;
  ingestedFileCount: number;
  skipped: SkippedFile[];
  deletedPaths: string[];
  durationMs: number;
}

interface FileRow {
  path: string;
  size: number;
  modified: number;
  hash: string;
  lastIndexedAt: number;
  content: string | null;
}

function toPosixPath(relativePath: string): string {
  return relativePath.split(path.sep).join('/');
}

function isProbablyBinary(buffer: Buffer): boolean {
  const lengthToCheck = Math.min(buffer.length, 1024);
  for (let i = 0; i < lengthToCheck; i += 1) {
    if (buffer[i] === 0) {
      return true;
    }
  }
  return false;
}

function ensureSchema(db: DatabaseInstance) {
  db.exec(`
    PRAGMA journal_mode = WAL;
    CREATE TABLE IF NOT EXISTS files (
      path TEXT PRIMARY KEY,
      size INTEGER NOT NULL,
      modified INTEGER NOT NULL,
      hash TEXT NOT NULL,
      last_indexed_at INTEGER NOT NULL,
      content TEXT
    );
    CREATE TABLE IF NOT EXISTS ingestions (
      id TEXT PRIMARY KEY,
      root TEXT NOT NULL,
      started_at INTEGER NOT NULL,
      finished_at INTEGER NOT NULL,
      file_count INTEGER NOT NULL,
      skipped_count INTEGER NOT NULL,
      deleted_count INTEGER NOT NULL
    );
    CREATE INDEX IF NOT EXISTS files_hash_idx ON files(hash);
  `);
}

function toModuleSpecifier(root: string, specifier: string): string {
  if (specifier.startsWith('.') || specifier.startsWith('..')) {
    return pathToFileURL(path.resolve(root, specifier)).href;
  }

  if (path.isAbsolute(specifier)) {
    return pathToFileURL(specifier).href;
  }

  return specifier;
}

function isSanitizer(fn: unknown): fn is SanitizerModuleFunction {
  return typeof fn === 'function';
}

const identitySanitizer: ContentSanitizer = async ({ content }) => content;

async function loadSanitizer(root: string, spec?: ContentSanitizerSpec): Promise<ContentSanitizer> {
  if (!spec) {
    return identitySanitizer;
  }

  const moduleSpecifier = toModuleSpecifier(root, spec.module);
  const imported = await import(moduleSpecifier);
  const candidate = spec.exportName
    ? (imported as Record<string, unknown>)[spec.exportName]
    : (imported as Record<string, unknown>).default ?? (imported as Record<string, unknown>).sanitize;

  if (!isSanitizer(candidate)) {
    throw new Error(
      `Content sanitizer '${spec.module}${spec.exportName ? `#${spec.exportName}` : ''}' is not a function`
    );
  }

  return async (payload) => {
    const result = await candidate(payload, spec.options);
    if (typeof result === 'string' || result === null) {
      return result ?? null;
    }

    if (result === undefined) {
      return payload.content;
    }

    throw new Error('Content sanitizer must return a string, null, or undefined.');
  };
}

async function loadGitIgnore(root: string): Promise<Ignore | null> {
  const gitignorePath = path.join(root, '.gitignore');
  try {
    const contents = await fs.readFile(gitignorePath, 'utf8');
    if (!contents.trim()) {
      return null;
    }

    return ignore().add(contents);
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === 'ENOENT') {
      return null;
    }

    throw error;
  }
}

export async function ingestCodebase(options: IngestOptions): Promise<IngestResult> {
  const databaseName = options.databaseName ?? DEFAULT_DB_FILENAME;
  const includeGlobs = options.include ?? DEFAULT_INCLUDE;
  const excludeGlobs = Array.from(
    new Set([
      ...DEFAULT_EXCLUDE,
      ...(options.exclude ?? []),
      `**/${databaseName}`
    ])
  );
  const maxFileSizeBytes = options.maxFileSizeBytes ?? DEFAULT_MAX_FILE_SIZE_BYTES;
  const storeFileContent = options.storeFileContent ?? true;

  const absoluteRoot = path.resolve(options.root);
  const rootStats = await fs.stat(absoluteRoot);
  if (!rootStats.isDirectory()) {
    throw new Error(`Ingest root must be a directory: ${absoluteRoot}`);
  }

  const dbPath = path.join(absoluteRoot, databaseName);
  const startTime = Date.now();

  const db: DatabaseInstance = new Database(dbPath);
  try {
    ensureSchema(db);

    const sanitizeContent = await loadSanitizer(absoluteRoot, options.contentSanitizer);
    const gitIgnore = await loadGitIgnore(absoluteRoot);

    const existingRows = db
      .prepare('SELECT path, size, modified, hash, last_indexed_at as lastIndexedAt, content FROM files')
      .all() as FileRow[];
    const existingByPath = new Map<string, FileRow>(existingRows.map((row) => [row.path, row]));
    const existingPathsSet = new Set<string>(existingByPath.keys());

    const matches = await fg(includeGlobs, {
      cwd: absoluteRoot,
      dot: false,
      ignore: excludeGlobs,
      onlyFiles: true,
      followSymbolicLinks: false,
      unique: true
    });

    const files: FileRow[] = [];
    const skipped: SkippedFile[] = [];
    const seenPaths = new Set<string>();

    for (const relativePath of matches) {
      const normalizedPath = toPosixPath(relativePath);

      if (gitIgnore?.ignores(normalizedPath)) {
        continue;
      }

      const absPath = path.join(absoluteRoot, relativePath);
      let stats;
      try {
        stats = await fs.stat(absPath);
      } catch (error) {
        skipped.push({
          path: normalizedPath,
          reason: 'read-error',
          message: error instanceof Error ? error.message : 'Unknown error'
        });
        continue;
      }

      if (stats.size > maxFileSizeBytes) {
        skipped.push({
          path: normalizedPath,
          reason: 'file-too-large',
          size: stats.size
        });
        continue;
      }

      const normalizedMtime = Math.floor(stats.mtimeMs);
      const existing = existingByPath.get(normalizedPath);

      if (existing && existing.size === stats.size && existing.modified === normalizedMtime) {
        seenPaths.add(normalizedPath);
        files.push({
          path: normalizedPath,
          size: existing.size,
          modified: existing.modified,
          hash: existing.hash,
          lastIndexedAt: Date.now(),
          content: storeFileContent ? existing.content : null
        });
        continue;
      }

      let fileBuffer: Buffer;
      try {
        fileBuffer = await fs.readFile(absPath);
      } catch (error) {
        skipped.push({
          path: normalizedPath,
          reason: 'read-error',
          size: stats.size,
          message: error instanceof Error ? error.message : 'Unknown error'
        });
        continue;
      }

      let content: string | null = null;
      if (storeFileContent && !isProbablyBinary(fileBuffer)) {
        const rawContent = fileBuffer.toString('utf8');
        content = await sanitizeContent({
          path: normalizedPath,
          absolutePath: absPath,
          root: absoluteRoot,
          content: rawContent
        });
      }

      const hash = crypto
        .createHash('sha256')
        .update(fileBuffer)
        .digest('hex');

      seenPaths.add(normalizedPath);

      files.push({
        path: normalizedPath,
        size: stats.size,
        modified: normalizedMtime,
        hash,
        lastIndexedAt: Date.now(),
        content
      });
    }

    const deletedPaths = Array.from(existingPathsSet).filter((p) => !seenPaths.has(p));

    const upsertStmt = db.prepare(
      `INSERT INTO files (path, size, modified, hash, last_indexed_at, content)
       VALUES (@path, @size, @modified, @hash, @lastIndexedAt, @content)
       ON CONFLICT(path) DO UPDATE SET
         size = excluded.size,
         modified = excluded.modified,
         hash = excluded.hash,
         last_indexed_at = excluded.last_indexed_at,
         content = excluded.content`
    );

    const deleteStmt = db.prepare('DELETE FROM files WHERE path = ?');

    const insertIngestionStmt = db.prepare(
      `INSERT INTO ingestions (id, root, started_at, finished_at, file_count, skipped_count, deleted_count)
       VALUES (:id, :root, :started, :finished, :fileCount, :skippedCount, :deletedCount)`
    );

    const ingestionId = crypto.randomUUID();
    const endTime = Date.now();

    const transaction = db.transaction(() => {
      for (const file of files) {
        upsertStmt.run(file);
      }
      for (const deleted of deletedPaths) {
        deleteStmt.run(deleted);
      }
      insertIngestionStmt.run({
        id: ingestionId,
        root: absoluteRoot,
        started: startTime,
        finished: endTime,
        fileCount: files.length,
        skippedCount: skipped.length,
        deletedCount: deletedPaths.length
      });
    });

    transaction();

    const dbStats = await fs.stat(dbPath);

    return {
      root: absoluteRoot,
      databasePath: dbPath,
      databaseSizeBytes: dbStats.size,
      ingestedFileCount: files.length,
      skipped,
      deletedPaths,
      durationMs: endTime - startTime
    };
  } finally {
    db.close();
  }
}
