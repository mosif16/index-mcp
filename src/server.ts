import { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js';
import { StdioServerTransport } from '@modelcontextprotocol/sdk/server/stdio.js';
import type { GetPromptResult } from '@modelcontextprotocol/sdk/types.js';
import { z } from 'zod';

import { ingestCodebase } from './ingest.js';

const ingestToolArgs = {
  root: z.string().min(1, 'root directory is required'),
  include: z.array(z.string()).optional(),
  exclude: z.array(z.string()).optional(),
  databaseName: z.string().min(1).optional(),
  maxFileSizeBytes: z.number().int().positive().optional(),
  storeFileContent: z.boolean().optional()
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
  durationMs: z.number()
} as const;
const ingestToolOutputSchema = z.object(ingestToolOutputShape);

const SERVER_INSTRUCTIONS = [
  'Always run ingest_codebase on a new or freshly checked out codebase before asking for help.',
  'Any time you or the agent edits files, re-run ingest_codebase so the SQLite index stays current.'
].join(' ');

const INDEXING_GUIDANCE_PROMPT: GetPromptResult = {
  messages: [
    {
      role: 'assistant',
      content: {
        type: 'text',
        text: 'Always run ingest_codebase on a new codebase before requesting analysis, and run it again after you or I modify files so the SQLite index reflects the latest code.'
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
