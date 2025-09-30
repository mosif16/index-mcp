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

export interface RepositoryTimelineTopFile {
  path: string;
  insertions: number;
  deletions: number;
  net: number;
}

export interface RepositoryTimelineDirectoryChurn {
  path: string;
  insertions: number;
  deletions: number;
  net: number;
  filesChanged: number;
}

export interface RepositoryTimelineDiffSummary {
  filesChanged: number;
  insertions: number;
  deletions: number;
  net: number;
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
  topFiles: RepositoryTimelineTopFile[];
  directoryChurn: RepositoryTimelineDirectoryChurn[];
  diffSummary: RepositoryTimelineDiffSummary;
  highlights: string[];
  pullRequestUrl: string | null;
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
  remoteUrl: string | null;
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

function toTopFiles(fileChanges: RepositoryTimelineFileChange[], limit: number): RepositoryTimelineTopFile[] {
  const computeMagnitude = (item: RepositoryTimelineTopFile): number =>
    Math.abs(item.insertions) + Math.abs(item.deletions);

  return fileChanges
    .map((change) => {
      const insertions = change.insertions ?? 0;
      const deletions = change.deletions ?? 0;
      return {
        path: change.path,
        insertions,
        deletions,
        net: insertions - deletions
      } satisfies RepositoryTimelineTopFile;
    })
    .sort((a, b) => computeMagnitude(b) - computeMagnitude(a) || a.path.localeCompare(b.path))
    .slice(0, limit);
}

function aggregateDirectoryChurn(
  fileChanges: RepositoryTimelineFileChange[],
  limit: number
): RepositoryTimelineDirectoryChurn[] {
  const map = new Map<string, { insertions: number; deletions: number; files: Set<string> }>();
  for (const change of fileChanges) {
    const dir = change.path.includes('/') ? change.path.slice(0, change.path.lastIndexOf('/')) : '.';
    const entry = map.get(dir) ?? {
      insertions: 0,
      deletions: 0,
      files: new Set<string>()
    };
    entry.insertions += change.insertions ?? 0;
    entry.deletions += change.deletions ?? 0;
    entry.files.add(change.path);
    map.set(dir, entry);
  }

  const computeMagnitude = (item: RepositoryTimelineDirectoryChurn): number =>
    Math.abs(item.insertions) + Math.abs(item.deletions);

  return Array.from(map.entries())
    .map(([path, value]) => ({
      path,
      insertions: value.insertions,
      deletions: value.deletions,
      net: value.insertions - value.deletions,
      filesChanged: value.files.size
    }) satisfies RepositoryTimelineDirectoryChurn)
    .sort((a, b) => computeMagnitude(b) - computeMagnitude(a) || a.path.localeCompare(b.path))
    .slice(0, limit);
}

function buildHighlights(entry: RepositoryTimelineEntry): string[] {
  const highlights: string[] = [];
  if (entry.pullRequestNumber && entry.pullRequestUrl) {
    highlights.push(`PR #${entry.pullRequestNumber} · ${entry.pullRequestUrl}`);
  } else if (entry.pullRequestNumber) {
    highlights.push(`PR #${entry.pullRequestNumber}`);
  }

  if (entry.isMerge) {
    highlights.push('Merge commit');
  }

  const summary = entry.diffSummary;
  highlights.push(
    `Diff +${summary.insertions}/-${summary.deletions} across ${summary.filesChanged} file${summary.filesChanged === 1 ? '' : 's'}`
  );

  for (const file of entry.topFiles) {
    highlights.push(`${file.path}: +${file.insertions}/-${file.deletions}`);
  }

  if (entry.directoryChurn.length > 0) {
    const dir = entry.directoryChurn[0];
    highlights.push(
      `${dir.path} hotspot: +${dir.insertions}/-${dir.deletions} (${dir.filesChanged} file${dir.filesChanged === 1 ? '' : 's'})`
    );
  }

  return highlights;
}

function normalizeRemoteUrl(raw: string | null): string | null {
  if (!raw) {
    return null;
  }
  const trimmed = raw.trim();
  if (!trimmed) {
    return null;
  }

  if (trimmed.startsWith('http://') || trimmed.startsWith('https://')) {
    try {
      const url = new URL(trimmed);
      url.pathname = url.pathname.replace(/\.git$/, '');
      url.search = '';
      url.hash = '';
      url.username = '';
      url.password = '';
      return `${url.origin}${url.pathname}`;
    } catch {
      return null;
    }
  }

  const sshMatch = /^git@([^:]+):(.+)$/.exec(trimmed);
  if (sshMatch) {
    const host = sshMatch[1];
    const pathPart = sshMatch[2].replace(/\.git$/, '');
    return `https://${host}/${pathPart}`;
  }

  const sshUrlMatch = /^ssh:\/\/([^@]+)@([^/]+)\/(.+)$/.exec(trimmed);
  if (sshUrlMatch) {
    const host = sshUrlMatch[2];
    const pathPart = sshUrlMatch[3].replace(/\.git$/, '');
    return `https://${host}/${pathPart}`;
  }

  return null;
}

function buildPullRequestUrl(remoteUrl: string | null, pullRequestNumber: number | null): string | null {
  if (!remoteUrl || pullRequestNumber === null) {
    return null;
  }
  return `${remoteUrl.replace(/\/$/, '')}/pull/${pullRequestNumber}`;
}

function parseGitLog(
  output: string,
  includeFileStats: boolean,
  includeDiffs: boolean,
  remoteUrl: string | null
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

    const topFiles = includeFileStats ? toTopFiles(fileChanges, 3) : [];
    const directoryChurn = includeFileStats ? aggregateDirectoryChurn(fileChanges, 5) : [];
    const diffSummary: RepositoryTimelineDiffSummary = {
      filesChanged: fileChanges.length,
      insertions,
      deletions,
      net: insertions - deletions
    };
    const pullRequestNumber = parsePullRequestNumber(subject);

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
      pullRequestNumber,
      filesChanged: includeFileStats ? fileChanges.length : 0,
      insertions,
      deletions,
      fileChanges,
      ...(includeDiffs ? { diff: diff ?? null } : {}),
      topFiles,
      directoryChurn,
      diffSummary,
      highlights: [],
      pullRequestUrl: buildPullRequestUrl(remoteUrl, pullRequestNumber)
    };

    entry.highlights = buildHighlights(entry);

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

  const remoteUrl = normalizeRemoteUrl(await resolveRemoteUrl(repoRoot));
  const logOutput = await runGitLog(options, repoRoot);
  const entries = parseGitLog(
    logOutput,
    options.includeFileStats !== false,
    options.includeDiffs === true,
    remoteUrl
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
    entries,
    remoteUrl
  };
}

async function resolveRemoteUrl(repoRoot: string): Promise<string | null> {
  try {
    const { stdout } = await execFileAsync('git', ['config', '--get', 'remote.origin.url'], {
      cwd: repoRoot,
      maxBuffer: 1024 * 1024,
      encoding: 'utf8'
    });
    return stdout.trim() ? stdout.trim() : null;
  } catch {
    return null;
  }
}
