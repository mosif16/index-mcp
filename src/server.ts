import { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js';
import { StdioServerTransport } from '@modelcontextprotocol/sdk/server/stdio.js';
import type { GetPromptResult } from '@modelcontextprotocol/sdk/types.js';
import { z } from 'zod';

import { ingestCodebase } from './ingest.js';
import { semanticSearch } from './search.js';

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
    .optional()
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
  embeddingModel: z.string().nullable()
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

const SERVER_INSTRUCTIONS = [
  'Tools available: ingest_codebase (index the current codebase into SQLite), semantic_search (embedding-powered retrieval), and indexing_guidance (prompt describing when to reindex).',
  'Always run ingest_codebase on a new or freshly checked out codebase before asking for help.',
  'Any time you or the agent edits files, re-run ingest_codebase so the SQLite index stays current.'
].join(' ');

const INDEXING_GUIDANCE_PROMPT: GetPromptResult = {
  messages: [
    {
      role: 'assistant',
      content: {
        type: 'text',
        text: 'Tools: ingest_codebase (index the repository), semantic_search (find relevant snippets), and indexing_guidance (show these reminders). Always run ingest_codebase on a new codebase before requesting analysis, and run it again after you or I modify files so the SQLite index reflects the latest code.'
      }
    }
  ]
};

async function main() {
  const server = new McpServer({ name: 'index-mcp', version: '0.1.0' }, { instructions: SERVER_INSTRUCTIONS });

  server.registerTool(
    'ingest_codebase',
    {
      description:
        'Walk a codebase and store file metadata and (optionally) content in a SQLite database at the repository root.',
      inputSchema: ingestToolArgs,
      outputSchema: ingestToolOutputShape
    },
    async (args) => {
      const parsedInput = ingestToolSchema.parse(args);
      const result = ingestToolOutputSchema.parse(await ingestCodebase(parsedInput));

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
    }
  );

  server.registerTool(
    'semantic_search',
    {
      description: 'Return the most relevant indexed snippets using semantic embeddings.',
      inputSchema: semanticSearchArgs,
      outputSchema: semanticSearchOutputShape
    },
    async (args) => {
      const parsedInput = semanticSearchSchema.parse(args);
      const result = semanticSearchOutputSchema.parse(await semanticSearch(parsedInput));
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
