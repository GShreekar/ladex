import { categoryOf } from '../client/format';
import type { SharedFile } from '../client/types';
import type { IconName } from './icons';

export function iconFor(file: Pick<SharedFile, 'mimeType' | 'isFolder'>): IconName {
  if (file.isFolder) return 'folder';
  const { mimeType } = file;
  if (mimeType.startsWith('image/')) return 'image';
  if (mimeType.startsWith('video/')) return 'video';
  if (mimeType.startsWith('audio/')) return 'music';
  if (categoryOf(mimeType, false) === 'archive') return 'archive';
  if (categoryOf(mimeType, false) === 'document') return 'file-text';
  return 'file';
}

export function deviceIcon(kind: 'laptop' | 'phone' | 'tablet' | 'unknown'): IconName {
  if (kind === 'phone') return 'phone';
  if (kind === 'tablet') return 'tablet';
  return kind === 'laptop' ? 'laptop' : 'monitor';
}
