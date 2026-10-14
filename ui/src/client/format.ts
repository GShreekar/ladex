const UNITS = ['B', 'KB', 'MB', 'GB', 'TB'];

export function formatBytes(bytes: number): string {
  if (!(bytes > 0)) return '0 B';
  const unit = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), UNITS.length - 1);
  const value = bytes / 1024 ** unit;
  return `${value.toFixed(unit === 0 ? 0 : value < 10 ? 2 : 1)} ${UNITS[unit]}`;
}

export function formatSpeed(bytesPerSecond: number): string {
  return `${formatBytes(bytesPerSecond)}/s`;
}

export function formatTimeLeft(seconds: number): string {
  const total = Math.round(seconds);
  if (total < 60) return `${total}s left`;
  if (total < 3600) return `${Math.floor(total / 60)}m ${total % 60}s left`;
  return `${Math.floor(total / 3600)}h ${Math.floor((total % 3600) / 60)}m left`;
}

export function formatAgo(timestampMs: number, nowMs: number = Date.now()): string {
  const seconds = Math.max(0, Math.round((nowMs - timestampMs) / 1000));
  if (seconds < 10) return 'just now';
  if (seconds < 60) return `${seconds}s ago`;
  if (seconds < 3600) return `${Math.floor(seconds / 60)} min ago`;
  if (seconds < 86400) return `${Math.floor(seconds / 3600)} h ago`;
  return `${Math.floor(seconds / 86400)} d ago`;
}

/** "Pixel 8 · Chrome" from a User-Agent string, or null when it says nothing useful. */
export function friendlyDeviceName(userAgent: string | null | undefined): string | null {
  if (!userAgent) return null;
  const operatingSystem = detectOperatingSystem(userAgent);
  const browser = detectBrowser(userAgent);
  if (operatingSystem && browser) return `${operatingSystem} · ${browser}`;
  return operatingSystem ?? browser;
}

function detectOperatingSystem(userAgent: string): string | null {
  if (/iPhone/.test(userAgent)) return 'iPhone';
  if (/iPad/.test(userAgent)) return 'iPad';
  if (/Android/.test(userAgent)) {
    const model = userAgent.match(/Android [\d.]+;\s*([^;)]+?)(?:\s+Build\/|\))/)?.[1]?.trim();
    return model && model !== 'K' ? model : 'Android';
  }
  if (/Windows/.test(userAgent)) return 'Windows';
  if (/Mac OS X/.test(userAgent)) return 'Mac';
  if (/Linux/.test(userAgent)) return 'Linux';
  return null;
}

function detectBrowser(userAgent: string): string | null {
  if (/Edg\//.test(userAgent)) return 'Edge';
  if (/OPR\//.test(userAgent)) return 'Opera';
  if (/Firefox\//.test(userAgent)) return 'Firefox';
  if (/Chrome\/|CriOS\//.test(userAgent)) return 'Chrome';
  if (/Safari\//.test(userAgent)) return 'Safari';
  return null;
}

export function deviceKindOf(userAgent: string | null | undefined): 'laptop' | 'phone' | 'tablet' | 'unknown' {
  if (!userAgent) return 'unknown';
  if (/iPad|Tablet/.test(userAgent)) return 'tablet';
  if (/iPhone|Android.*Mobile|Mobile/.test(userAgent)) return 'phone';
  if (/Android/.test(userAgent)) return 'tablet';
  return /Windows|Mac OS X|Linux|CrOS/.test(userAgent) ? 'laptop' : 'unknown';
}

export function randomId(prefix: string, byteCount: number): string {
  const bytes = crypto.getRandomValues(new Uint8Array(byteCount));
  return prefix + Array.from(bytes, (byte) => byte.toString(16).padStart(2, '0')).join('');
}

export function initialsOf(name: string): string {
  const words = name.replace(/['’]s\b/g, '').split(/[\s·]+/).filter(Boolean);
  const letters = words.slice(0, 2).map((word) => word[0]?.toUpperCase() ?? '');
  return letters.join('') || '?';
}

const DOCUMENT_TYPES = /pdf|msword|officedocument|opendocument|text\/|rtf|epub/;

export type FileCategory = 'document' | 'media' | 'archive' | 'other';

export function categoryOf(mimeType: string, isFolder: boolean): FileCategory {
  if (isFolder || /zip|tar|gzip|compressed|7z|rar/.test(mimeType)) return 'archive';
  if (/^(image|video|audio)\//.test(mimeType)) return 'media';
  if (DOCUMENT_TYPES.test(mimeType)) return 'document';
  return 'other';
}
