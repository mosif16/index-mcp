import Database from 'better-sqlite3';
import { execFile } from 'node:child_process';
import { promises as fs } from 'node:fs';
import path from 'node:path';
import { promisify } from 'node:util';

import { DEFAULT_DB_FILENAME } from './constants.js';

const execFileAsync = promisify(execFile);

export interface IndexStatusOptions {
  root: string;
  databaseName?: string;
}

export interface IndexStatusResult {
  databasePath: string;
  databaseExists: boolean;
  databaseSizeBytes: number | null;
  totalSymbols: number;
  totalSnippets: number;
  lastIndexedAt: number | null;
  indexedCommitSha: string | null;
  currentCommitSha: string | null;
  isStale: boolean;
}

async function resolveGitCommitSha(root: string): Promise<string | null> {
  try {
    const { stdout } = await execFileAsync('git', ['rev-parse', 'HEAD'], { cwd: root });
    const sha = stdout.trim();
    return sha ? sha : null;
  } catch {
    return null;
  }
}

export async function getIndexStatus(options: IndexStatusOptions): Promise<IndexStatusResult> {
  const absoluteRoot = path.resolve(options.root);
  const databasePath = path.join(absoluteRoot, options.databaseName ?? DEFAULT_DB_FILENAME);

  let databaseExists = false;
  let databaseSizeBytes: number | null = null;

  try {
    const stats = await fs.stat(databasePath);
    if (!stats.isFile()) {
      throw new Error(`Expected SQLite database file at ${databasePath}, but found a different file type.`);
    }
    databaseExists = true;
    databaseSizeBytes = stats.size;
  } catch (error) {
    const code =
      error && typeof error === 'object' && 'code' in error ? (error as { code?: string }).code : undefined;
    if (code !== 'ENOENT') {
      throw error instanceof Error
        ? new Error(`Failed to stat database at ${databasePath}: ${error.message}`)
        : new Error(`Failed to stat database at ${databasePath}: ${String(error)}`);
    }
  }

  if (!databaseExists) {
    const currentCommitSha = await resolveGitCommitSha(absoluteRoot);
    return {
      databasePath,
      databaseExists: false,
      databaseSizeBytes: null,
      totalSymbols: 0,
      totalSnippets: 0,
      lastIndexedAt: null,
      indexedCommitSha: null,
      currentCommitSha,
      isStale: true
    };
  }

  const db = new Database(databasePath, { readonly: true, fileMustExist: true });
  try {
    const symbolCountRow = db
      .prepare('SELECT COUNT(*) as count FROM symbols')
      .get() as { count?: number } | undefined;
    const snippetCountRow = db
      .prepare('SELECT COUNT(*) as count FROM snippets')
      .get() as { count?: number } | undefined;
    const metaRow = db
      .prepare('SELECT commit_sha as commitSha, indexed_at as indexedAt FROM meta WHERE id = 1')
      .get() as { commitSha?: string | null; indexedAt?: number | null } | undefined;

    const indexedCommitSha = metaRow?.commitSha ?? null;
    const lastIndexedAt = metaRow?.indexedAt ?? null;
    const currentCommitSha = await resolveGitCommitSha(absoluteRoot);
    const isStale = Boolean(
      indexedCommitSha && currentCommitSha && indexedCommitSha !== currentCommitSha
    );

    return {
      databasePath,
      databaseExists: true,
      databaseSizeBytes,
      totalSymbols: symbolCountRow?.count ?? 0,
      totalSnippets: snippetCountRow?.count ?? 0,
      lastIndexedAt,
      indexedCommitSha,
      currentCommitSha,
      isStale
    };
  } finally {
    db.close();
  }
}
