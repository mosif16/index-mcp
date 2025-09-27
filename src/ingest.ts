import Database from 'better-sqlite3';
import type { Database as DatabaseInstance } from 'better-sqlite3';
import fg from 'fast-glob';
import { promises as fs } from 'node:fs';
import crypto from 'node:crypto';
import path from 'node:path';

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
}

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

    const existingPaths = db.prepare('SELECT path FROM files').pluck().all() as string[];
    const existingPathsSet = new Set<string>(existingPaths);

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
      const absPath = path.join(absoluteRoot, relativePath);
      let stats;
      try {
        stats = await fs.stat(absPath);
      } catch (error) {
        skipped.push({
          path: toPosixPath(relativePath),
          reason: 'read-error',
          message: error instanceof Error ? error.message : 'Unknown error'
        });
        continue;
      }

      if (stats.size > maxFileSizeBytes) {
        skipped.push({
          path: toPosixPath(relativePath),
          reason: 'file-too-large',
          size: stats.size
        });
        continue;
      }

      let fileBuffer: Buffer;
      try {
        fileBuffer = await fs.readFile(absPath);
      } catch (error) {
        skipped.push({
          path: toPosixPath(relativePath),
          reason: 'read-error',
          size: stats.size,
          message: error instanceof Error ? error.message : 'Unknown error'
        });
        continue;
      }

      const content: string | null = storeFileContent && !isProbablyBinary(fileBuffer)
        ? fileBuffer.toString('utf8')
        : null;

      const hash = crypto
        .createHash('sha256')
        .update(fileBuffer)
        .digest('hex');

      const normalizedPath = toPosixPath(relativePath);
      seenPaths.add(normalizedPath);

      files.push({
        path: normalizedPath,
        size: stats.size,
        modified: Math.floor(stats.mtimeMs),
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
