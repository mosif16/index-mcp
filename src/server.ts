import { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js';
import { StdioServerTransport } from '@modelcontextprotocol/sdk/server/stdio.js';
import type { GetPromptResult } from '@modelcontextprotocol/sdk/types.js';
import { parseArgs } from 'node:util';
import { z } from 'zod';

import { ingestCodebase } from './ingest.js';
import { semanticSearch } from './search.js';
import { graphNeighbors } from './graph-query.js';
import { startIngestWatcher } from './watcher.js';
import { resolveRootPath, type RootResolutionContext } from './root-resolver.js';
import { getPackageMetadata } from './package-metadata.js';
import { logger } from './logger.js';
import {
  normalizeContextBundleArgs,
  normalizeGraphArgs,
  normalizeIngestArgs,
  normalizeSearchArgs,
  normalizeStatusArgs
} from './input-normalizer.js';
import { getNativeModuleStatus, loadNativeModule } from './native/index.js';
import { getIndexStatus } from './status.js';
import { getContextBundle } from './context-bundle.js';

function rethrowWithContext(toolName: string, error: unknown): never {
  if (error instanceof Error) {
    throw Object.assign(new Error(`${toolName} failed: ${error.message}`), { cause: error });
  }
  throw new Error(`${toolName} failed: ${String(error)}`);
}

function normalizeHeaders(headers: unknown): Record<string, string> | undefined {
  if (!headers) {
    return undefined;
  }

  if (typeof (headers as { forEach?: unknown }).forEach === 'function') {
    const result: Record<string, string> = {};
    (headers as { forEach: (callback: (value: string, key: string) => void) => void }).forEach((value, key) => {
      result[key.toLowerCase()] = value;
    });
    return result;
  }

  if (typeof headers === 'object') {
    const result: Record<string, string> = {};
    for (const [key, value] of Object.entries(headers as Record<string, unknown>)) {
      if (typeof value === 'string' && value) {
        result[key.toLowerCase()] = value;
      } else if (Array.isArray(value) && value.length > 0 && typeof value[0] === 'string') {
        result[key.toLowerCase()] = value[0];
      }
    }
    return Object.keys(result).length ? result : undefined;
  }

  return undefined;
}

function createRootResolutionContext(extra: unknown): RootResolutionContext {
  if (!extra || typeof extra !== 'object') {
    return {};
  }

  const meta = (extra as { _meta?: unknown })._meta;
  const requestInfo = (extra as { requestInfo?: { headers?: unknown; env?: Record<string, string> } }).requestInfo;

  const context: RootResolutionContext = {};

  if (meta && typeof meta === 'object') {
    context.meta = meta as Record<string, unknown>;
  }

  const headers = requestInfo?.headers ? normalizeHeaders(requestInfo.headers) : undefined;
  if (headers) {
    context.headers = headers;
  }

  if (requestInfo?.env && typeof requestInfo.env === 'object') {
    context.env = requestInfo.env;
  }

  return context;
}

const { name: serverName, version: serverVersion, description: serverDescription } = getPackageMetadata();

const ingestToolJsonSchema = {
  type: 'object',
  title: 'Ingest Codebase Parameters',
  description:
    'Walk a repository and persist metadata/content into a SQLite index. Accepts relative paths and supports alias parameters like path/project_path.',
  properties: {
    root: {
      type: 'string',
      description: 'Absolute or relative path to the workspace to index.'
    },
    include: {
      type: 'array',
      description: 'Glob patterns to include (aliases: include_globs, globs).',
      items: { type: 'string' },
      default: ['**/*']
    },
    exclude: {
      type: 'array',
      description: 'Glob patterns to exclude (aliases: exclude_globs). Defaults include .git, dist, node_modules.',
      items: { type: 'string' }
    },
    databaseName: {
      type: 'string',
      description: 'Optional SQLite filename (aliases: database, database_path, db). Defaults to .mcp-index.sqlite.'
    },
    maxFileSizeBytes: {
      type: 'integer',
      description: 'Maximum file size to ingest in bytes (aliases: max_file_size, max_bytes). Defaults to 8 MiB.'
    },
    storeFileContent: {
      type: 'boolean',
      description: 'Whether to store file content in the index (aliases: store_content, include_content). Defaults to true.'
    },
    contentSanitizer: {
      type: 'object',
      description: 'Optional sanitizer module specification, supports module/exportName/options.',
      properties: {
        module: { type: 'string' },
        exportName: { type: 'string' },
        options: {}
      }
    },
    embedding: {
      type: 'object',
      description: 'Embedding configuration (aliases: embedding_options). Accepts “false” to disable.',
      properties: {
        enabled: { type: 'boolean', description: 'Toggle embedding generation.' },
        model: { type: 'string', description: 'Embedding model identifier (alias: embedding_model).' },
        chunkSizeTokens: { type: 'integer', description: 'Token count per chunk (aliases: chunk_size, chunk_tokens).' },
        chunkOverlapTokens: { type: 'integer', description: 'Token overlap (aliases: chunk_overlap, overlap_tokens).' },
        batchSize: { type: 'integer', description: 'Embedding batch size (aliases: batch, batch_size).' }
      }
    },
    graph: {
      type: 'object',
      description: 'Graph extraction configuration (aliases: graph_options). Accepts “false” to disable.',
      properties: {
        enabled: { type: 'boolean', description: 'Toggle structural graph extraction.' }
      }
    },
    paths: {
      type: 'array',
      description: 'Restrict ingest to specific relative paths (aliases: target_paths, changed_paths).',
      items: { type: 'string' }
    }
  },
  required: ['root'],
  additionalProperties: false
} as const;

const ingestToolSchema = z
  .object({
    root: z
      .string({ required_error: 'root directory is required' })
      .min(1, 'root directory is required')
      .describe('Absolute or relative path to the workspace to index.'),
    include: z
      .array(z.string({ invalid_type_error: 'include patterns must be strings' }))
      .describe('Glob patterns to include (aliases: include_globs, globs).')
      .optional(),
    exclude: z
      .array(z.string({ invalid_type_error: 'exclude patterns must be strings' }))
      .describe('Glob patterns to exclude (aliases: exclude_globs).')
      .optional(),
    databaseName: z
      .string()
      .min(1)
      .describe('Optional SQLite filename (aliases: database, database_path, db).')
      .optional(),
    maxFileSizeBytes: z
      .number()
      .int()
      .positive()
      .describe('Maximum file size to ingest in bytes (defaults to 8 MiB).')
      .optional(),
    storeFileContent: z
      .boolean()
      .describe('Whether to store file content in the index (aliases: store_content, include_content).')
      .optional(),
    contentSanitizer: z
      .object({
        module: z
          .string({ required_error: 'module specifier is required' })
          .min(1, 'module specifier is required'),
        exportName: z.string().min(1).optional(),
        options: z.unknown().optional()
      })
      .describe('Optional sanitizer module specification.')
      .optional(),
    embedding: z
      .object({
        enabled: z.boolean().optional().describe('Toggle embedding generation.'),
        model: z.string().min(1).optional().describe('Embedding model identifier (alias: embedding_model).'),
        chunkSizeTokens: z
          .number()
          .int()
          .positive()
          .optional()
          .describe('Token count per chunk (aliases: chunk_size, chunk_tokens).'),
        chunkOverlapTokens: z
          .number()
          .int()
          .min(0)
          .optional()
          .describe('Token overlap (aliases: chunk_overlap, overlap_tokens).'),
        batchSize: z
          .number()
          .int()
          .positive()
          .optional()
          .describe('Embedding batch size (aliases: batch, batch_size).')
      })
      .describe('Embedding configuration (aliases: embedding_options).')
      .optional(),
    graph: z
      .object({
        enabled: z.boolean().optional().describe('Toggle structural graph extraction.')
      })
      .describe('Graph extraction configuration (aliases: graph_options).')
      .optional(),
    paths: z
      .array(z.string())
      .describe('Restrict ingest to specific relative paths (aliases: target_paths, changed_paths).')
      .optional()
  })
  .strict();

const skippedFileSchema = z.object({
  path: z.string(),
  reason: z.enum(['file-too-large', 'read-error']),
  size: z.number().optional(),
  message: z.string().optional()
});
const ingestToolOutputShape = {
  root: z.string(),
  databasePath: z.string(),
  databaseSizeBytes: z.number(),
  ingestedFileCount: z.number(),
  skipped: z.array(skippedFileSchema),
  deletedPaths: z.array(z.string()),
  durationMs: z.number(),
  embeddedChunkCount: z.number().int().min(0),
  embeddingModel: z.string().nullable(),
  graphNodeCount: z.number().int().min(0),
  graphEdgeCount: z.number().int().min(0)
} as const;
const ingestToolOutputSchema = z.object(ingestToolOutputShape);

const semanticSearchJsonSchema = {
  type: 'object',
  title: 'Semantic Search Parameters',
  description: 'Search indexed chunks with embeddings. Accepts alias parameters like text/search_query.',
  properties: {
    root: {
      type: 'string',
      description: 'Absolute or relative path to the indexed workspace (aliases: path, workspace_root).'
    },
    query: {
      type: 'string',
      description: 'Full-text query used to rank chunks (aliases: text, search, search_query).'
    },
    databaseName: {
      type: 'string',
      description: 'Optional SQLite filename (aliases: database, database_path, db).'
    },
    limit: {
      type: 'integer',
      description: 'Maximum number of matches to return (aliases: max_results, top_k). Defaults to 8 and caps at 50.'
    },
    model: {
      type: 'string',
      description: 'Embedding model filter (alias: embedding_model). Required when multiple models are present.'
    }
  },
  required: ['root', 'query'],
  additionalProperties: false
} as const;

const semanticSearchSchema = z
  .object({
    root: z
      .string({ required_error: 'root directory is required' })
      .min(1, 'root directory is required')
      .describe('Absolute or relative path to the indexed workspace.'),
    query: z
      .string({ required_error: 'query text is required' })
      .min(1, 'query text is required')
      .describe('Full-text query used to rank chunks.'),
    databaseName: z.string().min(1).optional().describe('Optional SQLite filename.'),
    limit: z
      .number()
      .int()
      .positive()
      .max(50)
      .optional()
      .describe('Maximum number of matches to return (defaults to 8, caps at 50).'),
    model: z.string().min(1).optional().describe('Embedding model filter.')
  })
  .strict();

const semanticSearchMatchSchema = z.object({
  path: z.string(),
  chunkIndex: z.number().int(),
  score: z.number(),
  content: z.string(),
  embeddingModel: z.string(),
  byteStart: z.number().int().nullable(),
  byteEnd: z.number().int().nullable(),
  lineStart: z.number().int().nullable(),
  lineEnd: z.number().int().nullable(),
  contextBefore: z.string().nullable(),
  contextAfter: z.string().nullable()
});
const semanticSearchOutputShape = {
  databasePath: z.string(),
  embeddingModel: z.string().nullable(),
  totalChunks: z.number().int().min(0),
  evaluatedChunks: z.number().int().min(0),
  results: z.array(semanticSearchMatchSchema)
} as const;
const semanticSearchOutputSchema = z.object(semanticSearchOutputShape);

const graphNeighborJsonSchema = {
  type: 'object',
  title: 'Graph Neighbor Parameters',
  description:
    'Inspect structural relationships captured during ingestion. Accepts alias parameters like target/entity/name.',
  properties: {
    root: {
      type: 'string',
      description: 'Absolute or relative path to the indexed workspace (aliases: path, workspace_root).'
    },
    databaseName: {
      type: 'string',
      description: 'Optional SQLite filename (aliases: database, database_path, db).'
    },
    node: {
      type: 'object',
      description: 'Descriptor for the graph node (aliases: target, symbol, entity).',
      properties: {
        id: { type: 'string', description: 'Exact node id.' },
        path: {
          type: ['string', 'null'],
          description: 'File path for the node (aliases: file, file_path).'
        },
        kind: { type: 'string', description: 'Node type (alias: type).' },
        name: { type: 'string', description: 'Node name (alias: identifier).' }
      },
      required: ['name'],
      additionalProperties: false
    },
    direction: {
      type: 'string',
      enum: ['incoming', 'outgoing', 'both'],
      description: 'Neighbor direction (alias: edge_direction). Defaults to outgoing.'
    },
    limit: {
      type: 'integer',
      description: 'Maximum neighbors to return (aliases: max_neighbors, top_k). Defaults to 16, caps at 100.'
    }
  },
  required: ['root', 'node'],
  additionalProperties: false
} as const;

const graphNeighborSchema = z
  .object({
    root: z
      .string({ required_error: 'root directory is required' })
      .min(1, 'root directory is required')
      .describe('Absolute or relative path to the indexed workspace.'),
    databaseName: z.string().min(1).optional().describe('Optional SQLite filename.'),
    node: z
      .object({
        id: z.string().optional(),
        path: z.string().nullable().optional(),
        kind: z.string().optional(),
        name: z.string({ required_error: 'node name is required' }).min(1, 'node name is required')
      })
      .strict()
      .describe('Descriptor for the graph node.'),
    direction: z.enum(['incoming', 'outgoing', 'both']).optional(),
    limit: z
      .number()
      .int()
      .positive()
      .max(100)
      .optional()
      .describe('Maximum neighbors to return (defaults to 16, caps at 100).')
  })
  .strict();

const graphNeighborNodeSchema = z.object({
  id: z.string(),
  path: z.string().nullable(),
  kind: z.string(),
  name: z.string(),
  signature: z.string().nullable(),
  metadata: z.record(z.any()).nullable()
});

const graphNeighborEdgeSchema = z.object({
  id: z.string(),
  type: z.string(),
  direction: z.enum(['incoming', 'outgoing']),
  metadata: z.record(z.any()).nullable(),
  neighbor: graphNeighborNodeSchema
});

const graphNeighborOutputShape = {
  databasePath: z.string(),
  node: graphNeighborNodeSchema,
  neighbors: z.array(graphNeighborEdgeSchema)
} as const;
const graphNeighborOutputSchema = z.object(graphNeighborOutputShape);

const contextBundleJsonSchema = {
  type: 'object',
  title: 'Context Bundle Parameters',
  description: 'Return a compact summary of file metadata, definitions, snippets, and related symbols.',
  properties: {
    root: {
      type: 'string',
      description: 'Absolute or relative path to the indexed workspace (aliases: path, workspace_root).'
    },
    databaseName: {
      type: 'string',
      description: 'Optional SQLite filename (aliases: database, database_path, db). Defaults to .mcp-index.sqlite.'
    },
    file: {
      type: 'string',
      description: 'Relative file path to summarize (aliases: file_path, target_path).'
    },
    symbol: {
      type: ['object', 'string'],
      description: 'Optional symbol selector (aliases: target_symbol, symbol_selector). When a string, treated as the symbol name.',
      properties: {
        name: {
          type: 'string',
          description: 'Symbol name to focus on.'
        },
        kind: {
          type: 'string',
          description: 'Optional graph node kind to disambiguate (e.g., function, class).'
        }
      },
      required: ['name'],
      additionalProperties: false
    },
    maxSnippets: {
      type: 'integer',
      description: 'Maximum snippets to include (aliases: snippet_limit, max_chunks). Defaults to 3, max 10.',
      minimum: 0,
      maximum: 10
    },
    maxNeighbors: {
      type: 'integer',
      description: 'Maximum related edges to include per direction (aliases: neighbor_limit, edge_limit). Defaults to 12, max 50.',
      minimum: 0,
      maximum: 50
    }
  },
  required: ['root', 'file'],
  additionalProperties: false
} as const;

const contextBundleSymbolSchema = z
  .object({
    name: z.string({ required_error: 'symbol name is required' }).min(1, 'symbol name is required'),
    kind: z.string().min(1).optional()
  })
  .strict();

const contextBundleInputSchema = z
  .object({
    root: z
      .string({ required_error: 'root directory is required' })
      .min(1, 'root directory is required')
      .describe('Absolute or relative path to the indexed workspace.'),
    databaseName: z.string().min(1).optional().describe('Optional SQLite filename.'),
    file: z
      .string({ required_error: 'file path is required' })
      .min(1, 'file path is required')
      .describe('Relative file path to summarize.'),
    symbol: contextBundleSymbolSchema.optional().describe('Optional symbol selector to focus the bundle.'),
    maxSnippets: z
      .number()
      .int()
      .min(0)
      .max(10)
      .optional()
      .describe('Maximum snippets to include (defaults to 3, caps at 10).'),
    maxNeighbors: z
      .number()
      .int()
      .min(0)
      .max(50)
      .optional()
      .describe('Maximum related edges per direction (defaults to 12, caps at 50).')
  })
  .strict();

const contextBundleDefinitionSchema = z.object({
  id: z.string(),
  name: z.string(),
  kind: z.string(),
  signature: z.string().nullable(),
  rangeStart: z.number().int().nullable(),
  rangeEnd: z.number().int().nullable(),
  metadata: z.record(z.any()).nullable()
});

const contextBundleFileSchema = z.object({
  path: z.string(),
  size: z.number(),
  modified: z.number(),
  hash: z.string(),
  lastIndexedAt: z.number()
});

const contextBundleNeighborNodeSchema = z.object({
  id: z.string(),
  path: z.string().nullable(),
  kind: z.string(),
  name: z.string(),
  signature: z.string().nullable(),
  metadata: z.record(z.any()).nullable()
});

const contextBundleNeighborSchema = z.object({
  id: z.string(),
  type: z.string(),
  direction: z.enum(['incoming', 'outgoing']),
  metadata: z.record(z.any()).nullable(),
  fromNodeId: z.string(),
  toNodeId: z.string(),
  neighbor: contextBundleNeighborNodeSchema
});

const contextBundleSnippetSchema = z.object({
  source: z.enum(['chunk', 'content']),
  chunkIndex: z.number().int().nullable(),
  content: z.string(),
  byteStart: z.number().int().nullable(),
  byteEnd: z.number().int().nullable(),
  lineStart: z.number().int().nullable(),
  lineEnd: z.number().int().nullable()
});

const contextBundleIngestionSchema = z.object({
  id: z.string(),
  finishedAt: z.number(),
  durationMs: z.number(),
  fileCount: z.number()
});

const contextBundleOutputShape = {
  databasePath: z.string(),
  file: contextBundleFileSchema,
  definitions: z.array(contextBundleDefinitionSchema),
  focusDefinition: contextBundleDefinitionSchema.nullable(),
  related: z.array(contextBundleNeighborSchema),
  snippets: z.array(contextBundleSnippetSchema),
  latestIngestion: contextBundleIngestionSchema.nullable(),
  warnings: z.array(z.string())
} as const;

const contextBundleOutputSchema = z.object(contextBundleOutputShape);

const indexStatusJsonSchema = {
  type: 'object',
  title: 'Index Status Parameters',
  description: 'Summarize the SQLite index, including ingestion history and coverage metrics.',
  properties: {
    root: {
      type: 'string',
      description: 'Absolute or relative path to the workspace whose index should be inspected.'
    },
    databaseName: {
      type: 'string',
      description: 'Optional SQLite filename (aliases: database, database_path, db). Defaults to .mcp-index.sqlite.'
    },
    historyLimit: {
      type: 'integer',
      description: 'Number of recent ingestions to include (aliases: history_limit, ingestion_limit, recent_runs). Defaults to 5.',
      minimum: 0,
      maximum: 25
    }
  },
  required: ['root'],
  additionalProperties: false
} as const;

const indexStatusSchema = z
  .object({
    root: z
      .string({ required_error: 'root directory is required' })
      .min(1, 'root directory is required')
      .describe('Absolute or relative path to the workspace whose index should be inspected.'),
    databaseName: z
      .string()
      .min(1)
      .describe('Optional SQLite filename (aliases: database, database_path, db).')
      .optional(),
    historyLimit: z
      .number()
      .int()
      .min(0)
      .max(25)
      .describe('Number of recent ingestions to include (defaults to 5, capped at 25).')
      .optional()
  })
  .strict();

const indexStatusIngestionSchema = z.object({
  id: z.string(),
  root: z.string(),
  startedAt: z.number(),
  finishedAt: z.number(),
  durationMs: z.number(),
  fileCount: z.number(),
  skippedCount: z.number(),
  deletedCount: z.number()
});

const indexStatusOutputShape = {
  databasePath: z.string(),
  databaseExists: z.boolean(),
  databaseSizeBytes: z.number().nullable(),
  totalFiles: z.number(),
  totalChunks: z.number(),
  embeddingModels: z.array(z.string()),
  totalGraphNodes: z.number(),
  totalGraphEdges: z.number(),
  latestIngestion: indexStatusIngestionSchema.nullable(),
  recentIngestions: z.array(indexStatusIngestionSchema)
} as const;

const indexStatusOutputSchema = z.object(indexStatusOutputShape);

const infoToolJsonSchema = {
  type: 'object',
  title: 'Info Parameters',
  description: 'No parameters required.',
  properties: {},
  additionalProperties: false
} as const;

const indexingGuidanceToolJsonSchema = {
  type: 'object',
  title: 'Indexing Guidance Parameters',
  description: 'No parameters required.',
  properties: {},
  additionalProperties: false
} as const;

const nativeStatusSchema = z.object({
  status: z.enum(['ready', 'unavailable', 'error']),
  message: z.string().optional()
});

const infoToolOutputShape = {
  name: z.string(),
  version: z.string(),
  description: z.string().nullable(),
  instructions: z.string(),
  nativeModule: nativeStatusSchema,
  environment: z.object({
    nodeVersion: z.string(),
    platform: z.string(),
    cwd: z.string(),
    pid: z.number()
  })
} as const;

const infoToolOutputSchema = z.object(infoToolOutputShape);

const indexingGuidanceOutputShape = {
  guidance: z.string()
} as const;

const indexingGuidanceOutputSchema = z.object(indexingGuidanceOutputShape);

const SERVER_INSTRUCTIONS = [
  `Tools available from ${serverName} v${serverVersion}: ingest_codebase (index the current codebase into SQLite), semantic_search (embedding-powered retrieval with byte/line metadata and nearby context), graph_neighbors (explore GraphRAG relationships), context_bundle (assemble file-level definitions, snippets, and related symbols), index_status (summarize index coverage and recent ingestions), info (report server diagnostics), indexing_guidance_tool (return the indexing reminders as a tool), and indexing_guidance (prompt describing when to reindex).`,
  'Use this MCP server for all repository-aware searches: run ingest_codebase to refresh context, rely on semantic_search for locating code or docs, inspect graph_neighbors for structural call/import details, request context_bundle when the agent needs a compact summary of a file or symbol, call index_status to confirm coverage, and use indexing_guidance_tool when the client cannot invoke prompts directly.',
  'Always run ingest_codebase on a new or freshly checked out codebase before asking for help.',
  'When running ingest_codebase, always exclude files and folders matched by .gitignore patterns so ignored content never enters the index.',
  'Any time you or the agent edits files—or after upgrading this server—re-run ingest_codebase so the SQLite index stays current and backfills the latest metadata.'
].join(' ');

const INDEXING_GUIDANCE_PROMPT: GetPromptResult = {
  messages: [
    {
      role: 'assistant',
      content: {
        type: 'text',
        text: 'Tools: ingest_codebase (index the repository), semantic_search (find relevant snippets), graph_neighbors (inspect code graph relationships), index_status (check ingestion freshness and coverage), and indexing_guidance (show these reminders). Always run ingest_codebase on a new codebase before requesting analysis, ensure .gitignore-matched files stay excluded whenever you index, and run ingest_codebase again after you or I modify files so the SQLite index reflects the latest code.'
      }
    }
  ]
};

const cli = parseArgs({
  options: {
    watch: { type: 'boolean' },
    'watch-root': { type: 'string' },
    'watch-debounce': { type: 'string' },
    'watch-no-initial': { type: 'boolean' },
    'watch-quiet': { type: 'boolean' },
    'watch-database': { type: 'string' }
  },
  allowPositionals: true
});

async function main() {
  if (cli.values.watch) {
    const debounceValue = cli.values['watch-debounce'];
    const debounceMs = typeof debounceValue === 'string' ? Number(debounceValue) : undefined;
    const watchRoot = (cli.values['watch-root'] as string | undefined) ?? process.cwd();
    const watchDatabase = cli.values['watch-database'] as string | undefined;
    const runInitial = cli.values['watch-no-initial'] ? false : true;
    const quiet = cli.values['watch-quiet'] ?? false;

    try {
      await startIngestWatcher({
        root: watchRoot,
        databaseName: watchDatabase,
        debounceMs,
        runInitial,
        quiet: quiet === true,
        graph: { enabled: true }
      });
    } catch (error) {
      logger.error({ err: error }, '[server] Failed to start watch daemon');
    }
  }

  const server = new McpServer({ name: serverName, version: serverVersion }, { instructions: SERVER_INSTRUCTIONS });

  logger.info({ name: serverName, version: serverVersion }, 'Starting MCP server');

  server.registerTool(
    'ingest_codebase',
    {
      description:
        'Walk a codebase and store file metadata and (optionally) content in a SQLite database at the repository root.',
      inputSchema: ingestToolSchema.shape,
      outputSchema: ingestToolOutputShape,
      annotations: {
        jsonSchema: ingestToolJsonSchema
      }
    },
    async (args, extra) => {
      try {
        const normalizedInput = normalizeIngestArgs(args);
        const parsedInput = ingestToolSchema.parse(normalizedInput);
        const context = createRootResolutionContext(extra);
        const resolvedRoot = resolveRootPath(parsedInput.root, context);
        const ingestInput = { ...parsedInput, root: resolvedRoot };
        const result = ingestToolOutputSchema.parse(await ingestCodebase(ingestInput));

        return {
          content: [
            {
              type: 'text',
              text: `Indexed ${result.ingestedFileCount} files in ${(result.durationMs / 1000).toFixed(
                2
              )}s. Database: ${result.databasePath}. Re-run ingest_codebase after any edits to keep the index fresh.`
            }
          ],
          structuredContent: result
        };
      } catch (error) {
        return rethrowWithContext('ingest_codebase', error);
      }
    }
  );

  server.registerTool(
    'semantic_search',
    {
      description: 'Return the most relevant indexed snippets using semantic embeddings.',
      inputSchema: semanticSearchSchema.shape,
      outputSchema: semanticSearchOutputShape,
      annotations: {
        jsonSchema: semanticSearchJsonSchema
      }
    },
    async (args, extra) => {
      try {
        const normalizedInput = normalizeSearchArgs(args);
        const parsedInput = semanticSearchSchema.parse(normalizedInput);
        const context = createRootResolutionContext(extra);
        const resolvedRoot = resolveRootPath(parsedInput.root, context);
        const searchInput = { ...parsedInput, root: resolvedRoot };
        const result = semanticSearchOutputSchema.parse(await semanticSearch(searchInput));
        const modelDescriptor = result.embeddingModel ? `model ${result.embeddingModel}` : 'stored embeddings';
        const summary = result.results.length
          ? `Semantic search scanned ${result.evaluatedChunks} chunks and returned ${result.results.length} matches (${modelDescriptor}).`
          : 'Semantic search evaluated the index but did not return any matches.';
        return {
          content: [
            {
              type: 'text',
              text: summary
            }
          ],
          structuredContent: result
        };
      } catch (error) {
        return rethrowWithContext('semantic_search', error);
      }
    }
  );

  server.registerTool(
    'graph_neighbors',
    {
      description: 'Explore structural relationships captured during ingestion to support GraphRAG workflows.',
      inputSchema: graphNeighborSchema.shape,
      outputSchema: graphNeighborOutputShape,
      annotations: {
        jsonSchema: graphNeighborJsonSchema
      }
    },
    async (args, extra) => {
      try {
        const normalizedInput = normalizeGraphArgs(args);
        const parsedInput = graphNeighborSchema.parse(normalizedInput);
        const context = createRootResolutionContext(extra);
        const resolvedRoot = resolveRootPath(parsedInput.root, context);
        const graphInput = { ...parsedInput, root: resolvedRoot };
        const result = graphNeighborOutputSchema.parse(await graphNeighbors(graphInput));
        const neighborCount = result.neighbors.length;
        const directionDescriptor = parsedInput.direction ?? 'outgoing';
        const summary = neighborCount
          ? `Graph query found ${neighborCount} ${neighborCount === 1 ? 'neighbor' : 'neighbors'} (${directionDescriptor}) for node '${result.node.name}'.`
          : `Graph query found no ${directionDescriptor} neighbors for node '${result.node.name}'.`;
        return {
          content: [
            {
              type: 'text',
              text: summary
            }
          ],
          structuredContent: result
        };
      } catch (error) {
        return rethrowWithContext('graph_neighbors', error);
      }
    }
  );

  server.registerTool(
    'context_bundle',
    {
      description: 'Bundle file metadata, definitions, snippets, and related symbols into an agent-friendly payload.',
      inputSchema: contextBundleInputSchema.shape,
      outputSchema: contextBundleOutputShape,
      annotations: {
        jsonSchema: contextBundleJsonSchema
      }
    },
    async (args, extra) => {
      try {
        const normalizedInput = normalizeContextBundleArgs(args);
        const parsedInput = contextBundleInputSchema.parse(normalizedInput);
        const context = createRootResolutionContext(extra);
        const resolvedRoot = resolveRootPath(parsedInput.root, context);
        const bundleInput = { ...parsedInput, root: resolvedRoot };
        const result = contextBundleOutputSchema.parse(await getContextBundle(bundleInput));

        const summaryPieces: string[] = [
          `Context bundle for ${result.file.path} includes ${result.definitions.length} definition(s)`
        ];
        if (result.focusDefinition) {
          summaryPieces.push(`focused on ${result.focusDefinition.kind} '${result.focusDefinition.name}'`);
        }
        if (result.related.length) {
          summaryPieces.push(`${result.related.length} related edge(s)`);
        }
        if (result.snippets.length) {
          summaryPieces.push(`${result.snippets.length} snippet(s)`);
        }
        if (result.warnings.length) {
          summaryPieces.push('warnings present');
        }

        const summary = `${summaryPieces.join(', ')}.`;

        return {
          content: [
            {
              type: 'text',
              text: summary
            }
          ],
          structuredContent: result
        };
      } catch (error) {
        return rethrowWithContext('context_bundle', error);
      }
    }
  );

  server.registerTool(
    'index_status',
    {
      description: 'Summarize the SQLite index to reveal ingestion freshness, coverage, and graph/embedding availability.',
      inputSchema: indexStatusSchema.shape,
      outputSchema: indexStatusOutputShape,
      annotations: {
        jsonSchema: indexStatusJsonSchema
      }
    },
    async (args, extra) => {
      try {
        const normalizedInput = normalizeStatusArgs(args);
        const parsedInput = indexStatusSchema.parse(normalizedInput);
        const context = createRootResolutionContext(extra);
        const resolvedRoot = resolveRootPath(parsedInput.root, context);
        const statusInput = { ...parsedInput, root: resolvedRoot };
        const result = indexStatusOutputSchema.parse(await getIndexStatus(statusInput));

        let summary: string;
        if (!result.databaseExists) {
          summary = `No SQLite index found at ${result.databasePath}. Run ingest_codebase to create a fresh index.`;
        } else if (!result.latestIngestion) {
          summary = `Index file ${result.databasePath} exists but no ingestion history is recorded yet. Run ingest_codebase to populate it.`;
        } else {
          const finishedIso = new Date(result.latestIngestion.finishedAt).toISOString();
          summary = `Index at ${result.databasePath} covers ${result.totalFiles} files and ${result.totalChunks} chunks. Last ingest finished ${finishedIso} after processing ${result.latestIngestion.fileCount} files.`;
        }

        return {
          content: [
            {
              type: 'text',
              text: summary
            }
          ],
          structuredContent: result
        };
      } catch (error) {
        return rethrowWithContext('index_status', error);
      }
    }
  );

  server.registerTool(
    'info',
    {
      description: 'Report server metadata, version, environment, and native dependency status.',
      inputSchema: {},
      outputSchema: infoToolOutputShape,
      annotations: {
        jsonSchema: infoToolJsonSchema
      }
    },
    async () => {
      let nativeModuleStatus: z.infer<typeof nativeStatusSchema> = { status: 'unavailable' };
      try {
        await loadNativeModule();
        const nativeStatus = getNativeModuleStatus();
        if (nativeStatus.state === 'native') {
          nativeModuleStatus = { status: 'ready' };
        } else if (nativeStatus.state === 'fallback') {
          const message = nativeStatus.message
            ? `${nativeStatus.message} (using JS fallback scanner)`
            : 'Using JS fallback scanner because native bindings were unavailable.';
          nativeModuleStatus = { status: 'error', message };
        } else {
          nativeModuleStatus = { status: 'unavailable' };
        }
      } catch (error) {
        const message = error instanceof Error ? error.message : String(error);
        nativeModuleStatus = { status: 'error', message };
      }

      const payload = infoToolOutputSchema.parse({
        name: serverName,
        version: serverVersion,
        description: serverDescription ?? null,
        instructions: SERVER_INSTRUCTIONS,
        nativeModule: nativeModuleStatus,
        environment: {
          nodeVersion: process.version,
          platform: `${process.platform}-${process.arch}`,
          cwd: process.cwd(),
          pid: process.pid
        }
      });

      return {
        content: [
          {
            type: 'text',
            text:
              nativeModuleStatus.status === 'ready'
                ? `${serverName} v${serverVersion} is ready.`
                : `${serverName} v${serverVersion} reported ${nativeModuleStatus.status} native bindings.`
          }
        ],
        structuredContent: payload
      };
    }
  );


  server.registerTool(
    'indexing_guidance_tool',
    {
      description: 'Return indexing reminders as a tool for clients that cannot invoke prompts.',
      inputSchema: {},
      outputSchema: indexingGuidanceOutputShape,
      annotations: {
        jsonSchema: indexingGuidanceToolJsonSchema
      }
    },
    async () => {
      const guidanceText = INDEXING_GUIDANCE_PROMPT.messages
        .map((message) => {
          const { content } = message;
          if (typeof content === 'string') {
            return content;
          }
          if (Array.isArray(content)) {
            return content
              .map((item) => (item && typeof item === 'object' && 'type' in item && item.type === 'text' ? item.text ?? '' : ''))
              .filter((text) => typeof text === 'string' && text.trim().length > 0)
              .join('\n');
          }
          if (content && typeof content === 'object' && 'type' in content && (content as { type?: string }).type === 'text') {
            const textValue = (content as { text?: string }).text;
            return typeof textValue === 'string' ? textValue : '';
          }
          return '';
        })
        .filter((snippet) => snippet.trim().length > 0)
        .join('\n');

      const payload = indexingGuidanceOutputSchema.parse({
        guidance:
          guidanceText ||
          'Always run ingest_codebase on new or freshly checked out codebases and after file edits so the SQLite index stays current.'
      });

      return {
        content: [
          {
            type: 'text',
            text: 'Indexing guidance provided. See structured guidance field for full reminders.'
          }
        ],
        structuredContent: payload
      };
    }
  );

  server.registerPrompt(
    'indexing_guidance',
    {
      description: 'When to run ingest_codebase to keep the index synchronized.'
    },
    async () => INDEXING_GUIDANCE_PROMPT
  );

  const transport = new StdioServerTransport();
  await server.connect(transport);
}

main().catch((error) => {
  logger.error({ err: error }, 'Unhandled server error');
  process.exitCode = 1;
});
