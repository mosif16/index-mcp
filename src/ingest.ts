import Database from 'better-sqlite3';
import type { Database as DatabaseInstance } from 'better-sqlite3';
import { createReadStream, promises as fs } from 'node:fs';
import crypto from 'node:crypto';
import path from 'node:path';
import { pathToFileURL } from 'node:url';
import { StringDecoder } from 'node:string_decoder';

import { embedTexts, float32ArrayToBuffer, getDefaultEmbeddingModel } from './embedding.js';
import { extractGraphMetadata, type GraphExtractionResult } from './graph.js';
import { DEFAULT_DB_FILENAME, DEFAULT_INCLUDE_GLOBS, DEFAULT_EXCLUDE_GLOBS } from './constants.js';
import { loadNativeModule } from './native/index.js';
import type { NativeFileEntry, NativeScanResult } from './types/native.js';
const DEFAULT_MAX_FILE_SIZE_BYTES = 8 * 1024 * 1024; // 8 MiB

const STREAM_READER_HIGH_WATER_MARK = 256 * 1024; // 256 KiB

const DEFAULT_CHUNK_SIZE_TOKENS = 256;
const DEFAULT_CHUNK_OVERLAP_TOKENS = 32;
const DEFAULT_EMBED_BATCH_SIZE = 12;

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

export interface EmbeddingOptions {
  enabled?: boolean;
  model?: string;
  chunkSizeTokens?: number;
  chunkOverlapTokens?: number;
  batchSize?: number;
}

export interface GraphOptions {
  enabled?: boolean;
}

export interface IngestOptions {
  root: string;
  include?: string[];
  exclude?: string[];
  databaseName?: string;
  maxFileSizeBytes?: number;
  storeFileContent?: boolean;
  contentSanitizer?: ContentSanitizerSpec;
  embedding?: EmbeddingOptions;
  graph?: GraphOptions;
  paths?: string[];
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
  embeddedChunkCount: number;
  embeddingModel: string | null;
  graphNodeCount: number;
  graphEdgeCount: number;
}

interface FileRow {
  path: string;
  size: number;
  modified: number;
  hash: string;
  lastIndexedAt: number;
  content: string | null;
}

interface PendingChunk {
  id: string;
  path: string;
  chunkIndex: number;
  content: string;
  model: string;
  embedding?: Buffer;
}

function toPosixPath(relativePath: string): string {
  return relativePath.split(path.sep).join('/');
}

function bufferContainsNullByte(buffer: Buffer): boolean {
  const lengthToCheck = Math.min(buffer.length, 1024);
  for (let i = 0; i < lengthToCheck; i += 1) {
    if (buffer[i] === 0) {
      return true;
    }
  }
  return false;
}

interface ReadFileResult {
  hash: string;
  content: string | null;
  isBinary: boolean;
}

async function readFileContentAndHash(absPath: string, needsContent: boolean): Promise<ReadFileResult> {
  const hash = crypto.createHash('sha256');
  const stream = createReadStream(absPath, { highWaterMark: STREAM_READER_HIGH_WATER_MARK });
  const decoder = needsContent ? new StringDecoder('utf8') : null;

  let collectedText = '';
  let collecting = needsContent;
  let isBinary = false;

  try {
    for await (const chunk of stream) {
      const buffer = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk);
      hash.update(buffer);

      if (!collecting) {
        continue;
      }

      if (bufferContainsNullByte(buffer)) {
        isBinary = true;
        collecting = false;
        collectedText = '';
        continue;
      }

      collectedText += decoder!.write(buffer);
    }

    if (collecting) {
      collectedText += decoder!.end();
    } else if (decoder) {
      decoder.end();
    }
  } finally {
    stream.destroy();
  }

  const hashHex = hash.digest('hex');
  const content = !isBinary && needsContent ? collectedText : null;

  return {
    hash: hashHex,
    content,
    isBinary
  };
}

function ensureSchema(db: DatabaseInstance) {
  db.exec(`
    PRAGMA foreign_keys = ON;
    PRAGMA journal_mode = WAL;
    CREATE TABLE IF NOT EXISTS files (
      path TEXT PRIMARY KEY,
      size INTEGER NOT NULL,
      modified INTEGER NOT NULL,
      hash TEXT NOT NULL,
      last_indexed_at INTEGER NOT NULL,
      content TEXT
    );
    CREATE TABLE IF NOT EXISTS file_chunks (
      id TEXT PRIMARY KEY,
      path TEXT NOT NULL,
      chunk_index INTEGER NOT NULL,
      content TEXT NOT NULL,
      embedding BLOB NOT NULL,
      embedding_model TEXT NOT NULL,
      FOREIGN KEY (path) REFERENCES files(path) ON DELETE CASCADE
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
    CREATE INDEX IF NOT EXISTS file_chunks_path_idx ON file_chunks(path);
    CREATE TABLE IF NOT EXISTS code_graph_nodes (
      id TEXT PRIMARY KEY,
      path TEXT,
      kind TEXT NOT NULL,
      name TEXT NOT NULL,
      signature TEXT,
      range_start INTEGER,
      range_end INTEGER,
      metadata TEXT,
      UNIQUE(path, kind, name)
    );
    CREATE TABLE IF NOT EXISTS code_graph_edges (
      id TEXT PRIMARY KEY,
      source_id TEXT NOT NULL,
      target_id TEXT NOT NULL,
      type TEXT NOT NULL,
      source_path TEXT,
      target_path TEXT,
      metadata TEXT,
      FOREIGN KEY (source_id) REFERENCES code_graph_nodes(id) ON DELETE CASCADE,
      FOREIGN KEY (target_id) REFERENCES code_graph_nodes(id) ON DELETE CASCADE
    );
    CREATE INDEX IF NOT EXISTS code_graph_nodes_path_idx ON code_graph_nodes(path);
    CREATE INDEX IF NOT EXISTS code_graph_edges_source_idx ON code_graph_edges(source_id);
    CREATE INDEX IF NOT EXISTS code_graph_edges_target_idx ON code_graph_edges(target_id);
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

function chunkText(content: string, chunkSizeTokens: number, overlapTokens: number): string[] {
  const sanitized = content.trim();
  if (!sanitized) {
    return [];
  }

  const maxChars = Math.max(256, chunkSizeTokens * 4);
  const overlapChars = Math.max(0, overlapTokens * 4);
  const chunks: string[] = [];

  let start = 0;
  while (start < sanitized.length) {
    let end = Math.min(sanitized.length, start + maxChars);

    if (end < sanitized.length) {
      const newlineBreak = sanitized.lastIndexOf('\n', end - 1);
      if (newlineBreak > start + 200) {
        end = newlineBreak + 1;
      }
    }

    const snippet = sanitized.slice(start, end).trimEnd();
    if (snippet) {
      chunks.push(snippet);
    }

    if (end >= sanitized.length) {
      break;
    }

    const nextStart = end - overlapChars;
    start = nextStart > start ? nextStart : end;
  }

  return chunks;
}

function resolveEmbeddingDefaults(options?: EmbeddingOptions) {
  const model = options?.model ?? getDefaultEmbeddingModel();
  return {
    enabled: options?.enabled ?? true,
    model,
    chunkSizeTokens: options?.chunkSizeTokens ?? DEFAULT_CHUNK_SIZE_TOKENS,
    chunkOverlapTokens: options?.chunkOverlapTokens ?? DEFAULT_CHUNK_OVERLAP_TOKENS,
    batchSize: options?.batchSize ?? DEFAULT_EMBED_BATCH_SIZE
  };
}

function resolveGraphDefaults(options?: GraphOptions) {
  return {
    enabled: options?.enabled ?? true
  };
}

type EmbeddingConfig = ReturnType<typeof resolveEmbeddingDefaults>;
type GraphConfig = ReturnType<typeof resolveGraphDefaults>;

interface ProcessNativeEntriesParams {
  absoluteRoot: string;
  nativeResult: NativeScanResult;
  existingByPath: Map<string, FileRow>;
  files: FileRow[];
  seenPaths: Set<string>;
  chunkJobs: PendingChunk[];
  chunkRefreshPaths: Set<string>;
  graphUpdates: Map<string, GraphExtractionResult | null>;
  sanitizeContent: ContentSanitizer;
  embeddingConfig: EmbeddingConfig;
  graphConfig: GraphConfig;
  storeFileContent: boolean;
}

async function processNativeEntries({
  absoluteRoot,
  nativeResult,
  existingByPath,
  files,
  seenPaths,
  chunkJobs,
  chunkRefreshPaths,
  graphUpdates,
  sanitizeContent,
  embeddingConfig,
  graphConfig,
  storeFileContent
}: ProcessNativeEntriesParams): Promise<void> {
  for (const entry of nativeResult.files) {
    await processNativeEntry({
      absoluteRoot,
      entry,
      existingByPath,
      files,
      seenPaths,
      chunkJobs,
      chunkRefreshPaths,
      graphUpdates,
      sanitizeContent,
      embeddingConfig,
      graphConfig,
      storeFileContent
    });
  }
}

interface ProcessNativeEntryParams {
  absoluteRoot: string;
  entry: NativeFileEntry;
  existingByPath: Map<string, FileRow>;
  files: FileRow[];
  seenPaths: Set<string>;
  chunkJobs: PendingChunk[];
  chunkRefreshPaths: Set<string>;
  graphUpdates: Map<string, GraphExtractionResult | null>;
  sanitizeContent: ContentSanitizer;
  embeddingConfig: EmbeddingConfig;
  graphConfig: GraphConfig;
  storeFileContent: boolean;
}

async function processNativeEntry({
  absoluteRoot,
  entry,
  existingByPath,
  files,
  seenPaths,
  chunkJobs,
  chunkRefreshPaths,
  graphUpdates,
  sanitizeContent,
  embeddingConfig,
  graphConfig,
  storeFileContent
}: ProcessNativeEntryParams): Promise<void> {
  const normalizedPath = toPosixPath(entry.path);
  const absolutePath = path.join(absoluteRoot, normalizedPath);
  const existing = existingByPath.get(normalizedPath);

  if (existing && existing.size === entry.size && existing.modified === entry.modified) {
    seenPaths.add(normalizedPath);
    files.push({
      path: normalizedPath,
      size: existing.size,
      modified: existing.modified,
      hash: existing.hash,
      lastIndexedAt: Date.now(),
      content: storeFileContent ? existing.content : null
    });
    return;
  }

  let content: string | null = null;
  if (!entry.isBinary && entry.content !== null) {
    content = await sanitizeContent({
      path: normalizedPath,
      absolutePath,
      root: absoluteRoot,
      content: entry.content
    });
  }

  seenPaths.add(normalizedPath);
  chunkRefreshPaths.add(normalizedPath);

  if (embeddingConfig.enabled && content) {
    const trimmedContent = content.trim();
    if (trimmedContent) {
      const chunks = chunkText(trimmedContent, embeddingConfig.chunkSizeTokens, embeddingConfig.chunkOverlapTokens);
      const targetChunks = chunks.length > 0 ? chunks : [trimmedContent];
      targetChunks.forEach((chunkContent, index) => {
        if (!chunkContent.trim()) {
          return;
        }
        chunkJobs.push({
          id: crypto.randomUUID(),
          path: normalizedPath,
          chunkIndex: index,
          content: chunkContent,
          model: embeddingConfig.model
        });
      });
    }
  }

  files.push({
    path: normalizedPath,
    size: entry.size,
    modified: entry.modified,
    hash: entry.hash,
    lastIndexedAt: Date.now(),
    content: storeFileContent ? content : null
  });

  if (graphConfig.enabled) {
    const graphResult = content ? extractGraphMetadata(normalizedPath, content) : null;
    graphUpdates.set(normalizedPath, graphResult);
  }
}

function normalizeTargetPaths(root: string, paths?: string[]): string[] {
  if (!paths || paths.length === 0) {
    return [];
  }

  const absoluteRoot = path.resolve(root);
  const normalizedRootWithSep = absoluteRoot.endsWith(path.sep)
    ? absoluteRoot
    : `${absoluteRoot}${path.sep}`;

  const normalized = new Set<string>();

  for (const candidate of paths) {
    const absoluteCandidate = path.isAbsolute(candidate)
      ? path.resolve(candidate)
      : path.resolve(absoluteRoot, candidate);

    if (
      absoluteCandidate !== absoluteRoot &&
      !absoluteCandidate.startsWith(normalizedRootWithSep)
    ) {
      continue;
    }

    const relative = path.relative(absoluteRoot, absoluteCandidate);
    if (!relative || relative === '') {
      continue;
    }
    normalized.add(toPosixPath(relative));
  }

  return [...normalized];
}

export async function ingestCodebase(options: IngestOptions): Promise<IngestResult> {
  const databaseName = options.databaseName ?? DEFAULT_DB_FILENAME;
  const includeGlobs = options.include ?? DEFAULT_INCLUDE_GLOBS;
  const excludeGlobs = Array.from(
    new Set([
      ...DEFAULT_EXCLUDE_GLOBS,
      ...(options.exclude ?? []),
      `**/${databaseName}`,
      `**/${databaseName}-wal`,
      `**/${databaseName}-shm`
    ])
  );
  const maxFileSizeBytes = options.maxFileSizeBytes ?? DEFAULT_MAX_FILE_SIZE_BYTES;
  const storeFileContent = options.storeFileContent ?? true;
  const embeddingConfig = resolveEmbeddingDefaults(options.embedding);
  const graphConfig = resolveGraphDefaults(options.graph);

  const absoluteRoot = path.resolve(options.root);
  const targetPaths = normalizeTargetPaths(absoluteRoot, options.paths);
  const usingTargetPaths = targetPaths.length > 0;
  const searchPatterns = usingTargetPaths ? targetPaths : includeGlobs;
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

    const existingRows = db
      .prepare('SELECT path, size, modified, hash, last_indexed_at as lastIndexedAt, content FROM files')
      .all() as FileRow[];
    const existingByPath = new Map<string, FileRow>(existingRows.map((row) => [row.path, row]));

    const needsTextContent = storeFileContent || embeddingConfig.enabled || graphConfig.enabled;
    const nativeModule = await loadNativeModule();
    let nativeResult: NativeScanResult;

    try {
      nativeResult = await nativeModule.scanRepo({
        root: absoluteRoot,
        include: searchPatterns,
        exclude: excludeGlobs,
        maxFileSizeBytes: maxFileSizeBytes,
        needsContent: needsTextContent
      });
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      throw new Error(`[index-mcp] Native scan failed: ${message}`);
    }

    const files: FileRow[] = [];
    const skipped: SkippedFile[] = nativeResult.skipped.map((entry) => ({ ...entry }));
    const seenPaths = new Set<string>();
    const chunkJobs: PendingChunk[] = [];
    const chunkRefreshPaths = new Set<string>();
    const graphUpdates = new Map<string, GraphExtractionResult | null>();

    const existingPathsSet = usingTargetPaths
      ? new Set<string>(targetPaths.filter((p) => existingByPath.has(p)))
      : new Set<string>(existingByPath.keys());

    await processNativeEntries({
      absoluteRoot,
      nativeResult,
      existingByPath,
      files,
      seenPaths,
      chunkJobs,
      chunkRefreshPaths,
      graphUpdates,
      sanitizeContent,
      embeddingConfig,
      graphConfig,
      storeFileContent
    });

    const deletedPaths = usingTargetPaths
      ? targetPaths.filter((p) => !seenPaths.has(p) && existingByPath.has(p))
      : Array.from(existingPathsSet).filter((p) => !seenPaths.has(p));
    deletedPaths.forEach((p) => chunkRefreshPaths.add(p));

    let embeddedChunkCount = 0;

    if (embeddingConfig.enabled && chunkJobs.length > 0) {
      const jobsByModel = new Map<string, PendingChunk[]>();
      for (const job of chunkJobs) {
        const list = jobsByModel.get(job.model) ?? [];
        list.push(job);
        jobsByModel.set(job.model, list);
      }

      for (const [model, jobs] of jobsByModel) {
        for (let i = 0; i < jobs.length; i += embeddingConfig.batchSize) {
          const batch = jobs.slice(i, i + embeddingConfig.batchSize);
          const embeddings = await embedTexts(
            batch.map((job) => job.content),
            { model }
          );
          batch.forEach((job, idx) => {
            job.embedding = float32ArrayToBuffer(embeddings[idx]);
          });
          embeddedChunkCount += batch.length;
        }
      }
    }

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
    const deleteChunksStmt = db.prepare('DELETE FROM file_chunks WHERE path = ?');
    const insertChunkStmt = db.prepare(
      `INSERT INTO file_chunks (id, path, chunk_index, content, embedding, embedding_model)
       VALUES (@id, @path, @chunkIndex, @content, @embedding, @model)`
    );
    const deleteGraphNodesStmt = db.prepare('DELETE FROM code_graph_nodes WHERE path = ?');
    const insertGraphNodeStmt = db.prepare(
      `INSERT INTO code_graph_nodes (id, path, kind, name, signature, range_start, range_end, metadata)
       VALUES (@id, @path, @kind, @name, @signature, @rangeStart, @rangeEnd, @metadata)
       ON CONFLICT(id) DO UPDATE SET
         path = excluded.path,
         kind = excluded.kind,
         name = excluded.name,
         signature = excluded.signature,
         range_start = excluded.range_start,
         range_end = excluded.range_end,
         metadata = excluded.metadata`
    );
    const insertGraphEdgeStmt = db.prepare(
      `INSERT INTO code_graph_edges (id, source_id, target_id, type, source_path, target_path, metadata)
       VALUES (@id, @sourceId, @targetId, @type, @sourcePath, @targetPath, @metadata)
       ON CONFLICT(id) DO UPDATE SET
         source_id = excluded.source_id,
         target_id = excluded.target_id,
         type = excluded.type,
         source_path = excluded.source_path,
         target_path = excluded.target_path,
         metadata = excluded.metadata`
    );

    const insertIngestionStmt = db.prepare(
      `INSERT INTO ingestions (id, root, started_at, finished_at, file_count, skipped_count, deleted_count)
       VALUES (:id, :root, :started, :finished, :fileCount, :skippedCount, :deletedCount)`
    );

    const ingestionId = crypto.randomUUID();
    const endTime = Date.now();

    let graphNodeCount = 0;
    let graphEdgeCount = 0;

    const transaction = db.transaction(() => {
      for (const pathToReset of chunkRefreshPaths) {
        deleteChunksStmt.run(pathToReset);
        deleteGraphNodesStmt.run(pathToReset);
      }
      for (const file of files) {
        upsertStmt.run(file);
      }
      for (const chunk of chunkJobs) {
        if (!chunk.embedding) {
          continue;
        }
        insertChunkStmt.run({
          id: chunk.id,
          path: chunk.path,
          chunkIndex: chunk.chunkIndex,
          content: chunk.content,
          embedding: chunk.embedding,
          model: chunk.model
        });
      }
      if (graphConfig.enabled) {
        for (const graphData of graphUpdates.values()) {
          if (!graphData) {
            continue;
          }
          for (const node of graphData.entities) {
            insertGraphNodeStmt.run({
              id: node.id,
              path: node.path,
              kind: node.kind,
              name: node.name,
              signature: node.signature ?? null,
              rangeStart: node.rangeStart ?? null,
              rangeEnd: node.rangeEnd ?? null,
              metadata: node.metadata ? JSON.stringify(node.metadata) : null
            });
            graphNodeCount += 1;
          }
          for (const edge of graphData.edges) {
            insertGraphEdgeStmt.run({
              id: edge.id,
              sourceId: edge.sourceId,
              targetId: edge.targetId,
              type: edge.type,
              sourcePath: edge.sourcePath ?? null,
              targetPath: edge.targetPath ?? null,
              metadata: edge.metadata ? JSON.stringify(edge.metadata) : null
            });
            graphEdgeCount += 1;
          }
        }
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
      durationMs: endTime - startTime,
      embeddedChunkCount,
      embeddingModel: embeddingConfig.enabled ? embeddingConfig.model : null,
      graphNodeCount,
      graphEdgeCount
    };
  } finally {
    db.close();
  }
}
