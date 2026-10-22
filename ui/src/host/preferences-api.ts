import type { NodeTransport } from '../client/transport';
import { callNode } from './node-api';

export type AutoAcceptFrom = 'nobody' | 'paired' | 'mesh';

export interface AutoAcceptRule {
  from: AutoAcceptFrom;
  ask_above_mib: number | null;
}

export interface Preferences {
  is_setup_done: boolean;
  downloads_dir: string | null;
  storage_limit_gib: number;
  auto_accept: AutoAcceptRule;
  // Other nodes kept connected by address, as "ip:port".
  peer_addresses: string[];
}

export interface PreferencesView {
  preferences: Preferences;
  data_dir: string;
  is_default_data_dir: boolean;
  has_passphrase: boolean;
  needs_restart: boolean;
  store_used_bytes: number;
  disk_free_bytes: number | null;
  platform: 'windows' | 'unix';
  // What another device types to add this one by address.
  this_address: string | null;
}

export interface PreferencesUpdate {
  // An empty path goes back to the system's downloads folder.
  downloads_dir?: string;
  storage_limit_gib?: number;
  auto_accept?: AutoAcceptRule;
  // The whole list, as typed; an IP alone uses the default port.
  peer_addresses?: string[];
}

export type Verdict = 'ok' | 'warning' | 'problem';

export interface Check {
  name: string;
  verdict: Verdict;
  detail: string;
}

interface TrustedNode {
  node_id: string;
  trusted_via: 'passphrase' | 'pairing';
  revoked: boolean;
}

/** What the person at the node's machine chooses about their node, and how healthy the node says it is. */
export class PreferencesApi {
  constructor(private readonly transport: NodeTransport) {}

  get(): Promise<PreferencesView> {
    return this.call<PreferencesView>('GET', '/api/preferences');
  }

  update(update: PreferencesUpdate): Promise<PreferencesView> {
    return this.call<PreferencesView>('PUT', '/api/preferences', update);
  }

  // The running node keeps its passphrase until it restarts, which is why the view says so.
  changePassphrase(passphrase: string): Promise<PreferencesView> {
    return this.call<PreferencesView>('PUT', '/api/passphrase', { passphrase });
  }

  async currentPassphrase(): Promise<string | null> {
    return (await this.call<{ passphrase: string | null }>('GET', '/api/passphrase')).passphrase;
  }

  async suggestPassphrase(): Promise<string> {
    return (await this.call<{ passphrase: string }>('GET', '/api/passphrase/suggestion')).passphrase;
  }

  async diagnose(): Promise<Check[]> {
    return (await this.call<{ checks: Check[] }>('GET', '/api/doctor')).checks;
  }

  async pairedNodeIds(): Promise<Set<string>> {
    const { nodes } = await this.call<{ nodes: TrustedNode[] }>('GET', '/api/trust');
    return new Set(nodes.filter((node) => node.trusted_via === 'pairing' && !node.revoked).map((node) => node.node_id));
  }

  private call<T>(method: string, path: string, body?: unknown): Promise<T> {
    return callNode<T>(this.transport, method, path, body);
  }
}
