import crypto from 'node:crypto';
import path from 'node:path';
import { promises as fs } from 'node:fs';
import fg from 'fast-glob';
import ignore, { type Ignore } from 'ignore';

import type {
  NativeFileEntry,
  NativeModule,
  NativeScanOptions,
  NativeScanResult,
  NativeSkippedFile
} from '../types/native.js';

function toPosix(relativePath: string): string {
  return relativePath.split(path.sep).join('/');
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

export async function fallbackScanRepo(options: NativeScanOptions): Promise<NativeScanResult> {
  const maxBytes = options.maxFileSizeBytes ?? Number.POSITIVE_INFINITY;
  const patterns = options.include.length > 0 ? options.include : ['**/*'];

  const gitignoreMatcher = await loadGitignoreMatcher(options.root, options.exclude);

  const entries = await fg(patterns, {
    cwd: options.root,
    dot: true,
    ignore: options.exclude,
    onlyFiles: true,
    followSymbolicLinks: false,
    unique: true,
    suppressErrors: true
  });

  const files: NativeFileEntry[] = [];
  const skipped: NativeSkippedFile[] = [];

  for (const entry of entries) {
    const relativePath = entry;
    const absolutePath = path.join(options.root, relativePath);
    const posixPath = toPosix(relativePath);

    if (path.posix.basename(posixPath) === '.gitignore') {
      continue;
    }

    if (gitignoreMatcher.ignores(posixPath)) {
      continue;
    }

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

      const fileEntry = await readFileEntry(options.root, relativePath, options.needsContent);
      files.push(fileEntry);
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      skipped.push({
        path: posixPath,
        reason: 'read-error',
        message
      });
    }
  }

  files.sort((a, b) => a.path.localeCompare(b.path));

  return { files, skipped };
}

export const fallbackNativeModule: NativeModule = {
  async scanRepo(options: NativeScanOptions): Promise<NativeScanResult> {
    return fallbackScanRepo(options);
  }
};

async function loadGitignoreMatcher(root: string, exclude: string[]): Promise<Ignore> {
  const matcher = ignore();

  // Always ignore the .git directory by default.
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
