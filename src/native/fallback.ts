import crypto from 'node:crypto';
import path from 'node:path';
import { promises as fs } from 'node:fs';
import fg from 'fast-glob';
import ignore, { type Ignore } from 'ignore';

import type {
  NativeFileEntry,
  NativeMetadataEntry,
  NativeMetadataOptions,
  NativeMetadataResult,
  NativeModule,
  NativeReadOptions,
  NativeReadResult,
  NativeScanOptions,
  NativeScanResult,
  NativeSkippedFile
} from '../types/native.js';

const DEFAULT_INCLUDE_PATTERNS = ['**/*'];

function toPosix(relativePath: string): string {
  return relativePath.split(path.sep).join('/');
}

function fromPosix(relativePath: string): string {
  return relativePath.split('/').join(path.sep);
}

function isBinaryBuffer(buffer: Buffer): boolean {
  const length = Math.min(buffer.length, 8192);
  for (let i = 0; i < length; i += 1) {
    if (buffer[i] === 0) {
      return true;
    }
  }
  return false;
}

async function readFileEntry(
  root: string,
  relativePath: string,
  needsContent: boolean
): Promise<NativeFileEntry> {
  const absolutePath = path.join(root, relativePath);
  const stats = await fs.stat(absolutePath);
  const raw = await fs.readFile(absolutePath);
  const hash = crypto.createHash('sha256').update(raw).digest('hex');
  const binary = isBinaryBuffer(raw);

  let content: string | null = null;
  if (needsContent && !binary) {
    content = raw.toString('utf8');
  }

  return {
    path: toPosix(relativePath),
    size: stats.size,
    modified: Math.trunc(stats.mtimeMs),
    hash,
    content,
    isBinary: binary
  };
}

async function listCandidateFiles(
  root: string,
  include: string[],
  exclude: string[]
): Promise<string[]> {
  const patterns = include.length > 0 ? include : DEFAULT_INCLUDE_PATTERNS;
  const entries = await fg(patterns, {
    cwd: root,
    dot: true,
    ignore: exclude,
    onlyFiles: true,
    followSymbolicLinks: false,
    unique: true,
    suppressErrors: true
  });
  return entries;
}

async function fallbackScanRepoMetadata(
  options: NativeMetadataOptions
): Promise<NativeMetadataResult> {
  const gitignoreMatcher = await loadGitignoreMatcher(options.root, options.exclude);
  const candidates = await listCandidateFiles(options.root, options.include, options.exclude);
  const maxBytes = options.maxFileSizeBytes ?? Number.POSITIVE_INFINITY;

  const entries: NativeMetadataEntry[] = [];
  const skipped: NativeSkippedFile[] = [];

  for (const candidate of candidates) {
    const posixPath = toPosix(candidate);
    if (path.posix.basename(posixPath) === '.gitignore') {
      continue;
    }
    if (gitignoreMatcher.ignores(posixPath)) {
      continue;
    }

    const absolutePath = path.join(options.root, candidate);

    try {
      const stats = await fs.stat(absolutePath);
      if (stats.size > maxBytes) {
        skipped.push({
          path: posixPath,
          reason: 'file-too-large',
          size: stats.size
        });
        continue;
      }

      entries.push({
        path: posixPath,
        size: stats.size,
        modified: Math.trunc(stats.mtimeMs)
      });
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      skipped.push({
        path: posixPath,
        reason: 'read-error',
        message
      });
    }
  }

  entries.sort((a, b) => a.path.localeCompare(b.path));

  return { entries, skipped };
}

async function fallbackReadRepoFiles(options: NativeReadOptions): Promise<NativeReadResult> {
  const uniquePaths = Array.from(new Set(options.paths.map((p) => toPosix(p)))).sort();
  const files: NativeFileEntry[] = [];
  const skipped: NativeSkippedFile[] = [];
  const maxBytes = options.maxFileSizeBytes ?? Number.POSITIVE_INFINITY;

  for (const relative of uniquePaths) {
    const osRelative = fromPosix(relative);
    const absolutePath = path.join(options.root, osRelative);

    let stats; // re-use stats to avoid double conversions when possible.
    try {
      stats = await fs.stat(absolutePath);
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      skipped.push({
        path: toPosix(relative),
        reason: 'read-error',
        message
      });
      continue;
    }

    if (stats.size > maxBytes) {
      skipped.push({
        path: toPosix(relative),
        reason: 'file-too-large',
        size: stats.size
      });
      continue;
    }

    try {
      const entry = await readFileEntry(options.root, osRelative, options.needsContent);
      files.push(entry);
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      skipped.push({
        path: toPosix(relative),
        reason: 'read-error',
        message
      });
    }
  }

  return { files, skipped };
}

export async function fallbackScanRepo(options: NativeScanOptions): Promise<NativeScanResult> {
  const metadata = await fallbackScanRepoMetadata({
    root: options.root,
    include: options.include,
    exclude: options.exclude,
    maxFileSizeBytes: options.maxFileSizeBytes
  });

  const readResult = await fallbackReadRepoFiles({
    root: options.root,
    paths: metadata.entries.map((entry) => entry.path),
    maxFileSizeBytes: options.maxFileSizeBytes,
    needsContent: options.needsContent
  });

  return {
    files: readResult.files,
    skipped: [...metadata.skipped, ...readResult.skipped]
  };
}

export const fallbackNativeModule: NativeModule = {
  async scanRepo(options: NativeScanOptions): Promise<NativeScanResult> {
    return fallbackScanRepo(options);
  },
  async scanRepoMetadata(options: NativeMetadataOptions): Promise<NativeMetadataResult> {
    return fallbackScanRepoMetadata(options);
  },
  async readRepoFiles(options: NativeReadOptions): Promise<NativeReadResult> {
    return fallbackReadRepoFiles(options);
  }
};

async function loadGitignoreMatcher(root: string, exclude: string[]): Promise<Ignore> {
  const matcher = ignore();

  matcher.add('.git/');

  const gitignoreFiles = await fg('**/.gitignore', {
    cwd: root,
    dot: true,
    ignore: exclude,
    onlyFiles: true,
    followSymbolicLinks: false,
    unique: true,
    suppressErrors: true
  });

  gitignoreFiles.sort();

  for (const gitignorePath of gitignoreFiles) {
    const absolutePath = path.join(root, gitignorePath);
    let contents: string;
    try {
      contents = await fs.readFile(absolutePath, 'utf8');
    } catch {
      continue;
    }

    const directory = path.posix.dirname(toPosix(gitignorePath));
    const baseDir = directory === '.' ? '' : directory;

    for (const rawLine of contents.split(/\r?\n/)) {
      const trimmed = rawLine.trim();
      if (!trimmed || trimmed.startsWith('#')) {
        continue;
      }

      const isNegated = trimmed.startsWith('!');
      const patternBody = isNegated ? trimmed.slice(1) : trimmed;
      if (!patternBody) {
        continue;
      }

      const converted = convertGitignorePattern(patternBody, baseDir);
      if (!converted) {
        continue;
      }

      matcher.add(isNegated ? `!${converted}` : converted);
    }
  }

  return matcher;
}

function convertGitignorePattern(pattern: string, baseDir: string): string {
  const normalizedPattern = pattern.replace(/\\/g, '/');
  const prefix = baseDir ? `${baseDir}/` : '';

  if (normalizedPattern.startsWith('/')) {
    const relative = normalizedPattern.slice(1);
    const anchored = prefix ? `${prefix}${relative}` : relative;
    return anchored.replace(/\/+/g, '/');
  }

  return `${prefix}**/${normalizedPattern}`.replace(/\/+/g, '/');
}
