import chokidar from 'chokidar';
import path from 'node:path';

import { DEFAULT_DB_FILENAME, DEFAULT_EXCLUDE_GLOBS, DEFAULT_INCLUDE_GLOBS } from './constants.js';
import { ingestCodebase, type IngestOptions, type IngestResult } from './ingest.js';

export interface WatcherOptions {
  root: string;
  includeGlobs?: string[];
  excludeGlobs?: string[];
  databaseName?: string;
  debounceMs?: number;
  runInitial?: boolean;
  quiet?: boolean;
  maxFileSizeBytes?: number;
  storeFileContent?: boolean;
  embedding?: IngestOptions['embedding'];
  contentSanitizer?: IngestOptions['contentSanitizer'];
  graph?: IngestOptions['graph'];
}

export interface WatcherHandle {
  stop(): Promise<void>;
}

const DEFAULT_DEBOUNCE_MS = 500;

function toPosixPath(input: string): string {
  return input.split(path.sep).join('/');
}

export async function startIngestWatcher(options: WatcherOptions): Promise<WatcherHandle> {
  const absoluteRoot = path.resolve(options.root);
  const includeGlobs = options.includeGlobs?.length ? options.includeGlobs : DEFAULT_INCLUDE_GLOBS;
  const databaseName = options.databaseName ?? DEFAULT_DB_FILENAME;
  const excludeGlobs = Array.from(
    new Set([
      ...DEFAULT_EXCLUDE_GLOBS,
      ...(options.excludeGlobs ?? []),
      `**/${databaseName}`
    ])
  );

  const debounceMs = Number.isFinite(options.debounceMs)
    ? Math.max(50, Number(options.debounceMs))
    : DEFAULT_DEBOUNCE_MS;
  const runInitial = options.runInitial !== false;
  const quiet = options.quiet === true;

  const changedPaths = new Set<string>();
  const removedPaths = new Set<string>();

  let timer: ReturnType<typeof setTimeout> | null = null;
  let isIngesting = false;
  let rerunRequested = false;

  async function executeIngest(reason: string): Promise<void> {
    if (isIngesting) {
      rerunRequested = true;
      return;
    }

    const targetPaths = [...new Set([...changedPaths, ...removedPaths])];
    changedPaths.clear();
    removedPaths.clear();

    const ingestOptions: IngestOptions = {
      root: absoluteRoot,
      include: includeGlobs,
      exclude: excludeGlobs,
      databaseName,
      maxFileSizeBytes: options.maxFileSizeBytes,
      storeFileContent: options.storeFileContent,
      contentSanitizer: options.contentSanitizer,
      embedding: options.embedding,
      graph: options.graph,
      paths: targetPaths.length ? targetPaths : undefined
    };

    if (!targetPaths.length && reason !== 'initial') {
      return;
    }

    isIngesting = true;
    const startedAt = Date.now();
    try {
      const result: IngestResult = await ingestCodebase(ingestOptions);
      if (!quiet) {
        const durationSec = ((Date.now() - startedAt) / 1000).toFixed(2);
        const changeDescriptor = targetPaths.length ? `${targetPaths.length} path(s)` : 'full scan';
        console.log(
          `[watcher] ${reason} ingest (${changeDescriptor}) completed in ${durationSec}s: ${result.ingestedFileCount} indexed, ${result.deletedPaths.length} deleted, ${result.skipped.length} skipped.`
        );
      }
    } catch (error) {
      console.error('[watcher] ingest failed:', error);
    } finally {
      isIngesting = false;
      if (rerunRequested) {
        rerunRequested = false;
        scheduleIngest(0, 'queued');
      }
    }
  }

  function scheduleIngest(delay: number, reason = 'change') {
    if (timer) {
      clearTimeout(timer);
    }
    timer = setTimeout(() => {
      timer = null;
      void executeIngest(reason);
    }, delay);
  }

  function trackChange(relativePath: string) {
    const normalized = toPosixPath(relativePath);
    if (!normalized) {
      return;
    }
    removedPaths.delete(normalized);
    changedPaths.add(normalized);
    scheduleIngest(debounceMs);
  }

  function trackRemoval(relativePath: string) {
    const normalized = toPosixPath(relativePath);
    if (!normalized) {
      return;
    }
    changedPaths.delete(normalized);
    removedPaths.add(normalized);
    scheduleIngest(debounceMs);
  }

  const watcher = chokidar.watch(includeGlobs, {
    cwd: absoluteRoot,
    ignored: excludeGlobs,
    persistent: true,
    ignoreInitial: true,
    awaitWriteFinish: {
      stabilityThreshold: Math.min(750, debounceMs),
      pollInterval: 50
    }
  });

  watcher
    .on('add', (filePath) => trackChange(filePath))
    .on('change', (filePath) => trackChange(filePath))
    .on('unlink', (filePath) => trackRemoval(filePath))
    .on('error', (error) => console.error('[watcher] error:', error));

  if (!quiet) {
    console.log(
      `[watcher] Watching ${absoluteRoot} (debounce ${debounceMs}ms, db ${databaseName}) for incremental ingest updates.`
    );
  }

  if (runInitial) {
    await executeIngest('initial');
  }

  return {
    async stop() {
      if (timer) {
        clearTimeout(timer);
      }
      await watcher.close();
    }
  };
}
