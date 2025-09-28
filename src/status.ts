import Database from 'better-sqlite3';
import { promises as fs } from 'node:fs';
import path from 'node:path';

import { DEFAULT_DB_FILENAME } from './constants.js';

export interface IndexStatusOptions {
  root: string;
  databaseName?: string;
  historyLimit?: number;
}

export interface IndexStatusIngestion {
  id: string;
  root: string;
  startedAt: number;
  finishedAt: number;
  durationMs: number;
  fileCount: number;
  skippedCount: number;
  deletedCount: number;
}

export interface IndexStatusResult {
  databasePath: string;
  databaseExists: boolean;
  databaseSizeBytes: number | null;
  totalFiles: number;
  totalChunks: number;
  embeddingModels: string[];
  totalGraphNodes: number;
  totalGraphEdges: number;
  latestIngestion: IndexStatusIngestion | null;
  recentIngestions: IndexStatusIngestion[];
}

interface RawIngestionRow {
  id: string;
  root: string;
  startedAt: number;
  finishedAt: number;
  fileCount: number;
  skippedCount: number;
  deletedCount: number;
}

const DEFAULT_HISTORY_LIMIT = 5;

function mapIngestionRow(row: RawIngestionRow): IndexStatusIngestion {
  return {
    ...row,
    durationMs: row.finishedAt - row.startedAt
  };
}

export async function getIndexStatus(options: IndexStatusOptions): Promise<IndexStatusResult> {
  const absoluteRoot = path.resolve(options.root);
  const dbPath = path.join(absoluteRoot, options.databaseName ?? DEFAULT_DB_FILENAME);

  let databaseExists = false;
  let databaseSizeBytes: number | null = null;

  try {
    const stats = await fs.stat(dbPath);
    if (!stats.isFile()) {
      throw new Error(`Expected SQLite database file at ${dbPath}, but found a different file type.`);
    }
    databaseExists = true;
    databaseSizeBytes = stats.size;
  } catch (error) {
    const code =
      error && typeof error === 'object' && 'code' in error
        ? (error as { code?: string }).code
        : undefined;
    if (code !== 'ENOENT') {
      throw error instanceof Error
        ? new Error(`Failed to stat database at ${dbPath}: ${error.message}`)
        : new Error(`Failed to stat database at ${dbPath}: ${String(error)}`);
    }
  }

  if (!databaseExists) {
    return {
      databasePath: dbPath,
      databaseExists: false,
      databaseSizeBytes: null,
      totalFiles: 0,
      totalChunks: 0,
      embeddingModels: [],
      totalGraphNodes: 0,
      totalGraphEdges: 0,
      latestIngestion: null,
      recentIngestions: []
    };
  }

  const db = new Database(dbPath, { readonly: true, fileMustExist: true });
  try {
    const totalFilesRow = db.prepare('SELECT COUNT(*) as count FROM files').get() as { count?: number } | undefined;
    const totalChunksRow = db.prepare('SELECT COUNT(*) as count FROM file_chunks').get() as { count?: number } | undefined;
    const totalGraphNodesRow = db
      .prepare('SELECT COUNT(*) as count FROM code_graph_nodes')
      .get() as { count?: number } | undefined;
    const totalGraphEdgesRow = db
      .prepare('SELECT COUNT(*) as count FROM code_graph_edges')
      .get() as { count?: number } | undefined;

    const embeddingRows = db
      .prepare('SELECT DISTINCT embedding_model as embeddingModel FROM file_chunks ORDER BY embedding_model ASC')
      .all() as { embeddingModel: string }[];

    const historyLimit = options.historyLimit ?? DEFAULT_HISTORY_LIMIT;
    const ingestionRows = historyLimit > 0
      ? (db
          .prepare(
            `SELECT
               id as id,
               root as root,
               started_at as startedAt,
               finished_at as finishedAt,
               file_count as fileCount,
               skipped_count as skippedCount,
               deleted_count as deletedCount
             FROM ingestions
             ORDER BY finished_at DESC
             LIMIT ?`
          )
          .all(historyLimit) as RawIngestionRow[])
      : [];

    const ingestions = ingestionRows.map(mapIngestionRow);

    return {
      databasePath: dbPath,
      databaseExists: true,
      databaseSizeBytes,
      totalFiles: totalFilesRow?.count ?? 0,
      totalChunks: totalChunksRow?.count ?? 0,
      embeddingModels: embeddingRows.map((row) => row.embeddingModel),
      totalGraphNodes: totalGraphNodesRow?.count ?? 0,
      totalGraphEdges: totalGraphEdgesRow?.count ?? 0,
      latestIngestion: ingestions[0] ?? null,
      recentIngestions: ingestions
    };
  } finally {
    db.close();
  }
}
