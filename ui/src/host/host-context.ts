import { useApp, type AppContext } from '../state/context';
import type { DesktopSettings } from '../desktop/desktop';
import type { GuestsApi } from './guests-api';
import type { PreferencesApi } from './preferences-api';

/** What the node's own page has beyond a guest's: managing guests. */
export interface HostContext extends AppContext {
  guestsApi: GuestsApi;
  preferencesApi: PreferencesApi;
  // Present in the desktop app only: the app's own settings and what the window cannot do itself.
  desktop: DesktopSettings | null;
}

export function useHost(): HostContext {
  return useApp() as HostContext;
}
