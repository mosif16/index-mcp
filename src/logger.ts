import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import pino from 'pino';

const logDirectory =
  process.env.INDEX_MCP_LOG_DIR ?? process.env.LOG_DIR ?? path.join(os.homedir(), '.index-mcp', 'logs');
const logFileName = process.env.INDEX_MCP_LOG_FILE ?? process.env.LOG_FILE ?? 'server.log';
const logLevel = process.env.INDEX_MCP_LOG_LEVEL ?? process.env.LOG_LEVEL ?? 'info';
const logToConsole = process.env.INDEX_MCP_LOG_CONSOLE === 'true';
const consoleStreamFd = process.env.INDEX_MCP_LOG_CONSOLE_STREAM === 'stderr' ? 2 : 1;

fs.mkdirSync(logDirectory, { recursive: true });

const destinations: pino.DestinationStream[] = [
  pino.destination({ dest: path.join(logDirectory, logFileName), mkdir: true, sync: false })
];

if (logToConsole) {
  destinations.push(pino.destination({ dest: consoleStreamFd, sync: false }));
}

const baseLogger = pino(
  {
    level: logLevel,
    messageKey: 'message',
    timestamp: pino.stdTimeFunctions.isoTime,
    base: null
  },
  pino.multistream(destinations)
);

process.on('beforeExit', () => {
  try {
    baseLogger.flush();
  } catch {
    // ignore flush failures during shutdown
  }
});

export function createLogger(scope: string) {
  return baseLogger.child({ scope });
}

export const logger = createLogger('server');

let loggerShutdownPromise: Promise<void> | null = null;

export function shutdownLogger(): Promise<void> {
  if (!loggerShutdownPromise) {
    loggerShutdownPromise = (async () => {
      try {
        baseLogger.flush();
      } catch {
        // ignore flush failures during shutdown
      }
    })();
  }

  return loggerShutdownPromise;
}
