import type { NativeModule } from '../types/native.js';
import { createLogger } from '../logger.js';

const log = createLogger('native');

type NativeModuleState = 'uninitialized' | 'native' | 'error';

let nativeModulePromise: Promise<NativeModule> | null = null;
let nativeModuleState: NativeModuleState = 'uninitialized';
let nativeModuleError: unknown;

function validateNativeModule(candidate: unknown): NativeModule {
  if (candidate && typeof candidate === 'object') {
    const asRecord = candidate as Record<string, unknown>;

    if (
      'scanRepo' in asRecord &&
      typeof asRecord.scanRepo === 'function' &&
      'generateEmbeddings' in asRecord &&
      typeof asRecord.generateEmbeddings === 'function'
    ) {
      return candidate as NativeModule;
    }

    if ('default' in asRecord) {
      const defaultExport = asRecord.default;
      if (
        defaultExport &&
        typeof defaultExport === 'object' &&
        'scanRepo' in (defaultExport as Record<string, unknown>) &&
        typeof (defaultExport as Record<string, unknown>).scanRepo === 'function' &&
        'generateEmbeddings' in (defaultExport as Record<string, unknown>) &&
        typeof (defaultExport as Record<string, unknown>).generateEmbeddings === 'function'
      ) {
        return defaultExport as NativeModule;
      }
    }
  }

  throw new Error(
    "@index-mcp/native did not expose the expected bindings. Rebuild the native package with 'npm run build' inside crates/index_mcp_native."
  );
}

async function importNativeModule(): Promise<NativeModule> {
  try {
    const imported = await import('@index-mcp/native');
    return validateNativeModule(imported);
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    throw new Error(
      `[index-mcp] Failed to load native bindings. Ensure the Rust addon is built (npm install && npm run build in crates/index_mcp_native). Original error: ${message}`
    );
  }
}

function normalizeError(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

export async function loadNativeModule(): Promise<NativeModule> {
  if (!nativeModulePromise) {
    nativeModulePromise = importNativeModule()
      .then((module) => {
        nativeModuleState = 'native';
        nativeModuleError = undefined;
        return module;
      })
      .catch((error) => {
        nativeModuleState = 'error';
        nativeModuleError = error;
        nativeModulePromise = null;
        log.warn({ err: error }, 'Failed to load native bindings');
        throw error;
      });
  }

  return nativeModulePromise;
}

export function getNativeModuleStatus(): { state: NativeModuleState; message?: string } {
  if (nativeModuleState === 'error' && nativeModuleError) {
    return { state: nativeModuleState, message: normalizeError(nativeModuleError) };
  }
  return { state: nativeModuleState };
}

export function resetNativeModuleCache(): void {
  nativeModulePromise = null;
  nativeModuleState = 'uninitialized';
  nativeModuleError = undefined;
}
