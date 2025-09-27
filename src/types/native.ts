export interface NativeScanOptions {
  root: string;
  include: string[];
  exclude: string[];
  maxFileSizeBytes?: number;
  needsContent: boolean;
}

export interface NativeFileEntry {
  path: string;
  size: number;
  modified: number;
  hash: string;
  content: string | null;
  isBinary: boolean;
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

export interface NativeModule {
  scanRepo(options: NativeScanOptions): Promise<NativeScanResult>;
}
