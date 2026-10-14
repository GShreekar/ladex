import type { NoticeLevel } from '../client/types';

export interface ToastAction {
  label: string;
  run: () => void;
}

export interface Toast {
  id: number;
  level: NoticeLevel;
  text: string;
  action?: ToastAction;
}

const DEFAULT_DURATION_MS = 4000;
const ERROR_DURATION_MS = 7000;

export class Toasts {
  items = $state<Toast[]>([]);
  private nextId = 1;

  show(level: NoticeLevel, text: string, action?: ToastAction): void {
    const toast: Toast = { id: this.nextId++, level, text, action };
    this.items.push(toast);
    const duration = level === 'error' || level === 'warning' ? ERROR_DURATION_MS : DEFAULT_DURATION_MS;
    setTimeout(() => this.dismiss(toast.id), duration);
  }

  dismiss(id: number): void {
    this.items = this.items.filter((toast) => toast.id !== id);
  }
}
