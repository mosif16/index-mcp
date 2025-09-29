import process from 'node:process';

import { createLogger, shutdownLogger } from './logger.js';
import { clearEmbeddingPipelineCache } from './embedding.js';
import { resetNativeModuleCache } from './native/index.js';

const log = createLogger('cleanup');

type CleanupTask = () => void | Promise<void>;

const cleanupTasks: CleanupTask[] = [];
let cleanupPromise: Promise<void> | null = null;
let hooksInstalled = false;

export function registerCleanupTask(task: CleanupTask): () => void {
  cleanupTasks.push(task);
  return () => {
    const index = cleanupTasks.indexOf(task);
    if (index !== -1) {
      cleanupTasks.splice(index, 1);
    }
  };
}

export function ensureCleanupHooks(): void {
  if (hooksInstalled) {
    return;
  }

  hooksInstalled = true;

  type CleanupSignal = 'SIGINT' | 'SIGTERM' | 'SIGQUIT' | 'SIGHUP';

  const handleSignal = (signal: CleanupSignal) => {
    log.info({ signal }, 'Received termination signal; running cleanup');
    runCleanup()
      .catch((error) => {
        log.error({ err: error }, 'Cleanup failed during signal handling');
      })
      .finally(() => {
        process.exit();
      });
  };

  const cleanupSignals: CleanupSignal[] = ['SIGINT', 'SIGTERM', 'SIGQUIT', 'SIGHUP'];

  for (const signal of cleanupSignals) {
    process.once(signal, handleSignal);
  }

  process.once('beforeExit', () => {
    if (!cleanupPromise) {
      void runCleanup().catch((error) => {
        log.error({ err: error }, 'Cleanup failed during beforeExit hook');
      });
    }
  });
}

export function runCleanup(): Promise<void> {
  if (!cleanupPromise) {
    cleanupPromise = (async () => {
      const tasks = cleanupTasks.splice(0).reverse();
      for (const task of tasks) {
        try {
          await task();
        } catch (error) {
          log.warn({ err: error }, 'Cleanup task failed');
        }
      }

      try {
        await clearEmbeddingPipelineCache();
      } catch (error) {
        log.warn({ err: error }, 'Failed to clear embedding pipeline cache during cleanup');
      }

      try {
        resetNativeModuleCache();
      } catch (error) {
        log.warn({ err: error }, 'Failed to reset native module cache during cleanup');
      }

      try {
        await shutdownLogger();
      } catch (error) {
        log.warn({ err: error }, 'Failed to flush logger during cleanup');
      }
    })().finally(() => {
      cleanupPromise = null;
    });
  }

  return cleanupPromise;
}
