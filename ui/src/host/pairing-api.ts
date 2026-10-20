import type { NodeTransport } from '../client/transport';
import { callNode } from './node-api';

export interface WaitingCode {
  id: number;
  node_id: string;
  name: string;
  words: string[];
  // This node dialed the other, rather than the other way round.
  dialed: boolean;
}

export interface PairingOutcome {
  name: string;
  paired: boolean;
  message: string;
}

export interface PairingStatus {
  can_pair: boolean;
  address: string | null;
  open: boolean;
  seconds_left: number;
  waiting: WaitingCode[];
  // Newest first.
  recent: PairingOutcome[];
}

export interface TrustedNode {
  node_id: string;
  name: string;
  trusted_via: 'passphrase' | 'pairing';
  revoked: boolean;
  connected: boolean;
  revoked_by: string | null;
}

/** Pairing with other nodes by comparing six words, and the nodes this one trusts. */
export class PairingApi {
  constructor(private readonly transport: NodeTransport) {}

  status(): Promise<PairingStatus> {
    return callNode<PairingStatus>(this.transport, 'GET', '/api/pairing');
  }

  async open(): Promise<void> {
    await callNode(this.transport, 'POST', '/api/pairing/open', {});
  }

  async close(): Promise<void> {
    await callNode(this.transport, 'POST', '/api/pairing/close', {});
  }

  async dial(address: string): Promise<void> {
    await callNode(this.transport, 'POST', '/api/pairing/dial', { address });
  }

  async answer(id: number, accepted: boolean): Promise<void> {
    await callNode(this.transport, 'POST', `/api/pairing/${id}`, { accepted });
  }

  async trustedNodes(): Promise<TrustedNode[]> {
    return (await callNode<{ nodes: TrustedNode[] }>(this.transport, 'GET', '/api/trust')).nodes;
  }

  async revoke(nodeId: string): Promise<void> {
    await callNode(this.transport, 'POST', `/api/trust/${encodeURIComponent(nodeId)}/revoke`, {});
  }
}
