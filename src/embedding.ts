import { ensureModelCacheDirectory } from './environment.js';
import { loadNativeModule } from './native/index.js';

const DEFAULT_MODEL = 'Xenova/bge-small-en-v1.5';

type NativeEmbeddingFn = (request: {
  texts: string[];
  model?: string;
  batchSize?: number;
}) => Promise<unknown>;

type NativeClearFn = () => unknown;

export interface EmbedConfig {
  model?: string;
  batchSize?: number;
}

type EmbeddingProvider = (texts: string[], config: EmbedConfig) => Promise<Float32Array[]>;

let cachedProvider: EmbeddingProvider | null = null;
let overrideProvider: EmbeddingProvider | null = null;

ensureModelCacheDirectory();

function normalizeVectors(vectors: unknown, expected: number): Float32Array[] {
  if (!Array.isArray(vectors)) {
    throw new Error('[index-mcp] Native embedding response is not an array of vectors');
  }

  if (vectors.length !== expected) {
    throw new Error(
      `[index-mcp] Native embedding count mismatch: expected ${expected}, received ${vectors.length}`
    );
  }

  return vectors.map((vector, index) => {
    if (vector instanceof Float32Array) {
      return vector;
    }

    if (ArrayBuffer.isView(vector)) {
      const view = vector as ArrayBufferView;
      if (view.byteLength % Float32Array.BYTES_PER_ELEMENT !== 0) {
        throw new Error(
          `[index-mcp] Native embedding vector at index ${index} has an incompatible typed array length`
        );
      }
      return new Float32Array(
        view.buffer.slice(view.byteOffset, view.byteOffset + view.byteLength)
      );
    }

    if (Array.isArray(vector)) {
      return Float32Array.from(vector);
    }

    throw new Error(`[index-mcp] Native embedding vector at index ${index} is not a supported type`);
  });
}

async function createNativeProvider(): Promise<EmbeddingProvider> {
  const nativeModule = await loadNativeModule();
  const generateEmbeddings = nativeModule.generateEmbeddings as NativeEmbeddingFn | undefined;

  if (typeof generateEmbeddings !== 'function') {
    throw new Error(
      "[index-mcp] Native bindings did not expose generateEmbeddings(); rebuild the Rust addon."
    );
  }

  return async (texts, config) => {
    const request = {
      texts,
      model: config.model,
      batchSize: config.batchSize
    };

    const result = await generateEmbeddings(request);
    return normalizeVectors(result, texts.length);
  };
}

async function getEmbeddingProvider(): Promise<EmbeddingProvider> {
  if (overrideProvider) {
    return overrideProvider;
  }
  if (cachedProvider) {
    return cachedProvider;
  }

  const provider = await createNativeProvider();
  cachedProvider = provider;
  return provider;
}

export async function embedTexts(texts: string[], config: EmbedConfig = {}): Promise<Float32Array[]> {
  if (!texts.length) {
    return [];
  }

  const provider = await getEmbeddingProvider();
  const normalizedConfig: EmbedConfig = {
    ...config,
    model: config.model ?? DEFAULT_MODEL
  };

  return provider(texts, normalizedConfig);
}

export function float32ArrayToBuffer(array: Float32Array): Buffer {
  const view = new Uint8Array(array.buffer, array.byteOffset, array.byteLength);
  return Buffer.from(view);
}

export function bufferToFloat32Array(buffer: Buffer): Float32Array {
  const arrayBuffer = buffer.buffer.slice(
    buffer.byteOffset,
    buffer.byteOffset + buffer.byteLength
  );
  return new Float32Array(arrayBuffer);
}

export function getDefaultEmbeddingModel(): string {
  return DEFAULT_MODEL;
}

export function clearEmbeddingPipelineCache(): void {
  cachedProvider = null;
  overrideProvider = null;

  void loadNativeModule()
    .then((nativeModule) => {
      const clear = nativeModule.clearEmbeddingCache as NativeClearFn | undefined;
      if (typeof clear === 'function') {
        try {
          clear();
        } catch {
          // Ignore errors during best-effort native cache clear
        }
      }
    })
    .catch(() => undefined);
}

export const __testing = {
  setEmbeddingProvider(provider: EmbeddingProvider | null): void {
    overrideProvider = provider;
  },
  reset(): void {
    cachedProvider = null;
    overrideProvider = null;
  },
  getCachedProvider(): EmbeddingProvider | null {
    return cachedProvider;
  },
  getOverrideProvider(): EmbeddingProvider | null {
    return overrideProvider;
  },
  createNativeProvider
};
