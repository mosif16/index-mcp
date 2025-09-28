import { type GraphNodeDescriptor } from './graph-query.js';
import type { ContextBundleSymbolSelector } from './context-bundle.js';

export type UnknownRecord = Record<string, unknown>;

type AliasMap = Record<string, string[]>;

function cloneRecord(input: UnknownRecord): UnknownRecord {
  return { ...input };
}

function applyAliases(record: UnknownRecord, aliases: AliasMap): void {
  for (const [canonical, names] of Object.entries(aliases)) {
    if (record[canonical] !== undefined) {
      continue;
    }
    for (const name of names) {
      if (record[name] !== undefined) {
        record[canonical] = record[name];
        delete record[name];
        break;
      }
    }
  }
}

function coerceBoolean(record: UnknownRecord, key: string): void {
  const value = record[key];
  if (typeof value === 'string') {
    const normalized = value.trim().toLowerCase();
    if (normalized === 'true') {
      record[key] = true;
    } else if (normalized === 'false') {
      record[key] = false;
    }
  }
}

function coerceNumber(record: UnknownRecord, key: string): void {
  const value = record[key];
  if (typeof value === 'string' && value.trim() !== '') {
    const parsed = Number(value);
    if (!Number.isNaN(parsed)) {
      record[key] = parsed;
    }
  }
}

function ensureArray(record: UnknownRecord, key: string): void {
  const value = record[key];
  if (value === undefined || value === null) {
    return;
  }
  if (Array.isArray(value)) {
    return;
  }
  record[key] = [value];
}

function normalizeNestedBoolean(record: UnknownRecord, key: string, targetKey: string): void {
  const value = record[key];
  if (typeof value === 'string') {
    const normalized = value.trim().toLowerCase();
    if (normalized === 'false') {
      record[key] = { [targetKey]: false };
    } else if (normalized === 'true') {
      record[key] = { [targetKey]: true };
    }
  }
}

function coerceNestedNumbers(record: UnknownRecord, key: string, fields: string[]): void {
  const value = record[key];
  if (!value || typeof value !== 'object') {
    return;
  }
  for (const field of fields) {
    const inner = (value as UnknownRecord)[field];
    if (typeof inner === 'string' && inner.trim() !== '') {
      const parsed = Number(inner);
      if (!Number.isNaN(parsed)) {
        (value as UnknownRecord)[field] = parsed;
      }
    }
  }
}

export function normalizeIngestArgs(raw: unknown): UnknownRecord {
  if (!raw || typeof raw !== 'object') {
    return {};
  }
  const record = cloneRecord(raw as UnknownRecord);

  applyAliases(record, {
    root: ['path', 'project_path', 'workspace_root', 'working_directory'],
    include: ['include_globs', 'globs'],
    exclude: ['exclude_globs'],
    databaseName: ['database', 'database_path', 'db'],
    maxFileSizeBytes: ['max_file_size', 'max_bytes'],
    storeFileContent: ['store_content', 'include_content'],
    contentSanitizer: ['sanitizer'],
    embedding: ['embedding_options'],
    graph: ['graph_options'],
    paths: ['target_paths', 'changed_paths']
  });

  ensureArray(record, 'include');
  ensureArray(record, 'exclude');
  ensureArray(record, 'paths');

  coerceNumber(record, 'maxFileSizeBytes');
  coerceBoolean(record, 'storeFileContent');

  normalizeNestedBoolean(record, 'embedding', 'enabled');
  normalizeNestedBoolean(record, 'graph', 'enabled');

  coerceNestedNumbers(record, 'embedding', ['chunkSizeTokens', 'chunkOverlapTokens', 'batchSize']);
  coerceNestedNumbers(record, 'graph', []);

  if (record.embedding && typeof record.embedding === 'object') {
    applyAliases(record.embedding as UnknownRecord, {
      model: ['embedding_model'],
      chunkSizeTokens: ['chunk_size', 'chunk_tokens'],
      chunkOverlapTokens: ['chunk_overlap', 'overlap_tokens'],
      batchSize: ['batch', 'batch_size']
    });
  }

  if (record.graph && typeof record.graph === 'object') {
    applyAliases(record.graph as UnknownRecord, {
      enabled: ['active']
    });
  }

  return record;
}

export function normalizeSearchArgs(raw: unknown): UnknownRecord {
  if (!raw || typeof raw !== 'object') {
    return {};
  }
  const record = cloneRecord(raw as UnknownRecord);

  applyAliases(record, {
    root: ['path', 'project_path', 'workspace_root', 'working_directory'],
    query: ['text', 'search', 'search_query'],
    databaseName: ['database', 'database_path', 'db'],
    limit: ['max_results', 'top_k'],
    model: ['embedding_model']
  });

  coerceNumber(record, 'limit');

  return record;
}

export function normalizeGraphArgs(raw: unknown): UnknownRecord {
  if (!raw || typeof raw !== 'object') {
    return {};
  }
  const record = cloneRecord(raw as UnknownRecord);

  applyAliases(record, {
    root: ['path', 'project_path', 'workspace_root', 'working_directory'],
    databaseName: ['database', 'database_path', 'db'],
    node: ['target', 'symbol', 'entity'],
    direction: ['edge_direction'],
    limit: ['max_neighbors', 'top_k']
  });

  if (typeof record.node === 'string') {
    const nodeName = record.node as string;
    record.node = { name: nodeName } as GraphNodeDescriptor;
  }

  if (record.node && typeof record.node === 'object') {
    const nodeRecord = record.node as UnknownRecord;
    applyAliases(nodeRecord, {
      name: ['identifier'],
      path: ['file', 'file_path'],
      kind: ['type']
    });
  }

  coerceNumber(record, 'limit');

  if (typeof record.direction === 'string') {
    record.direction = record.direction.toLowerCase();
  }

  return record;
}

export function normalizeStatusArgs(raw: unknown): UnknownRecord {
  if (!raw || typeof raw !== 'object') {
    return {};
  }

  const record = cloneRecord(raw as UnknownRecord);

  applyAliases(record, {
    root: ['path', 'project_path', 'workspace_root', 'working_directory'],
    databaseName: ['database', 'database_path', 'db'],
    historyLimit: ['history_limit', 'ingestion_limit', 'recent_runs']
  });

  coerceNumber(record, 'historyLimit');

  return record;
}

export function normalizeContextBundleArgs(raw: unknown): UnknownRecord {
  if (!raw || typeof raw !== 'object') {
    return {};
  }

  const record = cloneRecord(raw as UnknownRecord);

  applyAliases(record, {
    root: ['path', 'project_path', 'workspace_root', 'working_directory'],
    databaseName: ['database', 'database_path', 'db'],
    file: ['file_path', 'relative_path', 'target_path'],
    symbol: ['symbol_selector', 'target_symbol'],
    maxSnippets: ['snippet_limit', 'max_chunks'],
    maxNeighbors: ['neighbor_limit', 'edge_limit', 'max_edges']
  });

  coerceNumber(record, 'maxSnippets');
  coerceNumber(record, 'maxNeighbors');

  const symbolValue = record.symbol;
  if (typeof symbolValue === 'string') {
    record.symbol = { name: symbolValue } satisfies ContextBundleSymbolSelector;
  } else if (symbolValue && typeof symbolValue === 'object') {
    const symbolRecord = symbolValue as UnknownRecord;
    applyAliases(symbolRecord, {
      name: ['symbol_name'],
      kind: ['symbol_kind', 'type']
    });
  }

  return record;
}
