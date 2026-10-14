import { getContext, setContext } from 'svelte';
import type { Room } from './room.svelte';
import type { Toasts } from './toasts.svelte';
import type { Transfers } from './transfers.svelte';

export interface AppContext {
  room: Room;
  transfers: Transfers;
  toasts: Toasts;
}

const KEY = Symbol('ladex-app');

export function provideApp(context: AppContext): void {
  setContext(KEY, context);
}

export function useApp(): AppContext {
  return getContext<AppContext>(KEY);
}
