import Database from 'better-sqlite3';
import path from 'node:path';

import { DEFAULT_DB_FILENAME } from './constants.js';

export interface ContextBundleSymbolSelector {
  name: string;
}

export interface ContextBundleOptions {
  root: string;
  databaseName?: string;
  file: string;
  symbol?: ContextBundleSymbolSelector;
  budgetTokens?: number;
}

export interface BundleSymbol {
  name: string;
  startLine: number;
  endLine: number;
  hits: number;
}

export interface BundleSnippet {
  snippetId: number;
  text: string;
  startLine: number;
  endLine: number;
}

export interface ContextBundleResult {
  databasePath: string;
  file: string;
  tokenBudget: number;
  estimatedTokens: number;
  focusSymbol: BundleSymbol | null;
  definitions: BundleSymbol[];
  snippets: BundleSnippet[];
  citations: Record<string, Array<[number, number]>>;
  warnings: string[];
}

interface SymbolRow {
  name: string;
  startLine: number;
  endLine: number;
  hits: number;
}

interface SnippetRow {
  id: number;
  text: string;
  startLine: number;
  endLine: number;
  hits: number;
}

const DEFAULT_TOKEN_BUDGET = 3000;
const MIN_TOKEN_BUDGET = 500;
const METADATA_TOKEN_RESERVE = 200;

function normalizeBudget(explicit?: number): number {
  if (explicit !== undefined && Number.isFinite(explicit)) {
    return Math.max(MIN_TOKEN_BUDGET, Math.floor(explicit));
  }
  const envValue = Number(process.env.INDEX_MCP_BUDGET_TOKENS);
  if (Number.isFinite(envValue) && envValue > 0) {
    return Math.max(MIN_TOKEN_BUDGET, Math.floor(envValue));
  }
  return DEFAULT_TOKEN_BUDGET;
}

function estimateTokens(text: string): number {
  if (!text) {
    return 0;
  }
  return Math.ceil(text.length / 4);
}

function toBundleSymbol(row: SymbolRow): BundleSymbol {
  return {
    name: row.name,
    startLine: row.startLine,
    endLine: row.endLine,
    hits: row.hits
  };
}

function buildCitations(file: string, snippets: BundleSnippet[]): Record<string, Array<[number, number]>> {
  const map: Record<string, Array<[number, number]>> = {};
  if (!snippets.length) {
    return map;
  }
  map[file] = snippets.map((snippet) => [snippet.startLine, snippet.endLine]);
  return map;
}

function sortDefinitions(definitions: BundleSymbol[]): BundleSymbol[] {
  return [...definitions].sort((a, b) => {
    if (b.hits !== a.hits) {
      return b.hits - a.hits;
    }
    return a.startLine - b.startLine;
  });
}

export async function getContextBundle(options: ContextBundleOptions): Promise<ContextBundleResult> {
  const absoluteRoot = path.resolve(options.root);
  const databasePath = path.join(absoluteRoot, options.databaseName ?? DEFAULT_DB_FILENAME);
  const tokenBudget = normalizeBudget(options.budgetTokens);

  const db = new Database(databasePath, { fileMustExist: true });
  try {
    const symbolRows = db
      .prepare(
        `SELECT name, start_line as startLine, end_line as endLine, hits
         FROM symbols
         WHERE file = ?`
      )
      .all(options.file) as SymbolRow[];

    if (!symbolRows.length) {
      throw new Error(`No symbols indexed for '${options.file}'. Run ingest_codebase first.`);
    }

    const definitions = sortDefinitions(symbolRows).slice(0, 24).map(toBundleSymbol);

    const focusName = options.symbol?.name?.toLowerCase();
    const focusSymbol = focusName
      ? definitions.find((definition) => definition.name.toLowerCase() === focusName) ?? null
      : definitions[0] ?? null;

    const snippetRows = db
      .prepare(
        `SELECT id, text, start_line as startLine, end_line as endLine, hits
         FROM snippets
         WHERE file = ?
         ORDER BY hits DESC, start_line ASC`
      )
      .all(options.file) as SnippetRow[];

    if (!snippetRows.length) {
      throw new Error(`No snippets indexed for '${options.file}'.`);
    }

    const selectedSnippets: BundleSnippet[] = [];
    let usedTokens = METADATA_TOKEN_RESERVE;
    const warnings: string[] = [];

    const seenSnippetIds = new Set<number>();

    const addSnippet = (row: SnippetRow) => {
      if (seenSnippetIds.has(row.id)) {
        return;
      }
      const snippetTokens = estimateTokens(row.text);
      if (usedTokens + snippetTokens > tokenBudget) {
        warnings.push(`Token budget reached before including snippet covering lines ${row.startLine}-${row.endLine}.`);
        return;
      }
      usedTokens += snippetTokens;
      selectedSnippets.push({
        snippetId: row.id,
        text: row.text,
        startLine: row.startLine,
        endLine: row.endLine
      });
      seenSnippetIds.add(row.id);
    };

    if (focusSymbol) {
      const coveringSnippet = snippetRows.find(
        (row) => row.startLine <= focusSymbol.startLine && row.endLine >= focusSymbol.startLine
      );
      if (coveringSnippet) {
        addSnippet(coveringSnippet);
      }
    }

    for (const row of snippetRows) {
      if (usedTokens >= tokenBudget) {
        break;
      }
      addSnippet(row);
    }

    if (!selectedSnippets.length) {
      addSnippet(snippetRows[0]);
    }

    selectedSnippets.sort((a, b) => a.startLine - b.startLine);

    const updateSnippetStmt = db.prepare('UPDATE snippets SET hits = hits + 1 WHERE id = ?');
    for (const snippet of selectedSnippets) {
      updateSnippetStmt.run(snippet.snippetId);
    }

    const updateSymbolStmt = db.prepare(
      'UPDATE symbols SET hits = hits + 1 WHERE name = ? AND file = ? AND start_line = ?'
    );
    const updatedSymbols = new Set<string>();
    for (const symbol of definitions) {
      if (updatedSymbols.has(symbol.name)) {
        continue;
      }
      updateSymbolStmt.run(symbol.name, options.file, symbol.startLine);
      updatedSymbols.add(symbol.name);
    }

    const estimatedTokens = usedTokens;

    return {
      databasePath,
      file: options.file,
      tokenBudget,
      estimatedTokens,
      focusSymbol,
      definitions,
      snippets: selectedSnippets,
      citations: buildCitations(options.file, selectedSnippets),
      warnings
    };
  } finally {
    db.close();
  }
}
