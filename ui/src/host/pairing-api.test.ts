import { describe, expect, it } from 'vitest';
import type { NodeTransport, RequestOptions } from '../client/transport';
import { TransferError } from '../client/types';
import { PairingApi } from './pairing-api';

class FakeTransport {
  requests: Array<{ method: string; path: string; body?: string }> = [];
  constructor(private readonly reply: { status: number; text: string }) {}

  async request(method: string, path: string, options: RequestOptions = {}) {
    this.requests.push({ method, path, body: options.body });
    return this.reply;
  }
}

function apiReplying(status: number, text = ''): { api: PairingApi; transport: FakeTransport } {
  const transport = new FakeTransport({ status, text });
  return { api: new PairingApi(transport as unknown as NodeTransport), transport };
}

describe('PairingApi', () => {
  it('dials_the_address_typed_in', async () => {
    const { api, transport } = apiReplying(202);
    await api.dial('192.168.1.20:8080');
    expect(transport.requests).toEqual([{ method: 'POST', path: '/api/pairing/dial', body: '{"address":"192.168.1.20:8080"}' }]);
  });

  it('answers_a_code_by_its_id', async () => {
    const { api, transport } = apiReplying(204);
    await api.answer(3, true);
    expect(transport.requests[0]).toMatchObject({ method: 'POST', path: '/api/pairing/3', body: '{"accepted":true}' });
  });

  it('revokes_a_node_by_its_id', async () => {
    const { api, transport } = apiReplying(204);
    await api.revoke('abc123');
    expect(transport.requests[0]).toMatchObject({ method: 'POST', path: '/api/trust/abc123/revoke' });
  });

  it('reports_why_a_code_could_not_be_answered', async () => {
    const { api } = apiReplying(404, JSON.stringify({ message: 'That pairing is no longer waiting' }));
    await expect(api.answer(9, true)).rejects.toThrow(new TransferError('That pairing is no longer waiting'));
  });
});
