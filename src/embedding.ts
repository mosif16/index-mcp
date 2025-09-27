import { pipeline, env, type FeatureExtractionPipeline } from '@xenova/transformers';

env.allowRemoteModels = true;

const DEFAULT_MODEL = 'Xenova/bge-small-en-v1.5';

type EmbeddingPipeline = FeatureExtractionPipeline;

type TensorLike = {
  data: Float32Array;
};

const pipelineCache = new Map<string, Promise<EmbeddingPipeline>>();

async function getEmbeddingPipeline(model = DEFAULT_MODEL): Promise<EmbeddingPipeline> {
  if (!pipelineCache.has(model)) {
    pipelineCache.set(
      model,
      pipeline('feature-extraction', model) as Promise<EmbeddingPipeline>
    );
  }
  return pipelineCache.get(model)!;
}

function tensorToFloat32Array(tensor: unknown): Float32Array {
  if (!tensor || typeof tensor !== 'object' || !(tensor as TensorLike).data) {
    throw new Error('Unexpected tensor output from embedding pipeline');
  }
  const data = (tensor as TensorLike).data;
  if (!(data instanceof Float32Array)) {
    return new Float32Array(data as Float32Array);
  }
  return data;
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

  if (Array.isArray(output)) {
    return output.map((tensor) => tensorToFloat32Array(tensor));
  }

  return [tensorToFloat32Array(output)];
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
