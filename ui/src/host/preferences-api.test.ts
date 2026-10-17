import { describe, expect, it } from 'vitest';
import type { NodeTransport, RequestOptions } from '../client/transport';
import { TransferError } from '../client/types';
import { PreferencesApi } from './preferences-api';

class FakeTransport {
  requests: Array<{ method: string; path: string; body?: string }> = [];
  constructor(private readonly reply: { status: number; text: string }) {}

  async request(method: string, path: string, options: RequestOptions = {}) {
    this.requests.push({ method, path, body: options.body });
    return this.reply;
  }
}

function apiReplying(status: number, body: unknown): { api: PreferencesApi; transport: FakeTransport } {
  const transport = new FakeTransport({ status, text: JSON.stringify(body) });
  return { api: new PreferencesApi(transport as unknown as NodeTransport), transport };
}

describe('PreferencesApi', () => {
  it('sends_only_the_fields_that_change', async () => {
    const { api, transport } = apiReplying(200, {});
    await api.update({ storage_limit_gib: 40 });
    expect(transport.requests).toEqual([{ method: 'PUT', path: '/api/preferences', body: '{"storage_limit_gib":40}' }]);
  });

  it('reports_why_the_node_refused_a_change', async () => {
    const { api } = apiReplying(400, { message: 'the storage limit must be between 1 and 1048576 GiB' });
    await expect(api.update({ storage_limit_gib: 0 })).rejects.toThrow(new TransferError('the storage limit must be between 1 and 1048576 GiB'));
  });

  it('lists_only_nodes_that_are_paired_and_not_revoked', async () => {
    const { api } = apiReplying(200, {
      nodes: [
        { node_id: 'a', trusted_via: 'pairing', revoked: false },
        { node_id: 'b', trusted_via: 'passphrase', revoked: false },
        { node_id: 'c', trusted_via: 'pairing', revoked: true },
      ],
    });
    expect([...(await api.pairedNodeIds())]).toEqual(['a']);
  });

  it('reads_the_checks_from_the_doctor', async () => {
    const { api, transport } = apiReplying(200, { checks: [{ name: 'Node', verdict: 'ok', detail: 'x' }] });
    expect(await api.diagnose()).toEqual([{ name: 'Node', verdict: 'ok', detail: 'x' }]);
    expect(transport.requests[0]).toMatchObject({ method: 'GET', path: '/api/doctor' });
  });
});
