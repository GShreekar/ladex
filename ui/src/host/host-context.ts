import { useApp, type AppContext } from '../state/context';
import type { DesktopSettings } from '../desktop/desktop';
import type { PhoneShell } from '../desktop/phone';
import type { GuestsApi } from './guests-api';
import type { PairingApi } from './pairing-api';
import type { PreferencesApi } from './preferences-api';

/** What the node's own page has beyond a guest's: managing guests. */
export interface HostContext extends AppContext {
  guestsApi: GuestsApi;
  pairingApi: PairingApi;
  preferencesApi: PreferencesApi;
  // Present in the desktop app only: the app's own settings and what the window cannot do itself.
  desktop: DesktopSettings | null;
  // The app on a phone: the desktop-only settings are hidden.
  isMobile: boolean;
  // Present in the Android app only: files shared in from other apps, and the folder received files are saved in.
  phone: PhoneShell | null;
}

export function useHost(): HostContext {
  return useApp() as HostContext;
}
