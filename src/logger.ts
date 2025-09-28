import path from 'node:path';
import pino from 'pino';

import {
  resolveLoggerConfiguration,
  type LoggerConfiguration,
  type LoggerSetupDiagnostic
} from './logger-config.js';

const loggerConfiguration: LoggerConfiguration = resolveLoggerConfiguration(process.env);

const destinations: pino.DestinationStream[] = [];

if (loggerConfiguration.fileLoggingEnabled && loggerConfiguration.logDirectory) {
  destinations.push(
    pino.destination({
      dest: path.join(loggerConfiguration.logDirectory, loggerConfiguration.logFileName),
      mkdir: true,
      sync: false
    })
  );
}

if (loggerConfiguration.logToConsole) {
  destinations.push(pino.destination({ dest: loggerConfiguration.consoleStreamFd, sync: false }));
}

if (destinations.length === 0) {
  destinations.push(pino.destination({ dest: 1, sync: false }));
}

const baseLogger = pino(
  {
    level: loggerConfiguration.logLevel,
    messageKey: 'message',
    timestamp: pino.stdTimeFunctions.isoTime,
    base: null
  },
  pino.multistream(destinations)
);

for (const diagnostic of loggerConfiguration.diagnostics) {
  const logLevel: 'warn' | 'error' = diagnostic.level;
  baseLogger[logLevel]({ event: diagnostic.code, ...diagnostic.context }, diagnostic.message);
}

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

export function getLoggerConfiguration(): LoggerConfiguration {
  return {
    ...loggerConfiguration,
    diagnostics: [...loggerConfiguration.diagnostics]
  };
}

export function getLoggerDiagnostics(): LoggerSetupDiagnostic[] {
  return [...loggerConfiguration.diagnostics];
}

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
