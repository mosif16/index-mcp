import Database from 'better-sqlite3';
import path from 'node:path';

import { getContextBundle, type ContextBundleResult } from './context-bundle.js';
import { semanticSearch, type SemanticSearchResult } from './search.js';
import { DEFAULT_DB_FILENAME } from './constants.js';

interface SymbolRow {
  name: string;
  file: string;
  startLine: number;
  endLine: number;
  hits: number;
}

export interface AutoLookupOptions {
  root: string;
  databaseName?: string;
  query?: string;
  file?: string;
  symbolName?: string;
  budgetTokens?: number;
  limit?: number;
}

export interface AutoLookupResult {
  databasePath: string;
  mode: 'symbol' | 'file' | 'search' | 'none';
  summary: string;
  bundle?: ContextBundleResult;
  search?: SemanticSearchResult;
  confidence: number;
}

const LOW_CONFIDENCE_THRESHOLD = 0.35;

function resolveDatabasePath(root: string, databaseName?: string): string {
  const absoluteRoot = path.resolve(root);
  return path.join(absoluteRoot, databaseName ?? DEFAULT_DB_FILENAME);
}

function findSymbolMatch(db: Database.Database, symbolName: string, file?: string): SymbolRow | null {
  const trimmed = symbolName.trim();
  if (!trimmed) {
    return null;
  }
  const matcher = db.prepare(
    `SELECT name, file, start_line as startLine, end_line as endLine, hits
       FROM symbols
      WHERE LOWER(name) = LOWER(?)${file ? ' AND file = ?' : ''}
      ORDER BY hits DESC, start_line ASC
      LIMIT 1`
  );
  const row = file ? (matcher.get(trimmed, file) as SymbolRow | undefined) : (matcher.get(trimmed) as SymbolRow | undefined);
  return row ?? null;
}

export async function autoLookup(options: AutoLookupOptions): Promise<AutoLookupResult> {
  const databasePath = resolveDatabasePath(options.root, options.databaseName);
  const db = new Database(databasePath, { readonly: true, fileMustExist: true });
  let symbolMatch: SymbolRow | null = null;
  try {
    if (options.symbolName) {
      symbolMatch = findSymbolMatch(db, options.symbolName, options.file);
      if (!symbolMatch && options.file) {
        // Attempt partial match when file hint provided.
        symbolMatch = db
          .prepare(
            `SELECT name, file, start_line as startLine, end_line as endLine, hits
               FROM symbols
              WHERE file = ? AND LOWER(name) LIKE LOWER(?)
              ORDER BY hits DESC, start_line ASC
              LIMIT 1`
          )
          .get(options.file, `${options.symbolName.trim().toLowerCase()}%`) as SymbolRow | undefined ?? null;
      }
    }
  } finally {
    db.close();
  }

  if (symbolMatch) {
    const bundle = await getContextBundle({
      root: options.root,
      databaseName: options.databaseName,
      file: symbolMatch.file,
      symbol: { name: symbolMatch.name },
      budgetTokens: options.budgetTokens
    });
    const summary = `Focused context for symbol '${symbolMatch.name}' in ${symbolMatch.file} (${bundle.estimatedTokens}/${bundle.tokenBudget} tokens).`;
    return {
      databasePath,
      mode: 'symbol',
      summary,
      bundle,
      confidence: 1
    };
  }

  if (options.file) {
    const bundle = await getContextBundle({
      root: options.root,
      databaseName: options.databaseName,
      file: options.file,
      symbol: options.symbolName ? { name: options.symbolName } : undefined,
      budgetTokens: options.budgetTokens
    });
    const summary = `Context bundle for ${options.file} (${bundle.estimatedTokens}/${bundle.tokenBudget} tokens).`;
    return {
      databasePath,
      mode: 'file',
      summary,
      bundle,
      confidence: 0.75
    };
  }

  if (options.query) {
    const search = await semanticSearch({
      root: options.root,
      query: options.query,
      databaseName: options.databaseName,
      limit: options.limit
    });

    const topMatch = search.results[0] ?? null;
    const confidence = topMatch?.confidence ?? 0;

    if (!topMatch) {
      const summary = 'Search completed but no matching snippets were found.';
      return {
        databasePath,
        mode: 'search',
        summary,
        search,
        confidence
      };
    }

    const bundle = await getContextBundle({
      root: options.root,
      databaseName: options.databaseName,
      file: topMatch.file,
      budgetTokens: options.budgetTokens
    });

    const tokenDescriptor = `${bundle.estimatedTokens}/${bundle.tokenBudget} tokens`;
    const summary = confidence < LOW_CONFIDENCE_THRESHOLD
      ? `Low-confidence search match in ${topMatch.file}; review recommended. (${tokenDescriptor})`
      : `Search matched ${topMatch.file} with confidence ${(confidence * 100).toFixed(1)}% (${tokenDescriptor}).`;

    return {
      databasePath,
      mode: 'search',
      summary,
      bundle,
      search,
      confidence
    };
  }

  return {
    databasePath,
    mode: 'none',
    summary: 'No lookup target provided.',
    confidence: 0
  };
}
