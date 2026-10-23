import type { NoticeLevel } from '../client/types';

export interface ToastAction {
  label: string;
  run: () => void;
}

export interface Toast {
  id: number;
  level: NoticeLevel;
  text: string;
  actions: ToastAction[];
}

const DEFAULT_DURATION_MS = 4000;
const ERROR_DURATION_MS = 7000;

export class Toasts {
  items = $state<Toast[]>([]);
  private nextId = 1;

  show(level: NoticeLevel, text: string, ...actions: ToastAction[]): void {
    const toast: Toast = { id: this.nextId++, level, text, actions };
    this.items.push(toast);
    // A toast with something to do, like an error, stays long enough to be acted on.
    const isLingering = level === 'error' || level === 'warning' || actions.length > 0;
    const duration = isLingering ? ERROR_DURATION_MS : DEFAULT_DURATION_MS;
    setTimeout(() => this.dismiss(toast.id), duration);
  }

  dismiss(id: number): void {
    this.items = this.items.filter((toast) => toast.id !== id);
  }
}
