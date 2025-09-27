import type { NativeModule } from '../types/native.js';

let nativeModulePromise: Promise<NativeModule> | null = null;

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

export async function loadNativeModule(): Promise<NativeModule> {
  if (!nativeModulePromise) {
    nativeModulePromise = importNativeModule();
  }

  return nativeModulePromise;
}

export function resetNativeModuleCache(): void {
  nativeModulePromise = null;
}
