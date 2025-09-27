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

function sortByScoreDescending(matches: SemanticSearchMatch[]): SemanticSearchMatch[] {
  return matches.sort((left, right) => right.score - left.score);
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
    const chunkRows = db
      .prepare(
        `SELECT id, path, chunk_index as chunkIndex, content, embedding, embedding_model as embeddingModel
         FROM file_chunks`
      )
      .all() as ChunkRow[];

    if (!chunkRows.length) {
      return {
        databasePath: dbPath,
        embeddingModel: options.model ?? null,
        totalChunks: 0,
        evaluatedChunks: 0,
        results: []
      };
    }

    const availableModels = new Set(chunkRows.map((row) => row.embeddingModel));
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

    const modelRows = chunkRows.filter((row) => row.embeddingModel === requestedModel);

    if (!modelRows.length) {
      return {
        databasePath: dbPath,
        embeddingModel: requestedModel,
        totalChunks: chunkRows.length,
        evaluatedChunks: 0,
        results: []
      };
    }

    const [queryEmbedding] = await embedTexts([trimmedQuery], { model: requestedModel });

    const matches: SemanticSearchMatch[] = modelRows.map((row) => {
      const chunkEmbedding = bufferToFloat32Array(row.embedding);
      const score = dotProduct(queryEmbedding, chunkEmbedding);
      return {
        path: row.path,
        chunkIndex: row.chunkIndex,
        content: row.content,
        score,
        embeddingModel: row.embeddingModel
      };
    });

    const sorted = sortByScoreDescending(matches).slice(0, limit);

    return {
      databasePath: dbPath,
      embeddingModel: requestedModel,
      totalChunks: chunkRows.length,
      evaluatedChunks: modelRows.length,
      results: sorted
    };
  } finally {
    db.close();
  }
}
