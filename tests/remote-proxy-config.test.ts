import assert from 'node:assert/strict';
import type { CallToolResult, ListToolsResult } from '@modelcontextprotocol/sdk/types.js';

import {
  registerRemoteServers,
  type RemoteProxyHandle,
  parseRemoteServerConfigs,
  resolveAuthHeaders
} from '../src/remote-proxy.js';
import { runCleanup } from '../src/cleanup.js';

async function run() {
  const originalEnv: Record<string, string | undefined> = {
    TEST_REMOTE_TOKEN: process.env.TEST_REMOTE_TOKEN,
    TEST_REMOTE_HEADER: process.env.TEST_REMOTE_HEADER,
    REMOTE_DIRECT_TOKEN: process.env.REMOTE_DIRECT_TOKEN
  };

  try {
    const emptyConfigs = parseRemoteServerConfigs(undefined);
    assert.deepEqual(emptyConfigs, []);

    const simpleConfigs = parseRemoteServerConfigs('[{"name":"alpha","url":"https://example.test"}]');
    assert.equal(simpleConfigs.length, 1);
    assert.equal(simpleConfigs[0].namespace, 'alpha');

    const namespaced = parseRemoteServerConfigs([
      { name: 'beta', namespace: 'remote-beta', url: 'https://remote.beta' }
    ]);
    assert.equal(namespaced[0].namespace, 'remote-beta');

    process.env.TEST_REMOTE_TOKEN = 'abc123';
    const bearerConfig = parseRemoteServerConfigs([
      {
        name: 'secure',
        url: 'https://secure.example',
        auth: { type: 'bearer', tokenEnv: 'TEST_REMOTE_TOKEN' }
      }
    ])[0];
    const bearerHeaders = resolveAuthHeaders(bearerConfig);
    assert.equal(bearerHeaders.authorization, 'Bearer abc123');

    process.env.TEST_REMOTE_HEADER = 'custom-value';
    const headerConfig = parseRemoteServerConfigs([
      {
        name: 'header',
        url: 'https://header.example',
        auth: { type: 'header', header: 'x-custom', valueEnv: 'TEST_REMOTE_HEADER' }
      }
    ])[0];
    const headerHeaders = resolveAuthHeaders(headerConfig);
    assert.equal(headerHeaders['x-custom'], 'custom-value');

    const directConfig = parseRemoteServerConfigs([
      {
        name: 'direct',
        url: 'https://direct.example',
        auth: { type: 'bearer', token: 'Bearer direct-token' }
      }
    ])[0];
    const directHeaders = resolveAuthHeaders(directConfig);
    assert.equal(directHeaders.authorization, 'Bearer direct-token');

    assert.throws(() => {
      parseRemoteServerConfigs([
        { name: 'missing', url: 'https://missing.example', auth: { type: 'bearer', tokenEnv: 'MISSING_ENV' } }
      ]);
    }, /requires token or tokenEnv/);

    const originalRemoteServersEnv = process.env.INDEX_MCP_REMOTE_SERVERS;
    const registeredTools: string[] = [];
    let registrationResolve: (() => void) | undefined;
    let updateResolve: (() => void) | undefined;

    try {
      process.env.INDEX_MCP_REMOTE_SERVERS = JSON.stringify([
        { name: 'stub', namespace: 'remote-stub', url: 'https://stub.example' }
      ]);

      class StubProxy implements RemoteProxyHandle {
        namespace = 'remote-stub';
        remoteUrl = 'https://stub.example';
        tools: ListToolsResult['tools'] = [];
        #closed = false;
        connectAttempts = 0;
        #listeners = new Set<(tools: ListToolsResult['tools']) => void>();

        async ensureConnected(): Promise<void> {
          this.connectAttempts += 1;
          if (this.connectAttempts === 1) {
            throw new Error('upstream unavailable');
          }
          if (this.connectAttempts === 2) {
            this.tools = [
              {
                name: 'ping',
                description: 'Ping the remote server',
                inputSchema: { type: 'object', properties: {} }
              }
            ] as ListToolsResult['tools'];
          } else {
            this.tools = [
              {
                name: 'ping',
                description: 'Ping the remote server',
                inputSchema: { type: 'object', properties: {} }
              },
              {
                name: 'pong',
                description: 'Pong from remote server',
                inputSchema: { type: 'object', properties: {} }
              }
            ] as ListToolsResult['tools'];
          }
          this.#emit();
        }

        async callTool(): Promise<CallToolResult> {
          return { content: [] };
        }

        async close(): Promise<void> {
          this.#closed = true;
        }

        isClosed(): boolean {
          return this.#closed;
        }

        onToolsChanged(listener: (tools: ListToolsResult['tools']) => void): () => void {
          this.#listeners.add(listener);
          return () => {
            this.#listeners.delete(listener);
          };
        }

        #emit() {
          for (const listener of this.#listeners) {
            listener(this.tools);
          }
        }
      }

      const proxy = new StubProxy();
      const delayCalls: number[] = [];
      const registrationPromise = new Promise<void>((resolve) => {
        registrationResolve = resolve;
      });
      const updatePromise = new Promise<void>((resolve) => {
        updateResolve = resolve;
      });

      const fakeServer = {
        registerTool(name: string) {
          registeredTools.push(name);
          if (name === 'remote-stub.ping') {
            registrationResolve?.();
          }
          if (name === 'remote-stub.pong') {
            updateResolve?.();
          }
        }
      } as unknown as Parameters<typeof registerRemoteServers>[0];

      await registerRemoteServers(fakeServer, {
        createProxy: () => proxy,
        delayFn: async (ms) => {
          delayCalls.push(ms);
        }
      });

      await registrationPromise;
      await updatePromise;
      assert(proxy.connectAttempts >= 3);
      assert.deepEqual(registeredTools, ['remote-stub.ping', 'remote-stub.pong']);
      assert(delayCalls.length >= 2);
    } finally {
      if (originalRemoteServersEnv === undefined) {
        delete process.env.INDEX_MCP_REMOTE_SERVERS;
      } else {
        process.env.INDEX_MCP_REMOTE_SERVERS = originalRemoteServersEnv;
      }

      await runCleanup();
    }
  } finally {
    for (const [key, value] of Object.entries(originalEnv)) {
      if (typeof value === 'undefined') {
        delete process.env[key];
      } else {
        process.env[key] = value;
      }
    }
  }
}

run().catch((error) => {
  console.error(error);
  process.exitCode = 1;
});
