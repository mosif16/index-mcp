import Database from 'better-sqlite3';
import { promises as fs } from 'node:fs';
import path from 'node:path';

import { DEFAULT_DB_FILENAME } from './constants.js';

export interface ContextBundleSymbolSelector {
  name: string;
  kind?: string;
}

export interface ContextBundleOptions {
  root: string;
  databaseName?: string;
  file: string;
  symbol?: ContextBundleSymbolSelector;
  maxSnippets?: number;
  maxNeighbors?: number;
}

export interface BundleFileMetadata {
  path: string;
  size: number;
  modified: number;
  hash: string;
  lastIndexedAt: number;
}

export interface BundleSnippet {
  source: 'chunk' | 'content';
  chunkIndex: number | null;
  content: string;
  byteStart: number | null;
  byteEnd: number | null;
  lineStart: number | null;
  lineEnd: number | null;
}

export interface BundleDefinition {
  id: string;
  name: string;
  kind: string;
  signature: string | null;
  rangeStart: number | null;
  rangeEnd: number | null;
  metadata: Record<string, unknown> | null;
  visibility?: string | null;
  docstring?: string | null;
  todoCount?: number;
}

export interface BundleEdgeNeighbor {
  id: string;
  type: string;
  direction: 'incoming' | 'outgoing';
  metadata: Record<string, unknown> | null;
  fromNodeId: string;
  toNodeId: string;
  neighbor: {
    id: string;
    path: string | null;
    kind: string;
    name: string;
    signature: string | null;
    metadata: Record<string, unknown> | null;
  };
}

export interface BundleIngestionSummary {
  id: string;
  finishedAt: number;
  durationMs: number;
  fileCount: number;
}

export interface ContextBundleResult {
  databasePath: string;
  file: BundleFileMetadata;
  definitions: BundleDefinition[];
  focusDefinition: BundleDefinition | null;
  related: BundleEdgeNeighbor[];
  snippets: BundleSnippet[];
  latestIngestion: BundleIngestionSummary | null;
  warnings: string[];
  quickLinks: ContextBundleQuickLink[];
}

export interface ContextBundleQuickLink {
  type: 'file' | 'relatedSymbol';
  label: string;
  path: string | null;
  direction?: 'incoming' | 'outgoing';
  symbolId?: string;
  symbolKind?: string;
}

interface FileRow {
  path: string;
  size: number;
  modified: number;
  hash: string;
  lastIndexedAt: number;
  content: string | null;
}

interface ChunkRow {
  chunkIndex: number;
  content: string;
  byteStart: number | null;
  byteEnd: number | null;
  lineStart: number | null;
  lineEnd: number | null;
}

interface NodeRow {
  id: string;
  name: string;
  kind: string;
  signature: string | null;
  rangeStart: number | null;
  rangeEnd: number | null;
  metadata: string | null;
}

interface EdgeRow {
  id: string;
  type: string;
  metadata: string | null;
  sourceId: string;
  targetId: string;
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
    return {
      parseError: error instanceof Error ? error.message : String(error)
    };
  }
}

function clamp(value: number, min: number, max: number): number {
  return Math.max(min, Math.min(max, value));
}

function extractDocstring(content: string, rangeStart: number | null): string | null {
  if (rangeStart === null || rangeStart <= 0) {
    return null;
  }

  const prefix = content.slice(0, rangeStart);
  const blockMatch = /\/\*\*[\s\S]*?\*\/\s*$/u.exec(prefix);
  if (blockMatch) {
    const cleaned = blockMatch[0]
      .replace(/^\/\*\*/u, '')
      .replace(/\*\/\s*$/u, '')
      .split(/\r?\n/u)
      .map((line) => line.replace(/^\s*\*?\s?/u, '').trimEnd())
      .join('\n')
      .trim();
    return cleaned.length ? cleaned : null;
  }

  const lineMatch = /((?:\/\/.*\n)+)\s*$/u.exec(prefix);
  if (lineMatch) {
    const cleaned = lineMatch[1]
      .split(/\r?\n/u)
      .map((line) => line.replace(/^\s*\/\//u, '').trim())
      .filter((line) => line.length > 0)
      .join('\n')
      .trim();
    return cleaned.length ? cleaned : null;
  }

  return null;
}

function determineVisibility(
  content: string,
  rangeStart: number | null,
  kind: string,
  metadata: Record<string, unknown> | null
): string | null {
  if (rangeStart === null) {
    return null;
  }

  const preceding = content.slice(0, rangeStart);
  const lastLineStart = preceding.lastIndexOf('\n') + 1;
  const line = content.slice(lastLineStart, rangeStart + 200).split(/\r?\n/u)[0]?.trim() ?? '';

  if (/\bprivate\b/u.test(line)) {
    return 'private';
  }
  if (/\bprotected\b/u.test(line)) {
    return 'protected';
  }
  if (/\bpublic\b/u.test(line) || /\bexport\b/u.test(line)) {
    return 'public';
  }
  if (kind === 'method' && metadata && typeof metadata.className === 'string') {
    return 'public';
  }
  return 'internal';
}

function countTodos(content: string, rangeStart: number | null, rangeEnd: number | null): number {
  if (rangeStart === null || rangeEnd === null || rangeEnd <= rangeStart) {
    return 0;
  }
  const snippet = content.slice(rangeStart, rangeEnd);
  const matches = snippet.match(/\b(?:TODO|FIXME)\b/gi);
  return matches ? matches.length : 0;
}

function createQuickLinks(
  file: BundleFileMetadata,
  definitions: BundleDefinition[],
  related: BundleEdgeNeighbor[],
  focusDefinition: BundleDefinition | null
): ContextBundleQuickLink[] {
  const links = new Map<string, ContextBundleQuickLink>();

  links.set(`file:${file.path}`, {
    type: 'file',
    label: file.path,
    path: file.path
  });

  if (focusDefinition) {
    links.set(`symbol:${focusDefinition.id}`, {
      type: 'relatedSymbol',
      label: focusDefinition.name,
      path: file.path,
      symbolId: focusDefinition.id,
      symbolKind: focusDefinition.kind
    });
  }

  for (const definition of definitions) {
    if (definition === focusDefinition) {
      continue;
    }
    const key = `symbol:${definition.id}`;
    if (!links.has(key)) {
      links.set(key, {
        type: 'relatedSymbol',
        label: definition.name,
        path: file.path,
        symbolId: definition.id,
        symbolKind: definition.kind
      });
    }
  }

  for (const neighbor of related) {
    const key = `neighbor:${neighbor.neighbor.id}:${neighbor.direction}`;
    if (!links.has(key)) {
      links.set(key, {
        type: 'relatedSymbol',
        label: neighbor.neighbor.name,
        path: neighbor.neighbor.path,
        direction: neighbor.direction,
        symbolId: neighbor.neighbor.id,
        symbolKind: neighbor.neighbor.kind
      });
    }
  }

  return Array.from(links.values()).slice(0, 12);
}

export async function getContextBundle(options: ContextBundleOptions): Promise<ContextBundleResult> {
  const absoluteRoot = path.resolve(options.root);
  const stats = await fs.stat(absoluteRoot);
  if (!stats.isDirectory()) {
    throw new Error(`Context bundle root must be a directory: ${absoluteRoot}`);
  }

  const databasePath = path.join(absoluteRoot, options.databaseName ?? DEFAULT_DB_FILENAME);
  const db = new Database(databasePath, { readonly: true, fileMustExist: true });

  try {
    const fileRow = db
      .prepare(
        `SELECT path, size, modified, hash, last_indexed_at as lastIndexedAt, content
         FROM files
         WHERE path = ?`
      )
      .get(options.file) as FileRow | undefined;

    if (!fileRow) {
      throw new Error(`No file metadata found for '${options.file}'. Ensure the path is relative to the workspace root.`);
    }

    const snippetLimit = clamp(options.maxSnippets ?? 3, 0, 10);
    const neighborLimit = clamp(options.maxNeighbors ?? 12, 0, 50);

    const chunkRows = snippetLimit
      ? (db
          .prepare(
            `SELECT chunk_index as chunkIndex, content, byte_start as byteStart, byte_end as byteEnd, line_start as lineStart, line_end as lineEnd
             FROM file_chunks
             WHERE path = ?
             ORDER BY chunk_index ASC
             LIMIT ?`
          )
          .all(options.file, snippetLimit) as ChunkRow[])
      : [];

    const nodeRows = db
      .prepare(
        `SELECT id, name, kind, signature, range_start as rangeStart, range_end as rangeEnd, metadata
         FROM code_graph_nodes
         WHERE path = ?
         ORDER BY COALESCE(range_start, 9223372036854775807), name`
      )
      .all(options.file) as NodeRow[];

    let fileContentString = typeof fileRow.content === 'string' ? fileRow.content : null;
    if (!fileContentString) {
      try {
        fileContentString = await fs.readFile(path.join(absoluteRoot, fileRow.path), 'utf8');
      } catch {
        fileContentString = null;
      }
    }
    const definitions: BundleDefinition[] = nodeRows.map((row) => {
      const parsedMetadata = parseMetadata(row.metadata);
      const rangeStart = row.rangeStart ?? (typeof parsedMetadata?.startOffset === 'number' ? parsedMetadata.startOffset : null);
      const rangeEnd = row.rangeEnd ?? (typeof parsedMetadata?.endOffset === 'number' ? parsedMetadata.endOffset : null);
      const visibility = fileContentString
        ? determineVisibility(fileContentString, rangeStart, row.kind, parsedMetadata)
        : null;
      const docstring = fileContentString ? extractDocstring(fileContentString, rangeStart) : null;
      const todoCount = fileContentString ? countTodos(fileContentString, rangeStart, rangeEnd) : 0;

      return {
        id: row.id,
        name: row.name,
        kind: row.kind,
        signature: row.signature,
        rangeStart: row.rangeStart,
        rangeEnd: row.rangeEnd,
        metadata: parsedMetadata,
        visibility,
        docstring,
        todoCount
      };
    });

    let focusDefinition: BundleDefinition | null = null;
    if (options.symbol) {
      const { name, kind } = options.symbol;
      focusDefinition =
        definitions.find((def) => def.name === name && (!kind || def.kind === kind)) ??
        definitions.find((def) => def.name === name) ??
        null;
    }

    const focusNodeIds = new Set<string>();
    if (focusDefinition) {
      focusNodeIds.add(focusDefinition.id);
    } else {
      for (const definition of definitions) {
        focusNodeIds.add(definition.id);
      }
    }

    let related: BundleEdgeNeighbor[] = [];
    if (neighborLimit > 0 && focusNodeIds.size > 0) {
      const placeholders = Array.from(focusNodeIds).map(() => '?').join(', ');
      const params = Array.from(focusNodeIds);

      const outgoingRows = db
        .prepare(
          `SELECT e.id, e.type, e.metadata, e.source_id as sourceId, e.target_id as targetId,
                  n.id as neighborId, n.path as neighborPath, n.kind as neighborKind, n.name as neighborName,
                  n.signature as neighborSignature, n.metadata as neighborMetadata
           FROM code_graph_edges e
           JOIN code_graph_nodes n ON n.id = e.target_id
           WHERE e.source_id IN (${placeholders})
           LIMIT ?`
        )
        .all(...params, neighborLimit) as EdgeRow[];

      const incomingRows = db
        .prepare(
          `SELECT e.id, e.type, e.metadata, e.source_id as sourceId, e.target_id as targetId,
                  n.id as neighborId, n.path as neighborPath, n.kind as neighborKind, n.name as neighborName,
                  n.signature as neighborSignature, n.metadata as neighborMetadata
           FROM code_graph_edges e
           JOIN code_graph_nodes n ON n.id = e.source_id
           WHERE e.target_id IN (${placeholders})
           LIMIT ?`
        )
        .all(...params, neighborLimit) as EdgeRow[];

      const mapEdges = (rows: EdgeRow[], direction: 'incoming' | 'outgoing'): BundleEdgeNeighbor[] =>
        rows.map((row) => ({
          id: row.id,
          type: row.type,
          direction,
          metadata: parseMetadata(row.metadata),
          fromNodeId: row.sourceId,
          toNodeId: row.targetId,
          neighbor: {
            id: row.neighborId,
            path: row.neighborPath,
            kind: row.neighborKind,
            name: row.neighborName,
            signature: row.neighborSignature,
            metadata: parseMetadata(row.neighborMetadata)
          }
        }));

      related = [...mapEdges(outgoingRows, 'outgoing'), ...mapEdges(incomingRows, 'incoming')];
    }

    const snippets: BundleSnippet[] = chunkRows.map((row) => ({
      source: 'chunk',
      chunkIndex: row.chunkIndex,
      content: row.content,
      byteStart: row.byteStart,
      byteEnd: row.byteEnd,
      lineStart: row.lineStart,
      lineEnd: row.lineEnd
    }));

    if (!snippets.length && fileRow.content && snippetLimit > 0) {
      const lines = fileRow.content.split(/\r?\n/);
      const slice = lines.slice(0, Math.max(1, snippetLimit * 5));
      const content = slice.join('\n');
      const byteEnd = Buffer.byteLength(content, 'utf8');
      snippets.push({
        source: 'content',
        chunkIndex: null,
        content,
        byteStart: 0,
        byteEnd,
        lineStart: 1,
        lineEnd: slice.length
      });
    }

    const latestIngestionRow = db
      .prepare(
        `SELECT id, finished_at as finishedAt, started_at as startedAt, file_count as fileCount
         FROM ingestions
         ORDER BY finished_at DESC
         LIMIT 1`
      )
      .get() as { id: string; finishedAt: number; startedAt: number; fileCount: number } | undefined;

    const latestIngestion: BundleIngestionSummary | null = latestIngestionRow
      ? {
          id: latestIngestionRow.id,
          finishedAt: latestIngestionRow.finishedAt,
          durationMs: latestIngestionRow.finishedAt - latestIngestionRow.startedAt,
          fileCount: latestIngestionRow.fileCount
        }
      : null;

    const file: BundleFileMetadata = {
      path: fileRow.path,
      size: fileRow.size,
      modified: fileRow.modified,
      hash: fileRow.hash,
      lastIndexedAt: fileRow.lastIndexedAt
    };

    const warnings: string[] = [];
    if (!snippets.length) {
      warnings.push('No content snippets were available for the requested file.');
    }
    if (!definitions.length) {
      warnings.push('No graph metadata recorded for the requested file.');
    }

    const quickLinks = createQuickLinks(file, definitions, related, focusDefinition);

    return {
      databasePath,
      file,
      definitions,
      focusDefinition,
      related,
      snippets,
      latestIngestion,
      warnings,
      quickLinks
    };
  } finally {
    db.close();
  }
}
