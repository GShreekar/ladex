import { isTauri } from '@tauri-apps/api/core';
import { mount } from 'svelte';
import { HttpTransport } from '../client/http-transport';
import { LocalNodeClient } from '../client/local-node-client';
import type { NodeTransport } from '../client/transport';
import type { LadexClient } from '../client/types';
import { contextFor } from '../state/context';
import { isMobileApp, tauriDesktop, tauriSettings, tauriSetup } from '../desktop/desktop';
import { tauriPhone } from '../desktop/phone';
import { AutoAccept } from '../state/auto-accept';
import { Notifier } from '../state/notifier';
import { Room } from '../state/room.svelte';
import { Toasts } from '../state/toasts.svelte';
import { Transfers } from '../state/transfers.svelte';
import '../styles/base.css';
import App from './App.svelte';
import { GuestsApi } from './guests-api';
import { PairingApi } from './pairing-api';
import { PreferencesApi } from './preferences-api';
import SetupWizard from './SetupWizard.svelte';
import type { HostContext } from './host-context';

interface HostServices {
  client: LadexClient;
  transport: NodeTransport;
}

// A window attached to a running daemon shows that node's page over loopback HTTP, with no app commands behind it.
function isShownByTheApp(): boolean {
  const isLoopbackPage = location.protocol === 'http:' && ['localhost', '127.0.0.1'].includes(location.hostname);
  return isTauri() && !isLoopbackPage;
}

// The app's own window reaches its node in-process; any browser reaches it over HTTP.
async function connectToNode(): Promise<HostServices> {
  if (isShownByTheApp()) {
    const { IpcClient } = await import('../client/ipc-client');
    const client = new IpcClient();
    return { client, transport: client.transport };
  }
  const transport = new HttpTransport();
  return { client: new LocalNodeClient(transport), transport };
}

const target = document.getElementById('app') as HTMLElement;

// A first run has no node yet: the wizard collects the choices the node starts with.
async function showWizardIfNeeded(isMobile: boolean): Promise<boolean> {
  if (!isShownByTheApp()) return false;
  const state = await tauriSetup.state();
  if (!state.isNeeded) return false;
  const phone = isMobile ? tauriPhone : null;
  mount(SetupWizard, { target, props: { setup: tauriSetup, pickFolder: tauriSettings.pickFolder, phone, state, isMobile } });
  return true;
}

const isMobile = isShownByTheApp() && (await isMobileApp());

if (!(await showWizardIfNeeded(isMobile))) {
  const { client, transport } = await connectToNode();
  const toasts = new Toasts();
  const preferencesApi = new PreferencesApi(transport);
  const notifier = isShownByTheApp() ? new Notifier(client, tauriDesktop) : undefined;
  // Only the person at the node's machine has rules for what is saved without asking.
  const autoAccept = client.capabilities.canManageGuests
    ? new AutoAccept({ rule: async () => (await preferencesApi.get()).preferences.auto_accept, pairedNodeIds: () => preferencesApi.pairedNodeIds() })
    : undefined;
  const context: HostContext = {
    room: new Room(client, toasts, notifier, autoAccept),
    transfers: new Transfers(client, toasts),
    toasts,
    guestsApi: new GuestsApi(transport),
    pairingApi: new PairingApi(transport),
    preferencesApi,
    desktop: isShownByTheApp() ? tauriSettings : null,
    isMobile,
    phone: isMobile ? tauriPhone : null,
  };
  mount(App, { target, context: contextFor(context) });
}
