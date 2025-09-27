import { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js';
import { StdioServerTransport } from '@modelcontextprotocol/sdk/server/stdio.js';
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

async function main() {
  const server = new McpServer({ name: 'index-mcp', version: '0.1.0' });

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
            )}s. Database: ${result.databasePath}`
          }
        ],
        structuredContent: result
      };
    }
  );

  const transport = new StdioServerTransport();
  await server.connect(transport);
}

main().catch((error) => {
  console.error(error);
  process.exitCode = 1;
});
