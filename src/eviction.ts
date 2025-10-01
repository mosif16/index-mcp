import Database from 'better-sqlite3';
import { promises as fs } from 'node:fs';
import path from 'node:path';

import { DEFAULT_DB_FILENAME } from './constants.js';

export interface EvictionOptions {
  root: string;
  databaseName?: string;
  maxSizeBytes?: number;
}

export interface EvictionResult {
  databasePath: string;
  sizeBefore: number;
  sizeAfter: number;
  evictedChunks: number;
  evictedNodes: number;
  wasEvictionNeeded: boolean;
}

const DEFAULT_MAX_SIZE_BYTES = 150 * 1024 * 1024; // 150 MB

export async function evictLeastUsed(options: EvictionOptions): Promise<EvictionResult> {
  const absoluteRoot = path.resolve(options.root);
  const dbPath = path.join(absoluteRoot, options.databaseName ?? DEFAULT_DB_FILENAME);
  const maxSizeBytes = options.maxSizeBytes ?? DEFAULT_MAX_SIZE_BYTES;

  const statsBefore = await fs.stat(dbPath);
  const sizeBefore = statsBefore.size;

  if (sizeBefore <= maxSizeBytes) {
    return {
      databasePath: dbPath,
      sizeBefore,
      sizeAfter: sizeBefore,
      evictedChunks: 0,
      evictedNodes: 0,
      wasEvictionNeeded: false
    };
  }

  const db = new Database(dbPath);
  try {
    let evictedChunks = 0;
    let evictedNodes = 0;

    // Calculate how much we need to reduce (aim for 80% of max to avoid frequent evictions)
    const targetSize = maxSizeBytes * 0.8;
    const bytesToFree = sizeBefore - targetSize;
    
    // Evict least-used chunks first
    const chunkCountToEvict = Math.ceil((bytesToFree / sizeBefore) * 0.5 * 
      (db.prepare('SELECT COUNT(*) as count FROM file_chunks').get() as { count: number }).count);
    
    if (chunkCountToEvict > 0) {
      const deleteChunksStmt = db.prepare(`
        DELETE FROM file_chunks 
        WHERE id IN (
          SELECT id FROM file_chunks 
          ORDER BY COALESCE(hits, 0) ASC, chunk_index ASC 
          LIMIT ?
        )
      `);
      const result = deleteChunksStmt.run(chunkCountToEvict);
      evictedChunks = result.changes;
    }

    // Evict least-used nodes if we still need more space
    const statsAfterChunks = await fs.stat(dbPath);
    if (statsAfterChunks.size > targetSize) {
      const nodeCountToEvict = Math.ceil((statsAfterChunks.size - targetSize) / sizeBefore * 0.3 * 
        (db.prepare('SELECT COUNT(*) as count FROM code_graph_nodes').get() as { count: number }).count);
      
      if (nodeCountToEvict > 0) {
        const deleteNodesStmt = db.prepare(`
          DELETE FROM code_graph_nodes 
          WHERE id IN (
            SELECT id FROM code_graph_nodes 
            ORDER BY COALESCE(hits, 0) ASC 
            LIMIT ?
          )
        `);
        const result = deleteNodesStmt.run(nodeCountToEvict);
        evictedNodes = result.changes;
      }
    }

    // Vacuum to reclaim space
    db.exec('VACUUM');

    const statsAfter = await fs.stat(dbPath);
    const sizeAfter = statsAfter.size;

    return {
      databasePath: dbPath,
      sizeBefore,
      sizeAfter,
      evictedChunks,
      evictedNodes,
      wasEvictionNeeded: true
    };
  } finally {
    db.close();
  }
}
