import { isTauri } from '@tauri-apps/api/core';
import { mount } from 'svelte';
import { HttpTransport } from '../client/http-transport';
import { LocalNodeClient } from '../client/local-node-client';
import type { NodeTransport } from '../client/transport';
import type { LadexClient } from '../client/types';
import { contextFor } from '../state/context';
import { Room } from '../state/room.svelte';
import { Toasts } from '../state/toasts.svelte';
import { Transfers } from '../state/transfers.svelte';
import '../styles/base.css';
import App from './App.svelte';
import { GuestsApi } from './guests-api';
import type { HostContext } from './host-context';

interface HostServices {
  client: LadexClient;
  transport: NodeTransport;
}

// The app's window reaches its node in-process; any browser reaches it over HTTP.
async function connectToNode(): Promise<HostServices> {
  if (isTauri()) {
    const { IpcClient } = await import('../client/ipc-client');
    const client = new IpcClient();
    return { client, transport: client.transport };
  }
  const transport = new HttpTransport();
  return { client: new LocalNodeClient(transport), transport };
}

const { client, transport } = await connectToNode();
const toasts = new Toasts();
const context: HostContext = {
  room: new Room(client, toasts),
  transfers: new Transfers(client, toasts),
  toasts,
  guestsApi: new GuestsApi(transport),
};
mount(App, { target: document.getElementById('app') as HTMLElement, context: contextFor(context) });
