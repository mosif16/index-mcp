import Database from 'better-sqlite3';
import path from 'node:path';

import { DEFAULT_DB_FILENAME } from './constants.js';

interface SnippetRow {
  id: number;
  file: string;
  text: string;
  startLine: number;
  endLine: number;
  score: number;
}

export interface SemanticSearchOptions {
  root: string;
  query: string;
  databaseName?: string;
  limit?: number;
}

export interface SemanticSearchMatch {
  file: string;
  snippetId: number;
  score: number;
  confidence: number;
  text: string;
  startLine: number;
  endLine: number;
}

export interface SemanticSearchResult {
  databasePath: string;
  totalSnippets: number;
  evaluatedSnippets: number;
  results: SemanticSearchMatch[];
  query: string;
}

const DEFAULT_RESULT_LIMIT = 8;
const MAX_RESULT_LIMIT = 50;

function normalizeResultLimit(limit: number | undefined): number {
  if (limit === undefined || Number.isNaN(limit)) {
    return DEFAULT_RESULT_LIMIT;
  }
  if (!Number.isFinite(limit)) {
    throw new Error('Result limit must be a finite number');
  }
  const coerced = Math.floor(limit);
  if (coerced <= 0) {
    return DEFAULT_RESULT_LIMIT;
  }
  return Math.min(coerced, MAX_RESULT_LIMIT);
}

function buildFtsQuery(input: string): string {
  const tokens = input
    .split(/\s+/)
    .map((token) => token.trim())
    .filter(Boolean);
  if (tokens.length === 0) {
    return '';
  }
  return tokens
    .map((token) => `"${token.replace(/"/g, '""')}"`)
    .join(' AND ');
}

function computeConfidence(score: number): number {
  const sanitized = Number.isFinite(score) ? score : Number.POSITIVE_INFINITY;
  const normalized = Math.max(0, sanitized);
  return 1 / (1 + normalized);
}

function updateSnippetHits(db: Database.Database, ids: number[]): void {
  if (!ids.length) {
    return;
  }
  const placeholders = ids.map(() => '?').join(', ');
  db.prepare(`UPDATE snippets SET hits = hits + 1 WHERE id IN (${placeholders})`).run(...ids);
}

export async function semanticSearch(options: SemanticSearchOptions): Promise<SemanticSearchResult> {
  const trimmedQuery = options.query.trim();
  if (!trimmedQuery) {
    throw new Error('Query must not be empty');
  }

  const absoluteRoot = path.resolve(options.root);
  const databasePath = path.join(absoluteRoot, options.databaseName ?? DEFAULT_DB_FILENAME);
  const limit = normalizeResultLimit(options.limit);

  const db = new Database(databasePath, { fileMustExist: true });
  try {
    const totalRow = db.prepare('SELECT COUNT(*) as count FROM snippets').get() as { count?: number } | undefined;
    const totalSnippets = totalRow?.count ?? 0;

    if (totalSnippets === 0) {
      return {
        databasePath,
        totalSnippets,
        evaluatedSnippets: 0,
        results: [],
        query: trimmedQuery
      };
    }

    const searchStmt = db.prepare(
      `SELECT snippets.id as id,
              snippets.file as file,
              snippets.text as text,
              snippets.start_line as startLine,
              snippets.end_line as endLine,
              bm25(snippets_fts) as score
         FROM snippets_fts
         JOIN snippets ON snippets_fts.rowid = snippets.id
         WHERE snippets_fts MATCH ?
         ORDER BY score ASC
         LIMIT ?`
    );

    const ftsQueryPrimary = buildFtsQuery(trimmedQuery);
    if (!ftsQueryPrimary) {
      throw new Error('Query must include searchable terms.');
    }

    let rows: SnippetRow[] = [];
    try {
      rows = searchStmt.all(ftsQueryPrimary, limit) as SnippetRow[];
    } catch (error) {
      const fallbackQuery = `"${trimmedQuery.replace(/"/g, '""')}"`;
      rows = searchStmt.all(fallbackQuery, limit) as SnippetRow[];
      if (!rows.length) {
        throw error instanceof Error
          ? new Error(`Search failed: ${error.message}`)
          : new Error(`Search failed: ${String(error)}`);
      }
    }

    const matches: SemanticSearchMatch[] = rows.map((row) => {
      const score = Number.isFinite(row.score) ? row.score : Number(row.score);
      return {
        file: row.file,
        snippetId: row.id,
        score,
        confidence: computeConfidence(score),
        text: row.text,
        startLine: row.startLine,
        endLine: row.endLine
      };
    });

    if (matches.length) {
      updateSnippetHits(
        db,
        matches.map((match) => match.snippetId)
      );
    }

    return {
      databasePath,
      totalSnippets,
      evaluatedSnippets: matches.length,
      results: matches,
      query: trimmedQuery
    };
  } finally {
    db.close();
  }
}
