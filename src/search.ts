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
  content: string;
  embeddingModel: string;
}

export interface SemanticSearchResult {
  databasePath: string;
  embeddingModel: string | null;
  totalChunks: number;
  evaluatedChunks: number;
  results: SemanticSearchMatch[];
}

const DEFAULT_RESULT_LIMIT = 8;

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
  const limit = options.limit ?? DEFAULT_RESULT_LIMIT;

  const db = new Database(dbPath, { readonly: true });
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

    const modelRows = db
      .prepare(
        `SELECT id, path, chunk_index as chunkIndex, content, embedding, embedding_model as embeddingModel
         FROM file_chunks
         WHERE embedding_model = ?`
      )
      .all(requestedModel) as ChunkRow[];

    if (!modelRows.length) {
      return {
        databasePath: dbPath,
        embeddingModel: requestedModel,
        totalChunks,
        evaluatedChunks: 0,
        results: []
      };
    }

    const [queryEmbedding] = await embedTexts([trimmedQuery], { model: requestedModel });

    const topMatches: SemanticSearchMatch[] = [];
    for (const row of modelRows) {
      const chunkEmbedding = bufferToFloat32Array(row.embedding);
      const score = dotProduct(queryEmbedding, chunkEmbedding);
      insertIntoTopMatches(
        topMatches,
        {
          path: row.path,
          chunkIndex: row.chunkIndex,
          content: row.content,
          score,
          embeddingModel: row.embeddingModel
        },
        limit
      );
    }

    const results = limit > 0 ? [...topMatches].reverse() : [];

    return {
      databasePath: dbPath,
      embeddingModel: requestedModel,
      totalChunks,
      evaluatedChunks: modelRows.length,
      results
    };
  } finally {
    db.close();
  }
}
