import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { SSEClientTransport } from '@modelcontextprotocol/sdk/client/sse.js';
import type { CallToolResult, ListToolsResult, ServerNotification, ServerRequest } from '@modelcontextprotocol/sdk/types.js';
import type { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js';
import type { RequestHandlerExtra, RequestOptions } from '@modelcontextprotocol/sdk/shared/protocol.js';
import { z } from 'zod';

import { createLogger } from './logger.js';
import { registerCleanupTask } from './cleanup.js';
import { getPackageMetadata } from './package-metadata.js';

const REMOTE_CONFIG_ENV = 'INDEX_MCP_REMOTE_SERVERS';

const rawAuthConfigSchema = z.discriminatedUnion('type', [
  z.object({
    type: z.literal('bearer'),
    token: z.string().min(1).optional(),
    tokenEnv: z.string().min(1).optional(),
    header: z.string().optional()
  }),
  z.object({
    type: z.literal('header'),
    header: z.string().min(1),
    value: z.string().optional(),
    valueEnv: z.string().optional()
  })
]);

const authConfigSchema = rawAuthConfigSchema.superRefine((value, ctx) => {
  if (value.type === 'bearer') {
    if (!value.token && !(value.tokenEnv && process.env[value.tokenEnv])) {
      ctx.addIssue({
        code: z.ZodIssueCode.custom,
        message: 'Bearer auth requires token or tokenEnv with a defined environment variable'
      });
    }
    return;
  }

  if (!value.value && !(value.valueEnv && process.env[value.valueEnv])) {
    ctx.addIssue({
      code: z.ZodIssueCode.custom,
      message: 'Header auth requires value or valueEnv with a defined environment variable'
    });
  }
});

const retryConfigSchema = z
  .object({
    maxAttempts: z.number().int().min(1).optional(),
    initialDelayMs: z.number().int().min(0).optional(),
    maxDelayMs: z.number().int().min(1).optional(),
    backoffMultiplier: z.number().positive().optional()
  })
  .optional();

const remoteServerConfigSchema = z.object({
  name: z.string().min(1),
  url: z.string().url(),
  namespace: z.string().min(1).optional(),
  headers: z.record(z.string(), z.string()).optional(),
  auth: authConfigSchema.optional(),
  retry: retryConfigSchema
});

export type RemoteServerConfig = z.infer<typeof remoteServerConfigSchema>;

const defaultRetryConfig = {
  maxAttempts: 5,
  initialDelayMs: 500,
  maxDelayMs: 30_000,
  backoffMultiplier: 2
};

type RetryState = {
  maxAttempts: number;
  initialDelayMs: number;
  maxDelayMs: number;
  backoffMultiplier: number;
};

type ToolCallbackExtra = RequestHandlerExtra<ServerRequest, ServerNotification>;
type ToolUpdateListener = (tools: ListToolsResult['tools']) => void;

type DelayFn = (ms: number) => Promise<void>;

export function parseRemoteServerConfigs(raw: unknown): RemoteServerConfig[] {
  if (!raw) {
    return [];
  }

  let value: unknown = raw;
  if (typeof raw === 'string') {
    const trimmed = raw.trim();
    if (!trimmed) {
      return [];
    }
    try {
      value = JSON.parse(trimmed);
    } catch (error) {
      throw new Error(`Failed to parse ${REMOTE_CONFIG_ENV}: ${(error as Error).message}`);
    }
  }

  const parsed = z.array(remoteServerConfigSchema).safeParse(value);
  if (!parsed.success) {
    throw new Error(parsed.error.errors.map((issue) => issue.message).join('; '));
  }

  return parsed.data.map((config) => ({
    ...config,
    namespace: config.namespace ?? config.name
  }));
}

export function loadRemoteServerConfigs(): RemoteServerConfig[] {
  return parseRemoteServerConfigs(process.env[REMOTE_CONFIG_ENV]);
}

function delay(ms: number) {
  return new Promise<void>((resolve) => {
    setTimeout(resolve, ms);
  });
}

const { name: packageName, version: packageVersion } = getPackageMetadata();

function normalizeHeaderMap(headers: Record<string, string | undefined>): Record<string, string> {
  const normalized: Record<string, string> = {};
  for (const [key, value] of Object.entries(headers)) {
    if (typeof value === 'string' && value.trim()) {
      normalized[key.toLowerCase()] = value;
    }
  }
  return normalized;
}

export function resolveAuthHeaders(config: RemoteServerConfig): Record<string, string> {
  if (!config.auth) {
    return {};
  }

  if (config.auth.type === 'bearer') {
    const token = config.auth.token ?? (config.auth.tokenEnv ? process.env[config.auth.tokenEnv] : undefined);
    if (!token) {
      throw new Error(`Bearer auth for remote MCP server ${config.name} requires a token`);
    }
    const headerName = config.auth.header ?? 'authorization';
    const headerValue = token.startsWith('Bearer ') ? token : `Bearer ${token}`;
    return { [headerName.toLowerCase()]: headerValue };
  }

  const value = config.auth.value ?? (config.auth.valueEnv ? process.env[config.auth.valueEnv] : undefined);
  if (!value) {
    throw new Error(`Header auth for remote MCP server ${config.name} requires a value`);
  }

  return { [config.auth.header.toLowerCase()]: value };
}

function headersToHeadersInit(headers: Record<string, string>): Record<string, string> {
  const init: Record<string, string> = {};
  for (const [key, value] of Object.entries(headers)) {
    init[key] = value;
  }
  return init;
}

function createFetchWithHeaders(headers: Record<string, string>) {
  return async (
    input: Parameters<typeof fetch>[0],
    init?: Parameters<typeof fetch>[1]
  ): Promise<Response> => {
    const mergedHeaders = new Headers();

    for (const [key, value] of Object.entries(headers)) {
      mergedHeaders.set(key, value);
    }

    if (init?.headers) {
      const provided = new Headers(init.headers);
      provided.forEach((value, key) => {
        mergedHeaders.set(key, value);
      });
    }

    return fetch(input, {
      ...init,
      headers: mergedHeaders
    });
  };
}

function resolveRetryConfig(config: RemoteServerConfig): RetryState {
  return {
    maxAttempts: config.retry?.maxAttempts ?? defaultRetryConfig.maxAttempts,
    initialDelayMs: config.retry?.initialDelayMs ?? defaultRetryConfig.initialDelayMs,
    maxDelayMs: config.retry?.maxDelayMs ?? defaultRetryConfig.maxDelayMs,
    backoffMultiplier: config.retry?.backoffMultiplier ?? defaultRetryConfig.backoffMultiplier
  };
}

class RemoteServerProxy implements RemoteProxyHandle {
  readonly #config: RemoteServerConfig;
  readonly #logger;
  #client: Client | null = null;
  #transport: SSEClientTransport | null = null;
  #connectPromise: Promise<void> | null = null;
  #closed = false;
  #tools: ListToolsResult['tools'] = [];
  #toolListeners: Set<ToolUpdateListener> = new Set();

  constructor(config: RemoteServerConfig) {
    this.#config = config;
    this.#logger = createLogger(`remote:${config.name}`);
  }

  get namespace(): string {
    return this.#config.namespace ?? this.#config.name;
  }

  get remoteUrl(): string {
    return this.#config.url;
  }

  get tools(): ListToolsResult['tools'] {
    return this.#tools;
  }

  isClosed(): boolean {
    return this.#closed;
  }

  onToolsChanged(listener: ToolUpdateListener): () => void {
    this.#toolListeners.add(listener);
    return () => {
      this.#toolListeners.delete(listener);
    };
  }

  async ensureConnected(): Promise<void> {
    if (this.#closed) {
      throw new Error(`Remote MCP proxy ${this.#config.name} has been closed`);
    }

    if (this.#client && this.#transport) {
      return;
    }

    if (!this.#connectPromise) {
      const pending = this.#connectWithRetry();
      this.#connectPromise = pending
        .then(() => {
          this.#connectPromise = null;
        })
        .catch((error) => {
          this.#connectPromise = null;
          throw error;
        });
    }

    await this.#connectPromise;
  }

  async callTool(toolName: string, args: Record<string, unknown>, extra: ToolCallbackExtra): Promise<CallToolResult> {
    await this.ensureConnected();

    if (!this.#client) {
      throw new Error(`Remote MCP proxy ${this.#config.name} is not connected`);
    }

    const progressToken =
      (extra._meta && typeof extra._meta === 'object' && 'progressToken' in extra._meta
        ? (extra._meta.progressToken as string | number | undefined)
        : undefined) ?? extra.requestId;

    const requestOptions: RequestOptions = {
      signal: extra.signal,
      onprogress: async (progress) => {
        await extra.sendNotification({
          method: 'notifications/progress',
          params: {
            ...progress,
            progressToken
          }
        });
      }
    };

    try {
      const result = await this.#client.callTool(
        {
          name: toolName,
          arguments: args,
          _meta: extra._meta
        },
        undefined,
        requestOptions
      );

      if ('content' in result && Array.isArray(result.content)) {
        return result as CallToolResult;
      }

      return {
        ...result,
        content: []
      } as CallToolResult;
    } catch (error) {
      this.#logger.warn({ err: error }, '[remote] tools/call failed; resetting connection');
      await this.#resetConnection();
      throw error;
    }
  }

  async close(): Promise<void> {
    this.#closed = true;
    await this.#resetConnection();
  }

  async #connectWithRetry(): Promise<void> {
    const retry = resolveRetryConfig(this.#config);
    let attempt = 0;
    let delayMs = retry.initialDelayMs;
    let lastError: unknown;

    while (!this.#closed && (attempt < retry.maxAttempts || retry.maxAttempts === Infinity)) {
      attempt += 1;
      try {
        await this.#connectOnce();
        this.#logger.info({ url: this.#config.url }, '[remote] connected');
        return;
      } catch (error) {
        lastError = error;
        this.#logger.warn({ err: error, attempt }, '[remote] connection attempt failed');
        if (attempt >= retry.maxAttempts && retry.maxAttempts !== Infinity) {
          break;
        }
        await delay(delayMs);
        delayMs = Math.min(delayMs * retry.backoffMultiplier, retry.maxDelayMs);
      }
    }

    throw lastError instanceof Error
      ? lastError
      : new Error(`Unable to connect to remote MCP server ${this.#config.name}`);
  }

  async #connectOnce(): Promise<void> {
    const headers = normalizeHeaderMap({
      ...(this.#config.headers ?? {}),
      ...resolveAuthHeaders(this.#config)
    });
    const fetchImpl = createFetchWithHeaders(headers);
    const url = new URL(this.#config.url);

    const transport = new SSEClientTransport(url, {
      eventSourceInit: { fetch: fetchImpl },
      requestInit: { headers: headersToHeadersInit(headers) },
      fetch: fetchImpl
    });

    transport.onerror = async (error) => {
      if (this.#closed) {
        return;
      }
      this.#logger.warn({ err: error }, '[remote] transport error');
      await this.#resetConnection();
    };

    transport.onclose = async () => {
      if (this.#closed) {
        return;
      }
      this.#logger.info({}, '[remote] transport closed');
      await this.#resetConnection();
    };

    const client = new Client({
      name: `${packageName}-remote-proxy`,
      version: packageVersion
    });

    try {
      await client.connect(transport);
      const toolList = await client.listTools({});
      this.#client = client;
      this.#transport = transport;
      this.#tools = toolList.tools;
      this.#emitToolsChanged();
    } catch (error) {
      transport.onerror = undefined;
      transport.onclose = undefined;
      await transport.close().catch(() => {});
      await client.close().catch(() => {});
      throw error;
    }
  }

  async #resetConnection(): Promise<void> {
    const client = this.#client;
    const transport = this.#transport;
    this.#client = null;
    this.#transport = null;

    if (transport) {
      transport.onerror = undefined;
      transport.onclose = undefined;
      try {
        await transport.close();
      } catch {
        // ignore errors during shutdown
      }
    }

    if (client) {
      try {
        await client.close();
      } catch {
        // ignore errors during shutdown
      }
    }
  }

  #emitToolsChanged(): void {
    for (const listener of this.#toolListeners) {
      try {
        listener(this.#tools);
      } catch (error) {
        this.#logger.warn({ err: error }, '[remote] tool listener failed');
      }
    }
  }
}

export interface RemoteProxyHandle {
  readonly namespace: string;
  readonly remoteUrl: string;
  readonly tools: ListToolsResult['tools'];
  ensureConnected(): Promise<void>;
  callTool(toolName: string, args: Record<string, unknown>, extra: ToolCallbackExtra): Promise<CallToolResult>;
  close(): Promise<void>;
  isClosed(): boolean;
  onToolsChanged(listener: ToolUpdateListener): () => void;
}

export interface RemoteServerRegistrationOptions {
  createProxy?: (config: RemoteServerConfig) => RemoteProxyHandle;
  delayFn?: DelayFn;
}

const defaultDelay: DelayFn = (ms) => delay(ms);

export async function registerRemoteServers(
  server: McpServer,
  options?: RemoteServerRegistrationOptions
): Promise<void> {
  const configs = loadRemoteServerConfigs();
  if (!configs.length) {
    return;
  }

  const managerLogger = createLogger('remote');
  const delayFn = options?.delayFn ?? defaultDelay;
  const createProxy = options?.createProxy ?? ((config: RemoteServerConfig): RemoteProxyHandle => new RemoteServerProxy(config));

  const proxies = configs.map((config) => createProxy(config));
  const registeredToolNames = new Set<string>();
  const emptyToolNamespacesLogged = new Set<string>();
  const listenerDisposers: Array<() => void> = [];

  const registerToolsForProxy = (proxy: RemoteProxyHandle, tools: ListToolsResult['tools']) => {
    let newTools = 0;

    for (const tool of tools) {
      const namespacedName = `${proxy.namespace}.${tool.name}`;
      if (registeredToolNames.has(namespacedName)) {
        continue;
      }

      const annotations = tool.annotations ? structuredClone(tool.annotations) : {};
      const remoteMetadata: Record<string, unknown> = {
        server: proxy.namespace,
        url: proxy.remoteUrl
      };
      if (tool.inputSchema) {
        remoteMetadata.inputSchema = tool.inputSchema;
      }
      if (tool.outputSchema) {
        remoteMetadata.outputSchema = tool.outputSchema;
      }
      (annotations as Record<string, unknown>).remote = remoteMetadata;

      server.registerTool(
        namespacedName,
        {
          title: tool.title,
          description: tool.description
            ? `${tool.description} (proxied from ${proxy.namespace})`
            : `Remote tool ${tool.name} proxied from ${proxy.namespace}`,
          annotations
        },
        async (args: Record<string, unknown> | undefined, extra) => {
          return proxy.callTool(tool.name, args ?? {}, extra);
        }
      );

      registeredToolNames.add(namespacedName);
      newTools += 1;
    }

    if (newTools > 0) {
      managerLogger.info(
        { server: proxy.namespace, toolCount: newTools },
        'Registered remote MCP tools'
      );
      emptyToolNamespacesLogged.delete(proxy.namespace);
    } else if (!tools.length && !emptyToolNamespacesLogged.has(proxy.namespace)) {
      managerLogger.info({ server: proxy.namespace }, 'Remote MCP server connected with no tools');
      emptyToolNamespacesLogged.add(proxy.namespace);
    }
  };

  const scheduleProxyRegistration = (proxy: RemoteProxyHandle) => {
    const unsubscribe = proxy.onToolsChanged((tools) => {
      registerToolsForProxy(proxy, tools);
    });
    listenerDisposers.push(unsubscribe);

    if (proxy.tools.length > 0) {
      registerToolsForProxy(proxy, proxy.tools);
    }

    const run = async () => {
      const maxBackoffMs = 30_000;
      const stableDelayMs = 1_000;
      let attempt = 0;
      let backoffMs = 1_000;

      while (!proxy.isClosed()) {
        try {
          await proxy.ensureConnected();
          attempt = 0;
          backoffMs = 1_000;
        } catch (error) {
          attempt += 1;
          managerLogger.warn(
            { err: error, server: proxy.namespace, attempt },
            'Failed to connect to remote MCP server; retrying'
          );
          await delayFn(backoffMs);
          backoffMs = Math.min(backoffMs * 2, maxBackoffMs);
          continue;
        }

        await delayFn(stableDelayMs);
      }
    };

    run().catch((error) => {
      managerLogger.error({ err: error, server: proxy.namespace }, 'Remote proxy registration failed');
    });
  };

  for (const proxy of proxies) {
    scheduleProxyRegistration(proxy);
  }

  registerCleanupTask(async () => {
    for (const dispose of listenerDisposers.splice(0)) {
      try {
        dispose();
      } catch {
        // ignore errors during cleanup
      }
    }
    await Promise.all(proxies.map((proxy) => proxy.close().catch(() => {})));
  });
}
