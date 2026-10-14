import { svelte } from '@sveltejs/vite-plugin-svelte';
import { defineConfig } from 'vitest/config';
import type { ClientRequest } from 'node:http';
import type { ProxyOptions } from 'vite';

const nodeUrl = process.env.LADEX_NODE ?? 'http://localhost:8081';

// The node refuses cross-origin writes and unknown Host headers, so the dev server presents itself as the node.
function proxyToNode(extra: ProxyOptions = {}): ProxyOptions {
  return {
    target: nodeUrl,
    changeOrigin: true,
    configure: (proxy) => {
      proxy.on('proxyReq', (request: ClientRequest) => request.setHeader('origin', nodeUrl));
      proxy.on('proxyReqWs', (request: ClientRequest) => request.setHeader('origin', nodeUrl));
    },
    ...extra,
  };
}

export default defineConfig({
  plugins: [svelte()],
  build: {
    rollupOptions: { input: { index: 'index.html', join: 'join.html' } },
  },
  server: {
    proxy: {
      '/auth': proxyToNode(),
      '/auth-status': proxyToNode(),
      '/logout': proxyToNode(),
      '/api': proxyToNode(),
      '/blobs': proxyToNode(),
      '/uploads': proxyToNode(),
      '/ws': proxyToNode({ ws: true }),
      // GET /join is the page itself; only the POST that redeems an invitation goes to the node.
      '/join': proxyToNode({ bypass: (request) => (request.method === 'GET' ? '/join.html' : undefined) }),
    },
  },
  test: {
    environment: 'jsdom',
    include: ['src/**/*.test.ts'],
  },
});
