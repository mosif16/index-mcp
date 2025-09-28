import { once } from 'node:events';
import { createServer } from 'node:http';
import express from 'express';
import { z } from 'zod';
import { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js';
import { SSEServerTransport } from '@modelcontextprotocol/sdk/server/sse.js';

import { createLogger, shutdownLogger } from '../logger.js';
import { getPackageMetadata } from '../package-metadata.js';

const logger = createLogger('backend');
const { name: packageName, version: packageVersion } = getPackageMetadata();

const BACKEND_HOST = process.env.LOCAL_BACKEND_HOST ?? '127.0.0.1';
const BACKEND_PORT = Number.parseInt(process.env.LOCAL_BACKEND_PORT ?? '8765', 10);
const SSE_PATH = process.env.LOCAL_BACKEND_PATH ?? '/mcp';
const MESSAGE_PATH = process.env.LOCAL_BACKEND_MESSAGES_PATH ?? '/messages';
const START_TIME = Date.now();

if (Number.isNaN(BACKEND_PORT) || BACKEND_PORT <= 0) {
  throw new Error(`Invalid LOCAL_BACKEND_PORT value: ${process.env.LOCAL_BACKEND_PORT}`);
}

type SessionRecord = {
  transport: SSEServerTransport;
  server: McpServer;
};

const sessions = new Map<string, SessionRecord>();

function buildServer(): McpServer {
  const server = new McpServer(
    {
      name: `${packageName}-local-backend`,
      version: packageVersion
    },
    {
      capabilities: {
        logging: {}
      }
    }
  );

  const pingInputSchema = z
    .object({
      message: z.string().optional()
    })
    .describe('Optional message field to override the default "pong" reply.');

  server.registerTool(
    'ping',
    {
      title: 'Ping',
      description: 'Simple round-trip test that echoes back a payload.',
      inputSchema: pingInputSchema.shape
    },
    async (rawArgs) => {
      const { message } = pingInputSchema.parse(rawArgs ?? {});
      const reply = message && message.trim().length > 0 ? message : 'pong';
      return {
        content: [
          {
            type: 'text',
            text: reply
          }
        ]
      };
    }
  );

  const infoInputSchema = z.object({}).describe('No parameters required.');

  server.registerTool(
    'backend-info',
    {
      title: 'Backend Information',
      description: 'Returns metadata about the standalone backend instance.',
      inputSchema: infoInputSchema.shape
    },
    async (rawArgs) => {
      infoInputSchema.parse(rawArgs ?? {});
      const uptimeSeconds = Math.floor((Date.now() - START_TIME) / 1000);
      const info = {
        name: `${packageName}-local-backend`,
        version: packageVersion,
        uptimeSeconds,
        messagePath: MESSAGE_PATH
      };
      return {
        content: [
          {
            type: 'text',
            text: JSON.stringify(info)
          }
        ]
      };
    }
  );

  return server;
}

const app = express();
app.use(express.json());

app.get('/healthz', (_req, res) => {
  res.json({ status: 'ok', uptimeSeconds: Math.floor((Date.now() - START_TIME) / 1000) });
});

app.get(SSE_PATH, async (req, res) => {
  try {
    const transport = new SSEServerTransport(MESSAGE_PATH, res);
    const sessionId = transport.sessionId;
    logger.info({ sessionId }, 'Accepted SSE client connection');

    transport.onclose = () => {
      logger.info({ sessionId }, 'SSE connection closed');
      sessions.delete(sessionId);
    };

    transport.onerror = (error) => {
      logger.warn({ sessionId, err: error }, 'SSE transport error');
    };

    const server = buildServer();
    await server.connect(transport);
    sessions.set(sessionId, { transport, server });
  } catch (error) {
    logger.error({ err: error }, 'Failed to establish SSE connection');
    if (!res.headersSent) {
      res.status(500).send((error as Error).message);
    }
  }
});

app.post(MESSAGE_PATH, async (req, res) => {
  const sessionId = typeof req.query.sessionId === 'string' ? req.query.sessionId : undefined;
  if (!sessionId) {
    res.status(400).send('Missing sessionId query parameter');
    return;
  }

  const session = sessions.get(sessionId);
  if (!session) {
    res.status(404).send('Session not found');
    return;
  }

  try {
    await session.transport.handlePostMessage(req, res, req.body);
  } catch (error) {
    logger.warn({ sessionId, err: error }, 'Error handling POST message');
    if (!res.headersSent) {
      res.status(500).send((error as Error).message);
    }
  }
});

app.delete(MESSAGE_PATH, async (req, res) => {
  const sessionId = typeof req.query.sessionId === 'string' ? req.query.sessionId : undefined;
  if (!sessionId) {
    res.status(400).send('Missing sessionId query parameter');
    return;
  }

  const session = sessions.get(sessionId);
  if (!session) {
    res.status(404).send('Session not found');
    return;
  }

  try {
    await session.transport.close();
  } catch (error) {
    logger.warn({ sessionId, err: error }, 'Error closing transport');
  } finally {
    sessions.delete(sessionId);
  }

  res.status(204).send();
});

const httpServer = createServer(app);

async function shutdown(signal: string) {
  logger.info({ signal }, 'Shutting down local backend');
  for (const [sessionId, session] of sessions.entries()) {
    try {
      await session.transport.close();
    } catch (error) {
      logger.warn({ sessionId, err: error }, 'Error closing transport during shutdown');
    }
  }
  sessions.clear();
  httpServer.close();
  await once(httpServer, 'close').catch(() => {});
  await shutdownLogger();
}

process.on('SIGINT', () => {
  shutdown('SIGINT').finally(() => process.exit(0));
});

process.on('SIGTERM', () => {
  shutdown('SIGTERM').finally(() => process.exit(0));
});

httpServer.on('error', (error) => {
  logger.error({ err: error }, 'HTTP server error');
  process.exitCode = 1;
});

httpServer.listen(BACKEND_PORT, BACKEND_HOST, () => {
  logger.info({ host: BACKEND_HOST, port: BACKEND_PORT, ssePath: SSE_PATH, messagePath: MESSAGE_PATH }, 'Local backend listening');
});
