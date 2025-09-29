import { execFile } from 'node:child_process';
import { promises as fs } from 'node:fs';
import path from 'node:path';
import { promisify } from 'node:util';

const execFileAsync = promisify(execFile);

const GIT_LOG_FIELD_SEPARATOR = '\u001f';
const GIT_LOG_RECORD_SEPARATOR = '\u001e';

export interface RepositoryTimelineOptions {
  root: string;
  branch?: string;
  limit?: number;
  since?: string;
  includeMerges?: boolean;
  includeFileStats?: boolean;
  includeDiffs?: boolean;
  paths?: string[];
  diffPattern?: string;
}

export interface RepositoryTimelineFileChange {
  path: string;
  insertions: number | null;
  deletions: number | null;
  net: number | null;
}

export interface RepositoryTimelineEntry {
  sha: string;
  subject: string;
  summary: string;
  author: {
    name: string;
    email: string;
  };
  authorDate: string;
  committer: {
    name: string;
    email: string;
  };
  committerDate: string;
  parents: string[];
  isMerge: boolean;
  pullRequestNumber: number | null;
  filesChanged: number;
  insertions: number;
  deletions: number;
  fileChanges: RepositoryTimelineFileChange[];
  diff?: string | null;
}

export interface RepositoryTimelineResult {
  repositoryRoot: string;
  branch: string;
  limit: number;
  since?: string;
  includeMerges: boolean;
  includeFileStats: boolean;
  includeDiffs: boolean;
  paths?: string[];
  diffPattern?: string;
  totalCommits: number;
  mergeCommits: number;
  totalInsertions: number;
  totalDeletions: number;
  entries: RepositoryTimelineEntry[];
}

async function verifyGitRepository(root: string): Promise<string> {
  const stats = await fs.stat(root);
  if (!stats.isDirectory()) {
    throw new Error(`Repository root must be a directory: ${root}`);
  }

  try {
    const { stdout } = await execFileAsync('git', ['rev-parse', '--show-toplevel'], {
      cwd: root,
      maxBuffer: 10 * 1024 * 1024,
      encoding: 'utf8'
    });
    return stdout.trim();
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    throw new Error(`Failed to resolve git repository for ${root}: ${message}`);
  }
}

function normalizeSinceInput(value: string): string {
  const trimmed = value.trim();
  const relativeMatch = /^([0-9]+)\s*(d|w|m|y)$/i.exec(trimmed);
  if (relativeMatch) {
    const amount = relativeMatch[1];
    const unit = relativeMatch[2].toLowerCase();
    switch (unit) {
      case 'd':
        return `${amount}.days`;
      case 'w':
        return `${amount}.weeks`;
      case 'm':
        return `${amount}.months`;
      case 'y':
        return `${amount}.years`;
      default:
        break;
    }
  }
  return trimmed;
}

function parsePullRequestNumber(subject: string): number | null {
  const patterns = [
    /\(#(\d+)\)/,
    /\bPR\s*#(\d+)/i,
    /\b#(\d+)\b/
  ];

  for (const pattern of patterns) {
    const match = pattern.exec(subject);
    if (match) {
      const value = Number.parseInt(match[1], 10);
      if (!Number.isNaN(value)) {
        return value;
      }
    }
  }

  return null;
}

function parseGitLog(
  output: string,
  includeFileStats: boolean,
  includeDiffs: boolean
): RepositoryTimelineEntry[] {
  const entries: RepositoryTimelineEntry[] = [];
  const records = output.split(GIT_LOG_RECORD_SEPARATOR);

  for (const rawRecord of records) {
    const record = rawRecord.trim();
    if (!record) {
      continue;
    }

    const [headerLine, ...statLines] = record.split('\n');
    const fields = headerLine.split(GIT_LOG_FIELD_SEPARATOR);
    if (fields.length < 9) {
      continue;
    }

    const [
      sha,
      authorName,
      authorEmail,
      authorDate,
      committerName,
      committerEmail,
      committerDate,
      subject,
      parentsRaw
    ] = fields as [string, string, string, string, string, string, string, string, string];

    const parents = parentsRaw ? parentsRaw.split(' ').filter(Boolean) : [];
    const isMerge = parents.length > 1;

    let insertions = 0;
    let deletions = 0;
    const fileChanges: RepositoryTimelineFileChange[] = [];

    let diffStartIndex = -1;

    if (includeFileStats) {
      for (let i = 0; i < statLines.length; i += 1) {
        const rawLine = statLines[i];
        if (includeDiffs && rawLine.startsWith('diff --git ')) {
          diffStartIndex = i;
          break;
        }

        const trimmedLine = rawLine.trim();
        if (!trimmedLine) {
          continue;
        }
        const parts = trimmedLine.split('\t');
        if (parts.length < 3) {
          continue;
        }

        const [insertPart, deletePart, filePath] = parts;
        const insertValue = insertPart === '-' ? null : Number.parseInt(insertPart, 10);
        const deleteValue = deletePart === '-' ? null : Number.parseInt(deletePart, 10);

        const parsedInsertions = Number.isNaN(insertValue ?? NaN) ? null : insertValue;
        const parsedDeletions = Number.isNaN(deleteValue ?? NaN) ? null : deleteValue;
        const net = parsedInsertions !== null && parsedDeletions !== null
          ? parsedInsertions - parsedDeletions
          : null;

        if (parsedInsertions !== null) {
          insertions += parsedInsertions;
        }
        if (parsedDeletions !== null) {
          deletions += parsedDeletions;
        }

        fileChanges.push({
          path: filePath,
          insertions: parsedInsertions,
          deletions: parsedDeletions,
          net
        });
      }
    }

    let diff: string | null = null;
    if (includeDiffs) {
      if (diffStartIndex === -1) {
        diffStartIndex = statLines.findIndex((line) => line.startsWith('diff --git '));
      }

      if (diffStartIndex !== -1) {
        const patchLines = statLines.slice(diffStartIndex);
        const patchText = patchLines.join('\n').trim();
        diff = patchText.length > 0 ? patchText : null;
      }
    }

    const entry: RepositoryTimelineEntry = {
      sha,
      subject,
      summary: subject,
      author: {
        name: authorName,
        email: authorEmail
      },
      authorDate,
      committer: {
        name: committerName,
        email: committerEmail
      },
      committerDate,
      parents,
      isMerge,
      pullRequestNumber: parsePullRequestNumber(subject),
      filesChanged: includeFileStats ? fileChanges.length : 0,
      insertions,
      deletions,
      fileChanges,
      ...(includeDiffs ? { diff: diff ?? null } : {})
    };

    entries.push(entry);
  }

  return entries;
}

async function runGitLog(options: RepositoryTimelineOptions, repoRoot: string): Promise<string> {
  const args: string[] = ['log', '--no-color', '--date-order'];

  const limit = options.limit ?? 20;
  args.push(`--max-count=${limit}`);

  if (options.includeDiffs) {
    args.push('--patch');
  }

  const formatParts = [
    '%H',
    '%an',
    '%ae',
    '%aI',
    '%cn',
    '%ce',
    '%cI',
    '%s',
    '%P'
  ];

  args.push(`--format=${GIT_LOG_RECORD_SEPARATOR}${formatParts.join(GIT_LOG_FIELD_SEPARATOR)}`);

  if (options.includeFileStats !== false) {
    args.push('--numstat');
  }

  if (options.includeMerges === false) {
    args.push('--no-merges');
  }

  const diffPattern = typeof options.diffPattern === 'string' ? options.diffPattern.trim() : '';
  if (diffPattern) {
    args.push('-G', diffPattern);
  }

  if (options.since) {
    args.push(`--since=${normalizeSinceInput(options.since)}`);
  }

  args.push(options.branch ?? 'HEAD');

  const pathFilters = Array.isArray(options.paths)
    ? options.paths.map((value) => value.trim()).filter((value) => value.length > 0)
    : [];

  if (pathFilters.length > 0) {
    args.push('--', ...pathFilters);
  }

  try {
    const { stdout } = await execFileAsync('git', args, {
      cwd: repoRoot,
      maxBuffer: options.includeDiffs ? 50 * 1024 * 1024 : 20 * 1024 * 1024,
      encoding: 'utf8'
    });
    return stdout;
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    throw new Error(`Failed to read git history: ${message}`);
  }
}

export async function getRepositoryTimeline(
  options: RepositoryTimelineOptions
): Promise<RepositoryTimelineResult> {
  const absoluteRoot = path.resolve(options.root);
  const repoRoot = await verifyGitRepository(absoluteRoot);

  const logOutput = await runGitLog(options, repoRoot);
  const entries = parseGitLog(
    logOutput,
    options.includeFileStats !== false,
    options.includeDiffs === true
  );

  let totalInsertions = 0;
  let totalDeletions = 0;
  let mergeCommits = 0;

  for (const entry of entries) {
    totalInsertions += entry.insertions;
    totalDeletions += entry.deletions;
    if (entry.isMerge) {
      mergeCommits += 1;
    }
  }

  const pathFilters = Array.isArray(options.paths)
    ? options.paths.map((value) => value.trim()).filter((value) => value.length > 0)
    : undefined;

  const diffPattern = typeof options.diffPattern === 'string' ? options.diffPattern.trim() : undefined;

  return {
    repositoryRoot: repoRoot,
    branch: options.branch ?? 'HEAD',
    limit: options.limit ?? 20,
    since: options.since,
    includeMerges: options.includeMerges !== false,
    includeFileStats: options.includeFileStats !== false,
    includeDiffs: options.includeDiffs === true,
    paths: pathFilters,
    diffPattern,
    totalCommits: entries.length,
    mergeCommits,
    totalInsertions,
    totalDeletions,
    entries
  };
}
