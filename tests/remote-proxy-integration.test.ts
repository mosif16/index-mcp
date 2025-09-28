import assert from 'node:assert/strict';
import { once } from 'node:events';
import { createServer } from 'node:http';
import path from 'node:path';
import process from 'node:process';
import express from 'express';
import { z } from 'zod';
import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { StdioClientTransport } from '@modelcontextprotocol/sdk/client/stdio.js';
import type { CallToolResult } from '@modelcontextprotocol/sdk/types.js';
import { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js';
import { SSEServerTransport } from '@modelcontextprotocol/sdk/server/sse.js';

const TEST_BEARER_TOKEN = 'test-proxy-token';

type RemoteStats = {
  connections: number;
  toolCalls: Array<{ sessionId: string | undefined; tool: string; args: Record<string, unknown> }>;
  maxConcurrent: number;
  authFailures: number;
};

type RemoteSession = {
  transport: SSEServerTransport;
  server: McpServer;
  callCount: number;
};

async function startRemoteSseServer(): Promise<{
  baseUrl: string;
  close: () => Promise<void>;
  stats: RemoteStats;
}> {
  const app = express();
  app.use(express.json());

  const sessions = new Map<string, RemoteSession>();
  const stats: RemoteStats = {
    connections: 0,
    toolCalls: [],
    maxConcurrent: 0,
    authFailures: 0
  };

  function assertAuth(header: string | undefined) {
    if (header !== `Bearer ${TEST_BEARER_TOKEN}`) {
      stats.authFailures += 1;
      const error = new Error('Unauthorized');
      (error as { status?: number }).status = 401;
      throw error;
    }
  }

  const activeSessions = new Set<string>();

  function makeServerForConnection(connectionIndex: number): McpServer {
    const server = new McpServer(
      {
        name: 'remote-proxy-test-server',
        version: '0.0.1'
      },
      { capabilities: { logging: {} } }
    );

    const echoInputSchema = z.object({
      text: z.string()
    });

    server.registerTool(
      'echo',
      {
        description: 'Echo back text payload',
        inputSchema: echoInputSchema
      },
      async ({ text }, extra) => {
        stats.toolCalls.push({ sessionId: extra.sessionId, tool: 'echo', args: { text } });
        const sessionId = extra.sessionId;
        if (sessionId) {
          const session = sessions.get(sessionId);
          if (session) {
            session.callCount += 1;
            if (session.callCount === 1) {
              // Simulate upstream restart immediately after first call to force reconnects.
              setTimeout(() => {
                session.transport.close().catch(() => {});
              }, 25);
            }
          }
        }

        return {
          content: [
            {
              type: 'text',
              text: `remote:${text}`
            }
          ]
        } satisfies CallToolResult;
      }
    );

    if (connectionIndex >= 2) {
      const statsInputSchema = z.object({});

      server.registerTool(
        'stats',
        {
          description: 'Return remote stats snapshot',
          inputSchema: statsInputSchema
        },
        async (_args, extra) => {
          stats.toolCalls.push({ sessionId: extra.sessionId, tool: 'stats', args: {} });
          return {
            content: [
              {
                type: 'text',
                text: JSON.stringify({
                  connections: stats.connections,
                  toolCalls: stats.toolCalls.length
                })
              }
            ]
          } satisfies CallToolResult;
        }
      );
    }

    return server;
  }

  app.get('/mcp', async (req, res) => {
    try {
      assertAuth(typeof req.headers.authorization === 'string' ? req.headers.authorization : undefined);
    } catch (error) {
      const status = (error as { status?: number }).status ?? 500;
      res.status(status).send((error as Error).message);
      return;
    }

    try {
      const transport = new SSEServerTransport('/messages', res);
      const sessionId = transport.sessionId;
      activeSessions.add(sessionId);
      stats.connections += 1;
      stats.maxConcurrent = Math.max(stats.maxConcurrent, activeSessions.size);

      transport.onclose = () => {
        activeSessions.delete(sessionId);
        sessions.delete(sessionId);
      };

      const server = makeServerForConnection(stats.connections);

      await server.connect(transport);

      sessions.set(sessionId, {
        transport,
        server,
        callCount: 0
      });
    } catch (error) {
      res.status(500).send((error as Error).message);
    }
  });

  app.post('/messages', async (req, res) => {
    let sessionId: string | undefined;
    try {
      assertAuth(typeof req.headers.authorization === 'string' ? req.headers.authorization : undefined);
      sessionId = typeof req.query.sessionId === 'string' ? req.query.sessionId : undefined;
      if (!sessionId) {
        res.status(400).send('Missing sessionId parameter');
        return;
      }

      const session = sessions.get(sessionId);
      if (!session) {
        res.status(404).send('Session not found');
        return;
      }

      await session.transport.handlePostMessage(req, res, req.body);
    } catch (error) {
      if (!res.headersSent) {
        const status = (error as { status?: number }).status ?? 500;
        res.status(status).send((error as Error).message);
      }
    }
  });

  const httpServer = createServer(app);
  httpServer.listen(0);
  await once(httpServer, 'listening');
  const address = httpServer.address();
  if (!address || typeof address === 'string') {
    throw new Error('Failed to bind remote test server');
  }

  const baseUrl = `http://127.0.0.1:${address.port}/mcp`;

  return {
    baseUrl,
    stats,
    close: async () => {
      await Promise.all([...sessions.values()].map(async (session) => session.transport.close().catch(() => {})));
      await new Promise<void>((resolve, reject) => {
        httpServer.close((error) => {
          if (error) {
            reject(error);
            return;
          }
          resolve();
        });
      });
    }
  };
}

async function waitForTool(client: Client, toolName: string, attempts = 20, delayMs = 100): Promise<void> {
  for (let attempt = 0; attempt < attempts; attempt += 1) {
    const tools = (await client.listTools({})).tools;
    if (tools.some((tool) => tool.name === toolName)) {
      return;
    }
    await new Promise((resolve) => setTimeout(resolve, delayMs));
  }
  throw new Error(`Timed out waiting for tool ${toolName}`);
}

async function run() {
  const remote = await startRemoteSseServer();

  const stderrChunks: string[] = [];

  const transport = new StdioClientTransport({
    command: path.resolve('node_modules/.bin/tsx'),
    args: ['src/server.ts'],
    env: {
      ...process.env,
      INDEX_MCP_REMOTE_SERVERS: JSON.stringify([
        {
          name: 'remote',
          namespace: 'remote',
          url: remote.baseUrl,
          auth: { type: 'bearer', tokenEnv: 'TEST_PROXY_TOKEN' },
          retry: {
            maxAttempts: 20,
            initialDelayMs: 50,
            maxDelayMs: 500,
            backoffMultiplier: 2
          }
        }
      ]),
      TEST_PROXY_TOKEN: TEST_BEARER_TOKEN,
      INDEX_MCP_LOG_CONSOLE: 'true',
      INDEX_MCP_LOG_CONSOLE_STREAM: 'stderr',
      INDEX_MCP_LOG_LEVEL: 'debug'
    },
    stderr: 'pipe',
    cwd: process.cwd()
  });

  const stderrStream = transport.stderr;
  if (stderrStream) {
    stderrStream.setEncoding('utf8');
    stderrStream.on('data', (chunk: string) => {
      stderrChunks.push(chunk);
    });
  }

  await transport.start();

  const client = new Client({ name: 'proxy-integration-test', version: '0.0.0' });
  await client.connect(transport);

  try {
    const initialTools = (await client.listTools({})).tools;
    const initialToolNames = new Set(initialTools.map((tool) => tool.name));
    assert(initialToolNames.has('ingest_codebase'), 'Local tools should remain available');

    await waitForTool(client, 'remote.echo');

    const echoResponse = await client.callTool({
      name: 'remote.echo',
      arguments: { text: 'hello' }
    });
    assert.equal(echoResponse.content?.[0]?.type, 'text');
    assert.equal(echoResponse.content?.[0]?.text, 'remote:hello');

    await waitForTool(client, 'remote.stats');

    const statsResponse = await client.callTool({
      name: 'remote.stats',
      arguments: {}
    });
    assert.equal(statsResponse.content?.[0]?.type, 'text');
    assert.ok(statsResponse.content?.[0]?.text?.includes('"connections"'));

    const finalTools = (await client.listTools({})).tools;
    const uniqueToolNames = new Set(finalTools.map((tool) => tool.name));
    assert.equal(uniqueToolNames.size, finalTools.length, 'Tool list should not contain duplicates');

    assert.equal(remote.stats.authFailures, 0, 'Auth headers should be accepted by the remote server');
    assert(remote.stats.maxConcurrent <= 1, 'Remote proxy should reuse connections and avoid unbounded growth');
    assert(remote.stats.toolCalls.length >= 2, 'Remote tools should have been invoked multiple times');

    const combinedStderr = stderrChunks.join('');
    assert.notEqual(combinedStderr.length, 0, 'Server logs should be routed to stderr');
  } finally {
    await client.close();
    await transport.close();
    await remote.close();
  }
}

run().catch((error) => {
  console.error(error);
  process.exitCode = 1;
});
