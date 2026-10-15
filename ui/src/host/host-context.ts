import { useApp, type AppContext } from '../state/context';
import type { GuestsApi } from './guests-api';

/** What the node's own page has beyond a guest's: managing guests. */
export interface HostContext extends AppContext {
  guestsApi: GuestsApi;
}

export function useHost(): HostContext {
  return useApp() as HostContext;
}
