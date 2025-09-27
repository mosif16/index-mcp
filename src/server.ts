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
  const requestInfo = (extra as { requestInfo?: { headers?: unknown } }).requestInfo;

  const context: RootResolutionContext = {};

  if (meta && typeof meta === 'object') {
    context.meta = meta as Record<string, unknown>;
  }

  const headers = requestInfo?.headers ? normalizeHeaders(requestInfo.headers) : undefined;
  if (headers) {
    context.headers = headers;
  }

  return context;
}

const ingestToolArgs = {
  root: z.string().min(1, 'root directory is required'),
  include: z.array(z.string()).optional(),
  exclude: z.array(z.string()).optional(),
  databaseName: z.string().min(1).optional(),
  maxFileSizeBytes: z.number().int().positive().optional(),
  storeFileContent: z.boolean().optional(),
  contentSanitizer: z
    .object({
      module: z.string().min(1, 'module specifier is required'),
      exportName: z.string().min(1).optional(),
      options: z.unknown().optional()
    })
    .optional(),
  embedding: z
    .object({
      enabled: z.boolean().optional(),
      model: z.string().min(1).optional(),
      chunkSizeTokens: z.number().int().positive().optional(),
      chunkOverlapTokens: z.number().int().min(0).optional(),
      batchSize: z.number().int().positive().optional()
    })
    .optional(),
  graph: z
    .object({
      enabled: z.boolean().optional()
    })
    .optional(),
  paths: z.array(z.string()).optional()
} as const;
const ingestToolSchema = z.object(ingestToolArgs);

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

const semanticSearchArgs = {
  root: z.string().min(1, 'root directory is required'),
  query: z.string().min(1, 'query text is required'),
  databaseName: z.string().min(1).optional(),
  limit: z.number().int().positive().max(50).optional(),
  model: z.string().min(1).optional()
} as const;
const semanticSearchSchema = z.object(semanticSearchArgs);

const semanticSearchMatchSchema = z.object({
  path: z.string(),
  chunkIndex: z.number().int(),
  score: z.number(),
  content: z.string(),
  embeddingModel: z.string()
});
const semanticSearchOutputShape = {
  databasePath: z.string(),
  embeddingModel: z.string().nullable(),
  totalChunks: z.number().int().min(0),
  evaluatedChunks: z.number().int().min(0),
  results: z.array(semanticSearchMatchSchema)
} as const;
const semanticSearchOutputSchema = z.object(semanticSearchOutputShape);

const graphNeighborArgs = {
  root: z.string().min(1, 'root directory is required'),
  databaseName: z.string().min(1).optional(),
  node: z
    .object({
      id: z.string().optional(),
      path: z.string().nullable().optional(),
      kind: z.string().optional(),
      name: z.string().min(1, 'node name is required')
    })
    .strict(),
  direction: z.enum(['incoming', 'outgoing', 'both']).optional(),
  limit: z.number().int().positive().max(100).optional()
} as const;
const graphNeighborSchema = z.object(graphNeighborArgs);

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

const SERVER_INSTRUCTIONS = [
  'Tools available: ingest_codebase (index the current codebase into SQLite), semantic_search (embedding-powered retrieval), graph_neighbors (explore GraphRAG relationships), and indexing_guidance (prompt describing when to reindex).',
  'Use this MCP server for all repository-aware searches: run ingest_codebase to refresh context, rely on semantic_search for locating code or docs, and use graph_neighbors when you need structural call/import details before considering any other lookup method.',
  'Always run ingest_codebase on a new or freshly checked out codebase before asking for help.',
  'Always exclude files and folders matched by .gitignore patterns so ignored content never enters the index.',
  'Any time you or the agent edits files, re-run ingest_codebase so the SQLite index stays current.'
].join(' ');

const INDEXING_GUIDANCE_PROMPT: GetPromptResult = {
  messages: [
    {
      role: 'assistant',
      content: {
        type: 'text',
        text: 'Tools: ingest_codebase (index the repository), semantic_search (find relevant snippets), graph_neighbors (inspect code graph relationships), and indexing_guidance (show these reminders). Always run ingest_codebase on a new codebase before requesting analysis, and run it again after you or I modify files so the SQLite index reflects the latest code.'
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
      console.error('[server] Failed to start watch daemon:', error);
    }
  }

  const server = new McpServer({ name: 'index-mcp', version: '0.1.0' }, { instructions: SERVER_INSTRUCTIONS });

  server.registerTool(
    'ingest_codebase',
    {
      description:
        'Walk a codebase and store file metadata and (optionally) content in a SQLite database at the repository root.',
      inputSchema: ingestToolArgs,
      outputSchema: ingestToolOutputShape
    },
    async (args, extra) => {
      try {
        const parsedInput = ingestToolSchema.parse(args);
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
      inputSchema: semanticSearchArgs,
      outputSchema: semanticSearchOutputShape
    },
    async (args, extra) => {
      try {
        const parsedInput = semanticSearchSchema.parse(args);
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
      inputSchema: graphNeighborArgs,
      outputSchema: graphNeighborOutputShape
    },
    async (args, extra) => {
      try {
        const parsedInput = graphNeighborSchema.parse(args);
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
  console.error(error);
  process.exitCode = 1;
});
