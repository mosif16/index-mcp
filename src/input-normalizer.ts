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

function normalizeRootField(record: UnknownRecord): void {
  const value = record.root;
  if (typeof value !== 'string') {
    return;
  }
  const trimmed = value.trim();
  if (trimmed) {
    record.root = trimmed;
  } else {
    delete record.root;
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

  normalizeRootField(record);

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

  normalizeRootField(record);
  coerceNumber(record, 'limit');

  return record;
}

export function normalizeLookupArgs(raw: unknown): UnknownRecord {
  if (!raw || typeof raw !== 'object') {
    return {};
  }

  const record = cloneRecord(raw as UnknownRecord);

  applyAliases(record, {
    root: ['path', 'project_path', 'workspace_root', 'working_directory'],
    mode: ['intent', 'action'],
    query: ['text', 'search', 'search_query'],
    file: ['file_path', 'target_path'],
    symbol: ['symbol_selector', 'target_symbol', 'focus_symbol'],
    node: ['graph_node', 'graph_target', 'target', 'entity'],
    databaseName: ['database', 'database_path', 'db'],
    limit: ['max_results', 'top_k', 'max_neighbors'],
    maxSnippets: ['snippet_limit', 'max_chunks'],
    maxNeighbors: ['neighbor_limit', 'edge_limit'],
    direction: ['edge_direction'],
    model: ['embedding_model']
  });

  normalizeRootField(record);

  if (typeof record.mode === 'string') {
    record.mode = record.mode.trim().toLowerCase();
  }

  if (typeof record.query === 'string') {
    const queryText = record.query.trim();
    if (queryText) {
      record.query = queryText;
    } else {
      delete record.query;
    }
  }

  if (typeof record.file === 'string') {
    const filePath = record.file.trim();
    if (filePath) {
      record.file = filePath;
    } else {
      delete record.file;
    }
  }

  if (record.symbol && typeof record.symbol === 'object') {
    applyAliases(record.symbol as UnknownRecord, {
      name: ['identifier'],
      kind: ['type'],
      path: ['file', 'file_path']
    });
  }

  if (typeof record.symbol === 'string') {
    const symbolName = record.symbol.trim();
    if (symbolName) {
      record.symbol = { name: symbolName } as ContextBundleSymbolSelector;
    } else {
      delete record.symbol;
    }
  }

  if (record.node && typeof record.node === 'object') {
    applyAliases(record.node as UnknownRecord, {
      name: ['identifier'],
      path: ['file', 'file_path'],
      kind: ['type']
    });
  }

  if (typeof record.node === 'string') {
    const nodeName = record.node.trim();
    if (nodeName) {
      record.node = { name: nodeName };
    } else {
      delete record.node;
    }
  }

  coerceNumber(record, 'limit');
  coerceNumber(record, 'maxSnippets');
  coerceNumber(record, 'maxNeighbors');

  if (typeof record.direction === 'string') {
    record.direction = record.direction.trim().toLowerCase();
  }

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

  normalizeRootField(record);

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

  normalizeRootField(record);
  coerceNumber(record, 'historyLimit');

  return record;
}

export function normalizeTimelineArgs(raw: unknown): UnknownRecord {
  if (!raw || typeof raw !== 'object') {
    return {};
  }

  const record = cloneRecord(raw as UnknownRecord);

  applyAliases(record, {
    root: ['path', 'project_path', 'workspace_root', 'working_directory'],
    branch: ['ref', 'revision', 'target', 'branch_name'],
    limit: ['max_commits', 'max_results', 'top_k'],
    since: ['after', 'since_date', 'since_time'],
    includeMerges: ['merges', 'include_merges', 'with_merges'],
    includeFileStats: ['file_stats', 'include_stats', 'with_stats'],
    includeDiffs: ['diffs', 'with_diffs', 'include_patches'],
    paths: ['path_filters', 'files', 'file_paths'],
    diffPattern: ['pattern', 'diff_regex', 'search', 'content_match']
  });

  normalizeRootField(record);
  coerceNumber(record, 'limit');
  coerceBoolean(record, 'includeMerges');
  coerceBoolean(record, 'includeFileStats');
  coerceBoolean(record, 'includeDiffs');

  if (record.includeDiffs === undefined) {
    record.includeDiffs = true;
  }

  ensureArray(record, 'paths');

  if (typeof record.branch === 'string') {
    const trimmedBranch = record.branch.trim();
    if (trimmedBranch) {
      record.branch = trimmedBranch;
    } else {
      delete record.branch;
    }
  }

  if (typeof record.since === 'string') {
    const trimmedSince = record.since.trim();
    if (trimmedSince) {
      record.since = trimmedSince;
    } else {
      delete record.since;
    }
  }

  if (typeof record.diffPattern === 'string') {
    const trimmedPattern = record.diffPattern.trim();
    if (trimmedPattern) {
      record.diffPattern = trimmedPattern;
    } else {
      delete record.diffPattern;
    }
  }

  if (Array.isArray(record.paths)) {
    const normalizedPaths = record.paths
      .map((value) => (typeof value === 'string' ? value.trim() : undefined))
      .filter((value): value is string => typeof value === 'string' && value.length > 0);

    if (normalizedPaths.length > 0) {
      record.paths = normalizedPaths;
    } else {
      delete record.paths;
    }
  }

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

  normalizeRootField(record);
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
