import Database from 'better-sqlite3';
import { promises as fs } from 'node:fs';
import path from 'node:path';

import { bufferToFloat32Array, embedTexts } from './embedding.js';
import { DEFAULT_DB_FILENAME } from './constants.js';

interface ChunkRow {
  id: string;
  path: string;
  chunkIndex: number;
  content: string;
  embedding: Buffer;
  embeddingModel: string;
  byteStart: number | null;
  byteEnd: number | null;
  lineStart: number | null;
  lineEnd: number | null;
}

export interface SemanticSearchOptions {
  root: string;
  query: string;
  databaseName?: string;
  limit?: number;
  model?: string;
}

export interface SemanticSearchMatch {
  path: string;
  chunkIndex: number;
  score: number;
  normalizedScore: number;
  language: string | null;
  classification: 'function' | 'comment' | 'code';
  content: string;
  embeddingModel: string;
  byteStart: number | null;
  byteEnd: number | null;
  lineStart: number | null;
  lineEnd: number | null;
  contextBefore: string | null;
  contextAfter: string | null;
}

export interface SemanticSearchResult {
  databasePath: string;
  embeddingModel: string | null;
  totalChunks: number;
  evaluatedChunks: number;
  results: SemanticSearchMatch[];
}

const DEFAULT_RESULT_LIMIT = 8;
const MAX_RESULT_LIMIT = 50;
const CONTEXT_LINE_PADDING = 2;

const LANGUAGE_BY_EXTENSION: Record<string, string> = {
  '.ts': 'TypeScript',
  '.tsx': 'TypeScript',
  '.js': 'JavaScript',
  '.jsx': 'JavaScript',
  '.mjs': 'JavaScript',
  '.cjs': 'JavaScript',
  '.json': 'JSON',
  '.py': 'Python',
  '.rs': 'Rust',
  '.go': 'Go',
  '.java': 'Java',
  '.rb': 'Ruby',
  '.php': 'PHP',
  '.swift': 'Swift',
  '.kt': 'Kotlin',
  '.cs': 'C#',
  '.cpp': 'C++',
  '.cc': 'C++',
  '.c': 'C',
  '.h': 'C/C++ Header',
  '.hpp': 'C++ Header',
  '.md': 'Markdown',
  '.yml': 'YAML',
  '.yaml': 'YAML'
};

interface FileCacheEntry {
  content: string | null;
  lines: string[] | null;
  trimmed: string | null;
  trimmedLines: string[] | null;
}

function dotProduct(a: Float32Array, b: Float32Array): number {
  if (a.length !== b.length) {
    throw new Error('Mismatched embedding dimensions encountered during search');
  }

  let sum = 0;
  for (let i = 0; i < a.length; i += 1) {
    sum += a[i] * b[i];
  }
  return sum;
}

function insertIntoTopMatches(
  sortedMatches: SemanticSearchMatch[],
  candidate: SemanticSearchMatch,
  limit: number
): void {
  if (limit <= 0) {
    return;
  }

  const insertionIndex = sortedMatches.findIndex((match) => match.score > candidate.score);
  if (insertionIndex === -1) {
    sortedMatches.push(candidate);
  } else {
    sortedMatches.splice(insertionIndex, 0, candidate);
  }

  if (sortedMatches.length > limit) {
    sortedMatches.shift();
  }
}

function normalizeResultLimit(limit: number | undefined): number {
  if (limit === undefined || Number.isNaN(limit)) {
    return DEFAULT_RESULT_LIMIT;
  }

  if (!Number.isFinite(limit)) {
    throw new Error('Result limit must be a finite number');
  }

  if (limit <= 0) {
    return 0;
  }

  const coerced = Math.floor(limit);
  const positive = Math.max(coerced, 1);
  return Math.min(positive, MAX_RESULT_LIMIT);
}

function detectLanguageFromPath(filePath: string): string | null {
  const ext = path.extname(filePath).toLowerCase();
  return LANGUAGE_BY_EXTENSION[ext] ?? null;
}

function classifySnippetText(snippet: string): 'function' | 'comment' | 'code' {
  const trimmed = snippet.trim();
  if (!trimmed) {
    return 'code';
  }

  const lines = trimmed.split(/\r?\n/u);
  const commentLines = lines.filter((line) => /^\s*(?:\/\/|#|\/\*|\*|<!--)/u.test(line));
  if (commentLines.length === lines.length) {
    return 'comment';
  }

  if (/\bfunction\b|=>|class\s+\w+|def\s+\w+/u.test(trimmed)) {
    return 'function';
  }

  return 'code';
}

export async function semanticSearch(options: SemanticSearchOptions): Promise<SemanticSearchResult> {
  const trimmedQuery = options.query.trim();
  if (!trimmedQuery) {
    throw new Error('Query must not be empty');
  }

  const absoluteRoot = path.resolve(options.root);
  const stats = await fs.stat(absoluteRoot);
  if (!stats.isDirectory()) {
    throw new Error(`Semantic search root must be a directory: ${absoluteRoot}`);
  }

  const dbPath = path.join(absoluteRoot, options.databaseName ?? DEFAULT_DB_FILENAME);
  const limit = normalizeResultLimit(options.limit);

  const db = new Database(dbPath, { fileMustExist: true });
  try {
    const totalChunkRow = db
      .prepare('SELECT COUNT(*) as count FROM file_chunks')
      .get() as { count: number } | undefined;
    const totalChunks = totalChunkRow?.count ?? 0;

    if (totalChunks === 0) {
      return {
        databasePath: dbPath,
        embeddingModel: options.model ?? null,
        totalChunks,
        evaluatedChunks: 0,
        results: []
      };
    }

    const availableModelRows = db
      .prepare('SELECT DISTINCT embedding_model as embeddingModel FROM file_chunks')
      .all() as { embeddingModel: string }[];
    const availableModels = new Set(availableModelRows.map((row) => row.embeddingModel));
    const requestedModel = options.model ?? (availableModels.size === 1 ? [...availableModels][0] : null);

    if (!requestedModel) {
      throw new Error(
        `Multiple embedding models found (${[...availableModels].join(', ')}). Specify the desired model in the request.`
      );
    }

    if (!availableModels.has(requestedModel)) {
      throw new Error(
        `No chunks indexed with embedding model '${requestedModel}'. Available models: ${[...availableModels].join(', ')}`
      );
    }

    const chunkStmt = db.prepare(
      `SELECT
         id,
         path,
         chunk_index as chunkIndex,
         content,
         embedding,
         embedding_model as embeddingModel,
         byte_start as byteStart,
         byte_end as byteEnd,
         line_start as lineStart,
         line_end as lineEnd
       FROM file_chunks
       WHERE embedding_model = ?`
    );

    const fileContentStmt = db.prepare('SELECT content FROM files WHERE path = ?');
    const fileCache = new Map<string, FileCacheEntry>();

    const getFileEntry = (filePath: string): FileCacheEntry => {
      const cached = fileCache.get(filePath);
      if (cached) {
        return cached;
      }

      const row = fileContentStmt.get(filePath) as { content: string | null } | undefined;
      const content = row?.content ?? null;
      const lines = content ? content.split(/\r?\n/) : null;
      const trimmed = content ? content.trim() : null;
      const trimmedLines = trimmed ? trimmed.split(/\r?\n/) : null;
      const entry: FileCacheEntry = {
        content,
        lines,
        trimmed,
        trimmedLines
      };
      fileCache.set(filePath, entry);
      return entry;
    };

    const deriveMetadataFromContent = (
      entry: FileCacheEntry,
      snippet: string
    ): { byteStart: number | null; byteEnd: number | null; lineStart: number | null; lineEnd: number | null } => {
      if (!entry.trimmed) {
        return { byteStart: null, byteEnd: null, lineStart: null, lineEnd: null };
      }

      const startIndex = entry.trimmed.indexOf(snippet);
      if (startIndex === -1) {
        return { byteStart: null, byteEnd: null, lineStart: null, lineEnd: null };
      }

      const endIndex = startIndex + snippet.length;
      const byteStart = Buffer.byteLength(entry.trimmed.slice(0, startIndex), 'utf8');
      const byteEnd = Buffer.byteLength(entry.trimmed.slice(0, endIndex), 'utf8');

      const preSnippet = entry.trimmed.slice(0, startIndex);
      const lineStart = preSnippet ? preSnippet.split('\n').length : 1;
      const snippetLineCount = snippet ? snippet.split('\n').length : 1;
      const lineEnd = lineStart + Math.max(0, snippetLineCount - 1);

      return { byteStart, byteEnd, lineStart, lineEnd };
    };

    const extractContext = (
      entry: FileCacheEntry,
      startLine: number | null,
      endLine: number | null
    ): { before: string | null; after: string | null } => {
      if (!entry.trimmedLines || startLine === null || endLine === null) {
        return { before: null, after: null };
      }

      const beforeStart = Math.max(0, startLine - 1 - CONTEXT_LINE_PADDING);
      const beforeEnd = Math.max(0, startLine - 1);
      const afterStart = Math.min(entry.trimmedLines.length, endLine);
      const afterEnd = Math.min(entry.trimmedLines.length, endLine + CONTEXT_LINE_PADDING);

      const beforeLines = beforeEnd > beforeStart ? entry.trimmedLines.slice(beforeStart, beforeEnd) : [];
      const afterLines = afterEnd > afterStart ? entry.trimmedLines.slice(afterStart, afterEnd) : [];

      return {
        before: beforeLines.length > 0 ? beforeLines.join('\n') : null,
        after: afterLines.length > 0 ? afterLines.join('\n') : null
      };
    };

    const [queryEmbedding] = await embedTexts([trimmedQuery], { model: requestedModel });

    const topMatches: SemanticSearchMatch[] = [];
    const chunkIdsByMatch = new WeakMap<SemanticSearchMatch, string>();
    let evaluatedChunks = 0;

    for (const row of chunkStmt.iterate(requestedModel) as Iterable<ChunkRow>) {
      evaluatedChunks += 1;
      const chunkEmbedding = bufferToFloat32Array(row.embedding);
      const score = dotProduct(queryEmbedding, chunkEmbedding);
      const normalizedScore = Math.max(0, Math.min(1, (score + 1) / 2));
      const matchCandidate: SemanticSearchMatch = {
        path: row.path,
        chunkIndex: row.chunkIndex,
        content: row.content,
        score,
        normalizedScore,
        language: detectLanguageFromPath(row.path),
        classification: classifySnippetText(row.content),
        embeddingModel: row.embeddingModel,
        byteStart: row.byteStart ?? null,
        byteEnd: row.byteEnd ?? null,
        lineStart: row.lineStart ?? null,
        lineEnd: row.lineEnd ?? null,
        contextBefore: null,
        contextAfter: null
      };

      insertIntoTopMatches(topMatches, matchCandidate, limit);
      chunkIdsByMatch.set(matchCandidate, row.id);
    }

    const results = limit > 0 ? [...topMatches].reverse() : [];

    for (const match of results) {
      const entry = getFileEntry(match.path);

      const needsMetadataFallback =
        match.byteStart === null || match.byteEnd === null || match.lineStart === null || match.lineEnd === null;

      if (needsMetadataFallback) {
        const derived = deriveMetadataFromContent(entry, match.content);
        match.byteStart = match.byteStart ?? derived.byteStart;
        match.byteEnd = match.byteEnd ?? derived.byteEnd;
        match.lineStart = match.lineStart ?? derived.lineStart;
        match.lineEnd = match.lineEnd ?? derived.lineEnd;
      }

      const context = extractContext(entry, match.lineStart, match.lineEnd);
      match.contextBefore = context.before;
      match.contextAfter = context.after;

      if (!match.language) {
        match.language = detectLanguageFromPath(match.path);
      }
    }

    const updateHitsStmt = db.prepare(
      'UPDATE file_chunks SET hits = COALESCE(hits, 0) + 1 WHERE id = ?'
    );
    
    for (const match of results) {
      const chunkId = chunkIdsByMatch.get(match);
      if (chunkId) {
        updateHitsStmt.run(chunkId);
      }
    }

    return {
      databasePath: dbPath,
      embeddingModel: requestedModel,
      totalChunks,
      evaluatedChunks,
      results
    };
  } finally {
    db.close();
  }
}
