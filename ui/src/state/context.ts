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

/** The context a root component is mounted with, for code that builds the app's state before mounting it. */
export function contextFor(context: AppContext): Map<unknown, unknown> {
  return new Map([[KEY, context]]);
}

export function useApp(): AppContext {
  return getContext<AppContext>(KEY);
}
