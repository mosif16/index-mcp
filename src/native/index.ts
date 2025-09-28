import type { NativeModule } from '../types/native.js';
import { fallbackNativeModule } from './fallback.js';
import { createLogger } from '../logger.js';

const log = createLogger('native');

type NativeModuleState = 'uninitialized' | 'native' | 'fallback';

let nativeModulePromise: Promise<NativeModule> | null = null;
let nativeModuleState: NativeModuleState = 'uninitialized';
let nativeModuleError: unknown;

function validateNativeModule(candidate: unknown): NativeModule {
  if (candidate && typeof candidate === 'object') {
    const asRecord = candidate as Record<string, unknown>;

    if (
      'scanRepo' in asRecord &&
      typeof asRecord.scanRepo === 'function'
    ) {
      return candidate as NativeModule;
    }

    if ('default' in asRecord) {
      const defaultExport = asRecord.default;
      if (
        defaultExport &&
        typeof defaultExport === 'object' &&
        'scanRepo' in (defaultExport as Record<string, unknown>) &&
        typeof (defaultExport as Record<string, unknown>).scanRepo === 'function'
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

function shouldForceFallback(): boolean {
  const value = process.env.INDEX_MCP_NATIVE_DISABLE;
  if (!value) {
    return false;
  }

  const normalized = value.toLowerCase();
  return normalized === '1' || normalized === 'true' || normalized === 'yes';
}

function normalizeError(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function logWarning(message: string, error?: unknown): void {
  if (error) {
    log.warn({ err: error, message }, 'Native module warning');
  } else {
    log.warn({ message }, 'Native module warning');
  }
}

export async function loadNativeModule(): Promise<NativeModule> {
  if (!nativeModulePromise) {
    if (shouldForceFallback()) {
      nativeModuleState = 'fallback';
      nativeModuleError = new Error('Native bindings disabled via INDEX_MCP_NATIVE_DISABLE');
      logWarning('Native bindings disabled via INDEX_MCP_NATIVE_DISABLE; using JS fallback scanner.');
      nativeModulePromise = Promise.resolve(fallbackNativeModule);
    } else {
      nativeModulePromise = importNativeModule()
        .then((module) => {
          nativeModuleState = 'native';
          nativeModuleError = undefined;
          return module;
        })
        .catch((error) => {
          nativeModuleState = 'fallback';
          nativeModuleError = error;
          logWarning('Failed to load native bindings; using JS fallback scanner.', error);
          return fallbackNativeModule;
        });
    }
  }

  return nativeModulePromise;
}

export function getNativeModuleStatus(): { state: NativeModuleState; message?: string } {
  if (nativeModuleState === 'fallback' && nativeModuleError) {
    return { state: nativeModuleState, message: normalizeError(nativeModuleError) };
  }
  return { state: nativeModuleState };
}

export function resetNativeModuleCache(): void {
  nativeModulePromise = null;
  nativeModuleState = 'uninitialized';
  nativeModuleError = undefined;
}
