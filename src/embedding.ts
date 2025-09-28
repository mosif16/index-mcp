import {
  pipeline,
  env,
  type FeatureExtractionPipeline,
  type PipelineType
} from '@xenova/transformers';

env.allowRemoteModels = true;

const DEFAULT_MODEL = 'Xenova/all-MiniLM-L6-v2';

type EmbeddingPipeline = FeatureExtractionPipeline;

type TensorData = Float32Array | ArrayLike<number>;

type TensorLike = {
  data: TensorData;
  dims?: number[];
};

type PipelineFactory = (task: PipelineType, model: string) => Promise<EmbeddingPipeline>;

const pipelineCache = new Map<string, Promise<EmbeddingPipeline>>();

const defaultPipelineFactory: PipelineFactory = (task, model) =>
  pipeline(task, model) as Promise<EmbeddingPipeline>;

let currentPipelineFactory: PipelineFactory = defaultPipelineFactory;

async function getEmbeddingPipeline(model = DEFAULT_MODEL): Promise<EmbeddingPipeline> {
  if (!pipelineCache.has(model)) {
    const pipelinePromise = currentPipelineFactory('feature-extraction', model).catch((error) => {
      pipelineCache.delete(model);
      throw error;
    });
    pipelineCache.set(model, pipelinePromise);
  }
  return pipelineCache.get(model)!;
}

function toFloat32Array(data: TensorData): Float32Array {
  if (data instanceof Float32Array) {
    return data;
  }
  return new Float32Array(data);
}

function normalizeToFloat32Arrays(output: unknown): Float32Array[] {
  if (output instanceof Float32Array) {
    return [output];
  }

  if (ArrayBuffer.isView(output)) {
    const view = output as ArrayBufferView;
    const floatView = new Float32Array(
      view.buffer,
      view.byteOffset,
      view.byteLength / Float32Array.BYTES_PER_ELEMENT
    );
    return [floatView.slice()];
  }

  if (Array.isArray(output)) {
    if (output.length === 0) {
      return [];
    }

    if (typeof output[0] === 'number') {
      return [new Float32Array(output as number[])];
    }

    return output.flatMap((item) => normalizeToFloat32Arrays(item));
  }

  if (output && typeof output === 'object' && (output as TensorLike).data) {
    const tensor = output as TensorLike;
    const baseArray = toFloat32Array(tensor.data);
    const dims = Array.isArray(tensor.dims) ? tensor.dims : [];

    if (dims.length > 1 && dims[0] && dims[0] > 1) {
      const vectorLength = Math.floor(baseArray.length / dims[0]);
      const vectors: Float32Array[] = [];
      for (let i = 0; i < dims[0]; i += 1) {
        const start = i * vectorLength;
        const end = start + vectorLength;
        vectors.push(baseArray.slice(start, end));
      }
      return vectors;
    }

    return [baseArray];
  }

  throw new Error('Unexpected tensor output from embedding pipeline');
}

export interface EmbedConfig {
  model?: string;
}

export async function embedTexts(texts: string[], config: EmbedConfig = {}): Promise<Float32Array[]> {
  if (!texts.length) {
    return [];
  }
  const model = config.model ?? DEFAULT_MODEL;
  const embeddingPipeline = await getEmbeddingPipeline(model);
  const output = await embeddingPipeline(texts.length === 1 ? texts[0] : texts, {
    pooling: 'mean',
    normalize: true
  });

  return normalizeToFloat32Arrays(output);
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

function clearPipelineCache(): void {
  pipelineCache.clear();
}

function resetPipelineFactory(): void {
  currentPipelineFactory = defaultPipelineFactory;
  clearPipelineCache();
}

export function clearEmbeddingPipelineCache(): void {
  clearPipelineCache();
}

function setPipelineFactory(factory: PipelineFactory): void {
  currentPipelineFactory = factory;
  clearPipelineCache();
}

export const __testing = {
  clearPipelineCache,
  resetPipelineFactory,
  setPipelineFactory,
  getEmbeddingPipeline
};
