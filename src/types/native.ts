export interface NativeScanOptions {
  root: string;
  include: string[];
  exclude: string[];
  maxFileSizeBytes?: number;
  needsContent: boolean;
}

export interface NativeMetadataOptions {
  root: string;
  include: string[];
  exclude: string[];
  maxFileSizeBytes?: number;
}

export interface NativeFileEntry {
  path: string;
  size: number;
  modified: number;
  hash: string;
  content: string | null | undefined;
  isBinary: boolean;
}

export interface NativeMetadataEntry {
  path: string;
  size: number;
  modified: number;
}

export type NativeSkippedReason = 'file-too-large' | 'read-error';

export interface NativeSkippedFile {
  path: string;
  reason: NativeSkippedReason;
  size?: number;
  message?: string;
}

export interface NativeScanResult {
  files: NativeFileEntry[];
  skipped: NativeSkippedFile[];
}

export interface NativeMetadataResult {
  entries: NativeMetadataEntry[];
  skipped: NativeSkippedFile[];
}

export interface NativeReadOptions {
  root: string;
  paths: string[];
  maxFileSizeBytes?: number;
  needsContent: boolean;
}

export interface NativeReadResult {
  files: NativeFileEntry[];
  skipped: NativeSkippedFile[];
}

export interface NativeChunkFragment {
  content: string;
  byteStart: number;
  byteEnd: number;
  lineStart: number;
  lineEnd: number;
}

export interface NativeAnalyzeOptions {
  path: string;
  content: string;
  chunkSizeTokens?: number;
  chunkOverlapTokens?: number;
}

export interface NativeAnalysisResult {
  chunks: NativeChunkFragment[];
}

export interface NativeModule {
  scanRepo(options: NativeScanOptions): Promise<NativeScanResult>;
  scanRepoMetadata?(options: NativeMetadataOptions): Promise<NativeMetadataResult>;
  readRepoFiles?(options: NativeReadOptions): Promise<NativeReadResult>;
  analyzeFileContent?(options: NativeAnalyzeOptions): Promise<NativeAnalysisResult>;
}
