import Database from 'better-sqlite3';
import { promises as fs } from 'node:fs';
import path from 'node:path';

import { DEFAULT_DB_FILENAME } from './constants.js';

export type GraphNeighborDirection = 'incoming' | 'outgoing' | 'both';

export interface GraphNodeDescriptor {
  id?: string;
  path?: string | null;
  kind?: string;
  name: string;
}

export interface GraphNeighborsOptions {
  root: string;
  databaseName?: string;
  node: GraphNodeDescriptor;
  direction?: GraphNeighborDirection;
  limit?: number;
}

export interface GraphNodeSummary {
  id: string;
  path: string | null;
  kind: string;
  name: string;
  signature: string | null;
  metadata: Record<string, unknown> | null;
}

export interface GraphNeighborEdge {
  id: string;
  type: string;
  metadata: Record<string, unknown> | null;
  direction: 'incoming' | 'outgoing';
  neighbor: GraphNodeSummary;
}

export interface GraphNeighborsResult {
  databasePath: string;
  node: GraphNodeSummary;
  neighbors: GraphNeighborEdge[];
}

interface NodeRow {
  id: string;
  path: string | null;
  kind: string;
  name: string;
  signature: string | null;
  metadata: string | null;
}

interface EdgeRow {
  id: string;
  type: string;
  metadata: string | null;
  neighborId: string;
  neighborPath: string | null;
  neighborKind: string;
  neighborName: string;
  neighborSignature: string | null;
  neighborMetadata: string | null;
}

function parseMetadata(payload: string | null): Record<string, unknown> | null {
  if (!payload) {
    return null;
  }
  try {
    return JSON.parse(payload) as Record<string, unknown>;
  } catch (error) {
    return { parseError: (error as Error).message };
  }
}

function mapNodeRow(row: NodeRow): GraphNodeSummary {
  return {
    id: row.id,
    path: row.path,
    kind: row.kind,
    name: row.name,
    signature: row.signature,
    metadata: parseMetadata(row.metadata)
  };
}

export async function graphNeighbors(options: GraphNeighborsOptions): Promise<GraphNeighborsResult> {
  const absoluteRoot = path.resolve(options.root);
  const stats = await fs.stat(absoluteRoot);
  if (!stats.isDirectory()) {
    throw new Error(`Graph query root must be a directory: ${absoluteRoot}`);
  }

  const dbPath = path.join(absoluteRoot, options.databaseName ?? DEFAULT_DB_FILENAME);
  const db = new Database(dbPath, { readonly: true });

  try {
    let targetNode: GraphNodeSummary | null = null;

    if (options.node.id) {
      const row = db
        .prepare(
          `SELECT id, path, kind, name, signature, metadata
           FROM code_graph_nodes
           WHERE id = ?`
        )
        .get(options.node.id) as NodeRow | undefined;
      if (!row) {
        throw new Error(`No graph node found for id '${options.node.id}'.`);
      }
      targetNode = mapNodeRow(row);
    } else {
      const conditions: string[] = ['name = :name'];
      const params: Record<string, unknown> = { name: options.node.name };

      if (options.node.path !== undefined) {
        if (options.node.path === null) {
          conditions.push('path IS NULL');
        } else {
          conditions.push('path = :path');
          params.path = options.node.path;
        }
      }
      if (options.node.kind) {
        conditions.push('kind = :kind');
        params.kind = options.node.kind;
      }

      const query = `SELECT id, path, kind, name, signature, metadata
        FROM code_graph_nodes
        WHERE ${conditions.join(' AND ')}
        ORDER BY path IS NULL, path
        LIMIT 2`;

      const rows = db.prepare(query).all(params) as NodeRow[];
      if (!rows.length) {
        const filters = [`name='${options.node.name}'`];
        if (options.node.path) {
          filters.push(`path='${options.node.path}'`);
        }
        if (options.node.kind) {
          filters.push(`kind='${options.node.kind}'`);
        }
        throw new Error(`No graph node found matching ${filters.join(', ')}.`);
      }
      if (rows.length > 1) {
        throw new Error('Multiple graph nodes matched; please provide a more specific node descriptor.');
      }
      targetNode = mapNodeRow(rows[0]);
    }

    const direction = options.direction ?? 'outgoing';
    const limit = Math.min(Math.max(options.limit ?? 16, 1), 100);

    const neighbors: GraphNeighborEdge[] = [];
    const directionsToQuery: Array<'incoming' | 'outgoing'> = direction === 'both' ? ['outgoing', 'incoming'] : [direction];

    for (const dir of directionsToQuery) {
      const edgeQuery =
        dir === 'outgoing'
          ? `SELECT e.id, e.type, e.metadata, n.id as neighborId, n.path as neighborPath, n.kind as neighborKind, n.name as neighborName, n.signature as neighborSignature, n.metadata as neighborMetadata
             FROM code_graph_edges e
             JOIN code_graph_nodes n ON n.id = e.target_id
             WHERE e.source_id = ?
             LIMIT ?`
          : `SELECT e.id, e.type, e.metadata, n.id as neighborId, n.path as neighborPath, n.kind as neighborKind, n.name as neighborName, n.signature as neighborSignature, n.metadata as neighborMetadata
             FROM code_graph_edges e
             JOIN code_graph_nodes n ON n.id = e.source_id
             WHERE e.target_id = ?
             LIMIT ?`;

      const rows = db.prepare(edgeQuery).all(targetNode.id, limit) as EdgeRow[];
      for (const row of rows) {
        neighbors.push({
          id: row.id,
          type: row.type,
          metadata: parseMetadata(row.metadata),
          direction: dir,
          neighbor: {
            id: row.neighborId,
            path: row.neighborPath,
            kind: row.neighborKind,
            name: row.neighborName,
            signature: row.neighborSignature,
            metadata: parseMetadata(row.neighborMetadata)
          }
        });
      }
    }

    return {
      databasePath: dbPath,
      node: targetNode,
      neighbors
    };
  } finally {
    db.close();
  }
}
