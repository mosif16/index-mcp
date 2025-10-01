import Database from 'better-sqlite3';
import type { Database as DatabaseInstance } from 'better-sqlite3';
import { promises as fs } from 'node:fs';
import crypto from 'node:crypto';
import os from 'node:os';
import path from 'node:path';
import { pathToFileURL } from 'node:url';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';

import { embedTexts, float32ArrayToBuffer, getDefaultEmbeddingModel } from './embedding.js';
import { extractGraphMetadata } from './graph.js';
import { DEFAULT_DB_FILENAME, DEFAULT_INCLUDE_GLOBS, DEFAULT_EXCLUDE_GLOBS } from './constants.js';
import { loadNativeModule } from './native/index.js';
import type {
  NativeBatchAnalysisResult,
  NativeFileEntry,
  NativeModule,
  NativeScanResult,
  NativeSkippedFile
} from './types/native.js';

const execFileAsync = promisify(execFile);
const DEFAULT_MAX_FILE_SIZE_BYTES = 8 * 1024 * 1024; // 8 MiB

const DEFAULT_CHUNK_SIZE_TOKENS = 256;
const DEFAULT_CHUNK_OVERLAP_TOKENS = 32;
const DEFAULT_EMBED_BATCH_SIZE = 32;

function hasNativeMetadataFlow(
  module: NativeModule
): module is NativeModule & {
  scanRepoMetadata: NonNullable<NativeModule['scanRepoMetadata']>;
  readRepoFiles: NonNullable<NativeModule['readRepoFiles']>;
} {
  return typeof module.scanRepoMetadata === 'function' && typeof module.readRepoFiles === 'function';
}

function hasNativeBatchAnalysisFlow(
  module: NativeModule
): module is NativeModule & {
  analyzeFileContentBatch: NonNullable<NativeModule['analyzeFileContentBatch']>;
} {
  return typeof module.analyzeFileContentBatch === 'function';
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
  autoEvict?: boolean;
  maxDatabaseSizeBytes?: number;
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
  evicted?: {
    chunks: number;
    nodes: number;
  };
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
  byteStart: number | null;
  byteEnd: number | null;
  lineStart: number | null;
  lineEnd: number | null;
  embedding?: Buffer;
}

interface PendingAnalysis {
  path: string;
  content: string;
}

interface PendingGraphNode {
  id: string;
  path: string | null;
  kind: string;
  name: string;
  signature: string | null;
  rangeStart: number | null;
  rangeEnd: number | null;
  metadata: string | null;
}

interface PendingGraphEdge {
  id: string;
  sourceId: string;
  targetId: string;
  type: string;
  sourcePath: string | null;
  targetPath: string | null;
  metadata: string | null;
}

interface NormalizedFragment {
  content: string;
  byteStart: number | null;
  byteEnd: number | null;
  lineStart: number | null;
  lineEnd: number | null;
}

function availableParallelism(): number {
  try {
    if (typeof os.availableParallelism === 'function') {
      return Math.max(1, os.availableParallelism());
    }
  } catch {
    // Ignore errors and fall back to os.cpus().length
  }
  const cores = os.cpus();
  return Array.isArray(cores) && cores.length > 0 ? cores.length : 1;
}

function resolvePipelineConcurrency(): number {
  const raw = process.env.INDEX_MCP_INGEST_CONCURRENCY;
  if (typeof raw === 'string' && raw.trim().length > 0) {
    const parsed = Number.parseInt(raw, 10);
    if (Number.isInteger(parsed) && parsed > 0) {
      return parsed;
    }
  }

  const parallelism = availableParallelism();
  return Math.min(Math.max(2, parallelism), 16);
}

async function runWithConcurrency<T>(
  items: readonly T[],
  limit: number,
  worker: (item: T) => Promise<void>
): Promise<void> {
  if (!items.length) {
    return;
  }

  const normalizedLimit = Math.max(1, Math.floor(limit));
  let index = 0;

  async function runNext(): Promise<void> {
    for (;;) {
      const current = index;
      index += 1;
      if (current >= items.length) {
        return;
      }
      await worker(items[current]);
    }
  }

  const workers: Promise<void>[] = [];
  const workerCount = Math.min(normalizedLimit, items.length);
  for (let i = 0; i < workerCount; i += 1) {
    workers.push(runNext());
  }

  await Promise.all(workers);
}

function appendFragmentsToChunkJobs(
  path: string,
  fragments: NormalizedFragment[],
  _fallbackContent: string,
  chunkJobs: PendingChunk[],
  model: string
): void {
  const usableFragments = fragments.filter((fragment) => fragment.content);
  if (!usableFragments.length) {
    throw new Error(`[index-mcp] Native chunking produced no fragments for ${path}`);
  }

  usableFragments.forEach((fragment, index) => {
    chunkJobs.push({
      id: crypto.randomUUID(),
      path,
      chunkIndex: index,
      content: fragment.content,
      model,
      byteStart: fragment.byteStart,
      byteEnd: fragment.byteEnd,
      lineStart: fragment.lineStart,
      lineEnd: fragment.lineEnd
    });
  });
}

function normalizeNativeFragments(
  fragments: Array<{
    content?: string | null;
    byteStart?: number | null;
    byteEnd?: number | null;
    lineStart?: number | null;
    lineEnd?: number | null;
  }> | undefined
): NormalizedFragment[] {
  if (!fragments?.length) {
    return [];
  }

  return fragments
    .filter((fragment) => fragment && typeof fragment.content === 'string' && fragment.content.trim().length > 0)
    .map((fragment) => ({
      content: fragment.content as string,
      byteStart:
        fragment.byteStart !== undefined && fragment.byteStart !== null
          ? fragment.byteStart
          : null,
      byteEnd:
        fragment.byteEnd !== undefined && fragment.byteEnd !== null
          ? fragment.byteEnd
          : null,
      lineStart:
        fragment.lineStart !== undefined && fragment.lineStart !== null
          ? fragment.lineStart
          : null,
      lineEnd:
        fragment.lineEnd !== undefined && fragment.lineEnd !== null
          ? fragment.lineEnd
          : null
    }));
}

function toPosixPath(relativePath: string): string {
  return relativePath.split(path.sep).join('/');
}

function convertSkippedFile(entry: NativeSkippedFile): SkippedFile {
  return {
    path: toPosixPath(entry.path),
    reason: entry.reason,
    size: entry.size ?? undefined,
    message: entry.message ?? undefined
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
      byte_start INTEGER,
      byte_end INTEGER,
      line_start INTEGER,
      line_end INTEGER,
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
    CREATE TABLE IF NOT EXISTS meta (
      key TEXT PRIMARY KEY,
      value TEXT NOT NULL,
      updated_at INTEGER NOT NULL
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

  ensureFileChunkMetadataColumns(db);
  ensureHitsColumns(db);
}

function ensureFileChunkMetadataColumns(db: DatabaseInstance): void {
  const columns = db.prepare("PRAGMA table_info('file_chunks')").all() as { name: string }[];
  const columnNames = new Set(columns.map((column) => column.name));

  const migrations: Array<{ name: string; statement: string }> = [
    { name: 'byte_start', statement: 'ALTER TABLE file_chunks ADD COLUMN byte_start INTEGER' },
    { name: 'byte_end', statement: 'ALTER TABLE file_chunks ADD COLUMN byte_end INTEGER' },
    { name: 'line_start', statement: 'ALTER TABLE file_chunks ADD COLUMN line_start INTEGER' },
    { name: 'line_end', statement: 'ALTER TABLE file_chunks ADD COLUMN line_end INTEGER' }
  ];

  for (const migration of migrations) {
    if (!columnNames.has(migration.name)) {
      db.prepare(migration.statement).run();
    }
  }
}

function ensureHitsColumns(db: DatabaseInstance): void {
  const chunkColumns = db.prepare("PRAGMA table_info('file_chunks')").all() as { name: string }[];
  const chunkColumnNames = new Set(chunkColumns.map((column) => column.name));
  
  if (!chunkColumnNames.has('hits')) {
    db.prepare('ALTER TABLE file_chunks ADD COLUMN hits INTEGER DEFAULT 0').run();
  }

  const nodeColumns = db.prepare("PRAGMA table_info('code_graph_nodes')").all() as { name: string }[];
  const nodeColumnNames = new Set(nodeColumns.map((column) => column.name));
  
  if (!nodeColumnNames.has('hits')) {
    db.prepare('ALTER TABLE code_graph_nodes ADD COLUMN hits INTEGER DEFAULT 0').run();
  }
}

async function getCurrentGitCommitSha(root: string): Promise<string | null> {
  try {
    const { stdout } = await execFileAsync('git', ['rev-parse', 'HEAD'], { 
      cwd: root,
      maxBuffer: 1024 * 1024
    });
    return stdout.trim() || null;
  } catch {
    return null;
  }
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
  chunkRefreshPaths: Set<string>;
  graphNodeJobs: PendingGraphNode[];
  graphEdgeJobs: PendingGraphEdge[];
  sanitizeContent: ContentSanitizer;
  embeddingConfig: EmbeddingConfig;
  graphConfig: GraphConfig;
  storeFileContent: boolean;
  pendingAnalysis: PendingAnalysis[];
  pipelineConcurrency: number;
}

async function processNativeEntries({
  absoluteRoot,
  nativeResult,
  existingByPath,
  files,
  seenPaths,
  chunkRefreshPaths,
  graphNodeJobs,
  graphEdgeJobs,
  sanitizeContent,
  embeddingConfig,
  graphConfig,
  storeFileContent,
  pendingAnalysis,
  pipelineConcurrency
}: ProcessNativeEntriesParams): Promise<void> {
  await runWithConcurrency(nativeResult.files, pipelineConcurrency, async (entry) => {
    await processNativeEntry({
      absoluteRoot,
      entry,
      existingByPath,
      files,
      seenPaths,
      chunkRefreshPaths,
      graphNodeJobs,
      graphEdgeJobs,
      sanitizeContent,
      embeddingConfig,
      graphConfig,
      storeFileContent,
      pendingAnalysis
    });
  });
}

interface ProcessNativeEntryParams {
  absoluteRoot: string;
  entry: NativeFileEntry;
  existingByPath: Map<string, FileRow>;
  files: FileRow[];
  seenPaths: Set<string>;
  chunkRefreshPaths: Set<string>;
  graphNodeJobs: PendingGraphNode[];
  graphEdgeJobs: PendingGraphEdge[];
  sanitizeContent: ContentSanitizer;
  embeddingConfig: EmbeddingConfig;
  graphConfig: GraphConfig;
  storeFileContent: boolean;
  pendingAnalysis: PendingAnalysis[];
}

async function processNativeEntry({
  absoluteRoot,
  entry,
  existingByPath,
  files,
  seenPaths,
  chunkRefreshPaths,
  graphNodeJobs,
  graphEdgeJobs,
  sanitizeContent,
  embeddingConfig,
  graphConfig,
  storeFileContent,
  pendingAnalysis
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

  const entryContent = entry.content ?? null;

  let content: string | null = null;
  if (!entry.isBinary && entryContent !== null) {
    content = await sanitizeContent({
      path: normalizedPath,
      absolutePath,
      root: absoluteRoot,
      content: entryContent
    });
  }

  seenPaths.add(normalizedPath);
  chunkRefreshPaths.add(normalizedPath);

  if (embeddingConfig.enabled && content) {
    const trimmedContent = content.trim();
    if (trimmedContent) {
      pendingAnalysis.push({
        path: normalizedPath,
        content: trimmedContent
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

  if (graphConfig.enabled && content) {
    const graphResult = extractGraphMetadata(normalizedPath, content);
    if (graphResult) {
      for (const node of graphResult.entities) {
        graphNodeJobs.push({
          id: node.id,
          path: node.path,
          kind: node.kind,
          name: node.name,
          signature: node.signature ?? null,
          rangeStart: node.rangeStart ?? null,
          rangeEnd: node.rangeEnd ?? null,
          metadata: node.metadata ? JSON.stringify(node.metadata) : null
        });
      }
      for (const edge of graphResult.edges) {
        graphEdgeJobs.push({
          id: edge.id,
          sourceId: edge.sourceId,
          targetId: edge.targetId,
          type: edge.type,
          sourcePath: edge.sourcePath ?? null,
          targetPath: edge.targetPath ?? null,
          metadata: edge.metadata ? JSON.stringify(edge.metadata) : null
        });
      }
    }
  }

  content = null;
}

interface PopulateChunkJobsParams {
  nativeModule: NativeModule;
  pendingAnalysis: PendingAnalysis[];
  chunkJobs: PendingChunk[];
  embeddingConfig: EmbeddingConfig;
}

async function populateChunkJobs({
  nativeModule,
  pendingAnalysis,
  chunkJobs,
  embeddingConfig
}: PopulateChunkJobsParams): Promise<void> {
  if (!embeddingConfig.enabled || pendingAnalysis.length === 0) {
    return;
  }

  if (!hasNativeBatchAnalysisFlow(nativeModule)) {
    throw new Error('[index-mcp] Native module is missing batch analysis support.');
  }

  const batchResult: NativeBatchAnalysisResult = await nativeModule.analyzeFileContentBatch({
    files: pendingAnalysis.map((job) => ({
      path: job.path,
      content: job.content
    })),
    chunkSizeTokens: embeddingConfig.chunkSizeTokens,
    chunkOverlapTokens: embeddingConfig.chunkOverlapTokens
  });

  if (!batchResult.files?.length) {
    throw new Error('[index-mcp] Native batch analysis returned no results.');
  }

  const fragmentsByPath = new Map<string, NormalizedFragment[]>();
  for (const file of batchResult.files) {
    const normalized = normalizeNativeFragments(file.chunks);
    if (!normalized.length) {
      throw new Error(
        `[index-mcp] Native batch analysis produced empty fragments for ${file.path}`
      );
    }
    fragmentsByPath.set(file.path, normalized);
  }

  for (const job of pendingAnalysis) {
    const fragments = fragmentsByPath.get(job.path);
    if (!fragments) {
      throw new Error(
        `[index-mcp] Native batch analysis did not return fragments for ${job.path}`
      );
    }
    appendFragmentsToChunkJobs(job.path, fragments, job.content, chunkJobs, embeddingConfig.model);
  }

  pendingAnalysis.length = 0;
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
    throw new Error(`Ingest root must be a directory: ${absoluteRoot}. Please ensure the path points to a valid directory.`);
  }

  const dbPath = path.join(absoluteRoot, databaseName);
  const startTime = Date.now();

  const db: DatabaseInstance = new Database(dbPath);
  try {
    ensureSchema(db);

    const sanitizeContent = await loadSanitizer(absoluteRoot, options.contentSanitizer);

    const baseSelectColumns = `path, size, modified, hash, last_indexed_at as lastIndexedAt${
      storeFileContent ? ', content' : ''
    }`;
    let selectSql = `SELECT ${baseSelectColumns} FROM files`;
    const selectArgs: string[] = [];
    if (usingTargetPaths) {
      const placeholders = targetPaths.map(() => '?').join(', ');
      selectSql += ` WHERE path IN (${placeholders})`;
      selectArgs.push(...targetPaths);
    }

    const selectStmt = db.prepare(selectSql);
    const existingByPath = new Map<string, FileRow>();
    for (const row of selectStmt.iterate(...selectArgs)) {
      const record = row as {
        path: string;
        size: number;
        modified: number;
        hash: string;
        lastIndexedAt: number;
        content?: string | null;
      };
      existingByPath.set(record.path, {
        path: record.path,
        size: record.size,
        modified: record.modified,
        hash: record.hash,
        lastIndexedAt: record.lastIndexedAt,
        content: storeFileContent ? record.content ?? null : null
      });
    }

    const needsTextContent = storeFileContent || embeddingConfig.enabled || graphConfig.enabled;
    const nativeModule = await loadNativeModule();

    if (!hasNativeBatchAnalysisFlow(nativeModule)) {
      throw new Error('[index-mcp] Native module must expose analyzeFileContentBatch.');
    }

    const files: FileRow[] = [];
    const skipped: SkippedFile[] = [];
    const seenPaths = new Set<string>();
    const chunkJobs: PendingChunk[] = [];
    const chunkRefreshPaths = new Set<string>();
    const graphNodeJobs: PendingGraphNode[] = [];
    const graphEdgeJobs: PendingGraphEdge[] = [];
    const pendingAnalysis: PendingAnalysis[] = [];
    const pipelineConcurrency = resolvePipelineConcurrency();

    const existingPathsSet = usingTargetPaths
      ? new Set<string>(targetPaths.filter((p) => existingByPath.has(p)))
      : new Set<string>(existingByPath.keys());

    if (hasNativeMetadataFlow(nativeModule)) {
      let metadataResult;
      try {
        metadataResult = await nativeModule.scanRepoMetadata({
          root: absoluteRoot,
          include: searchPatterns,
          exclude: excludeGlobs,
          maxFileSizeBytes: maxFileSizeBytes
        });
      } catch (error) {
        const message = error instanceof Error ? error.message : String(error);
        throw new Error(`[index-mcp] Native metadata scan failed: ${message}`);
      }

      metadataResult.skipped.forEach((entry) => {
        skipped.push(convertSkippedFile(entry));
      });

      const pendingReadPaths = new Set<string>();

      for (const entry of metadataResult.entries) {
        const normalizedPath = toPosixPath(entry.path);
        const existing = existingByPath.get(normalizedPath);

        if (existing && existing.size === entry.size && existing.modified === entry.modified) {
          await processNativeEntry({
            absoluteRoot,
            entry: {
              path: normalizedPath,
              size: entry.size,
              modified: entry.modified,
              hash: existing.hash,
              content: existing.content,
              isBinary: existing.content === null
            },
            existingByPath,
            files,
            seenPaths,
            chunkRefreshPaths,
            graphNodeJobs,
            graphEdgeJobs,
            sanitizeContent,
            embeddingConfig,
            graphConfig,
            storeFileContent,
            pendingAnalysis
          });
        } else {
          pendingReadPaths.add(normalizedPath);
        }
      }

      if (pendingReadPaths.size > 0) {
        let readResult;
        try {
          readResult = await nativeModule.readRepoFiles({
            root: absoluteRoot,
            paths: Array.from(pendingReadPaths),
            maxFileSizeBytes: maxFileSizeBytes,
            needsContent: needsTextContent
          });
        } catch (error) {
          const message = error instanceof Error ? error.message : String(error);
          throw new Error(`[index-mcp] Native file load failed: ${message}`);
        }

        readResult.skipped.forEach((entry) => {
          skipped.push(convertSkippedFile(entry));
        });

        await runWithConcurrency(readResult.files, pipelineConcurrency, async (entry) => {
          const normalizedEntry: NativeFileEntry = {
            ...entry,
            path: toPosixPath(entry.path)
          };
          await processNativeEntry({
            absoluteRoot,
            entry: normalizedEntry,
            existingByPath,
            files,
            seenPaths,
            chunkRefreshPaths,
            graphNodeJobs,
            graphEdgeJobs,
            sanitizeContent,
            embeddingConfig,
            graphConfig,
            storeFileContent,
            pendingAnalysis
          });
        });
      }
    } else {
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

      nativeResult.skipped.forEach((entry) => {
        skipped.push(convertSkippedFile(entry));
      });

      await processNativeEntries({
        absoluteRoot,
        nativeResult,
        existingByPath,
        files,
        seenPaths,
        chunkRefreshPaths,
        graphNodeJobs,
        graphEdgeJobs,
        sanitizeContent,
        embeddingConfig,
        graphConfig,
        storeFileContent,
        pendingAnalysis,
        pipelineConcurrency
      });
    }

    await populateChunkJobs({
      nativeModule,
      pendingAnalysis,
      chunkJobs,
      embeddingConfig
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

      const batchTexts: string[] = [];
      for (const [model, jobs] of jobsByModel) {
        for (let i = 0; i < jobs.length; i += embeddingConfig.batchSize) {
          const end = Math.min(i + embeddingConfig.batchSize, jobs.length);
          batchTexts.length = 0;
          for (let cursor = i; cursor < end; cursor += 1) {
            batchTexts.push(jobs[cursor].content);
          }
          if (batchTexts.length === 0) {
            continue;
          }
          const embeddings = await embedTexts(batchTexts, { model });
          let embeddingIndex = 0;
          for (let cursor = i; cursor < end; cursor += 1) {
            jobs[cursor].embedding = float32ArrayToBuffer(embeddings[embeddingIndex]);
            embeddingIndex += 1;
          }
          embeddedChunkCount += batchTexts.length;
        }
      }
      batchTexts.length = 0;
      jobsByModel.clear();
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
      `INSERT INTO file_chunks (id, path, chunk_index, content, embedding, embedding_model, byte_start, byte_end, line_start, line_end)
       VALUES (@id, @path, @chunkIndex, @content, @embedding, @model, @byteStart, @byteEnd, @lineStart, @lineEnd)`
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

    const upsertMetaStmt = db.prepare(
      `INSERT INTO meta (key, value, updated_at)
       VALUES (@key, @value, @updatedAt)
       ON CONFLICT(key) DO UPDATE SET
         value = excluded.value,
         updated_at = excluded.updated_at`
    );

    const ingestionId = crypto.randomUUID();
    const endTime = Date.now();
    
    const currentCommitSha = await getCurrentGitCommitSha(absoluteRoot);

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
          model: chunk.model,
          byteStart: chunk.byteStart ?? null,
          byteEnd: chunk.byteEnd ?? null,
          lineStart: chunk.lineStart ?? null,
          lineEnd: chunk.lineEnd ?? null
        });
      }
      if (graphConfig.enabled) {
        for (const node of graphNodeJobs) {
          insertGraphNodeStmt.run(node);
          graphNodeCount += 1;
        }
        for (const edge of graphEdgeJobs) {
          insertGraphEdgeStmt.run(edge);
          graphEdgeCount += 1;
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
      
      if (currentCommitSha) {
        upsertMetaStmt.run({
          key: 'commit_sha',
          value: currentCommitSha,
          updatedAt: endTime
        });
      }
      
      upsertMetaStmt.run({
        key: 'indexed_at',
        value: endTime.toString(),
        updatedAt: endTime
      });
    });

    transaction();

    db.close();

    let dbStats = await fs.stat(dbPath);
    let evictionResult: { chunks: number; nodes: number } | undefined;

    // Check if eviction is needed
    const autoEvict = options.autoEvict ?? false;
    const maxDatabaseSizeBytes = options.maxDatabaseSizeBytes ?? 150 * 1024 * 1024; // 150 MB default

    if (autoEvict && dbStats.size > maxDatabaseSizeBytes) {
      const { evictLeastUsed } = await import('./eviction.js');
      const evicted = await evictLeastUsed({
        root: absoluteRoot,
        databaseName: databaseName,
        maxSizeBytes: maxDatabaseSizeBytes
      });
      
      if (evicted.wasEvictionNeeded) {
        evictionResult = {
          chunks: evicted.evictedChunks,
          nodes: evicted.evictedNodes
        };
        dbStats = await fs.stat(dbPath);
      }
    }

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
      graphEdgeCount,
      evicted: evictionResult
    };
  } catch (error) {
    db.close();
    throw error;
  }
}
