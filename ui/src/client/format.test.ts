import { describe, expect, it } from 'vitest';
import { categoryOf, formatAgo, formatBytes, formatTimeLeft, friendlyDeviceName, initialsOf } from './format';

describe('formatBytes', () => {
  it('shows zero for nothing', () => expect(formatBytes(0)).toBe('0 B'));
  it('keeps small values in bytes', () => expect(formatBytes(512)).toBe('512 B'));
  it('uses two decimals below ten of a unit', () => expect(formatBytes(1536)).toBe('1.50 KB'));
  it('uses one decimal from ten of a unit', () => expect(formatBytes(12.4 * 1024 * 1024)).toBe('12.4 MB'));
  it('stops at terabytes', () => expect(formatBytes(5 * 1024 ** 5)).toBe('5120.0 TB'));
});

describe('formatTimeLeft', () => {
  it('counts seconds under a minute', () => expect(formatTimeLeft(42)).toBe('42s left'));
  it('counts minutes and seconds', () => expect(formatTimeLeft(185)).toBe('3m 5s left'));
  it('counts hours and minutes', () => expect(formatTimeLeft(3720)).toBe('1h 2m left'));
});

describe('formatAgo', () => {
  const now = 1_000_000_000;
  it('says just now for the last few seconds', () => expect(formatAgo(now - 3000, now)).toBe('just now'));
  it('counts minutes', () => expect(formatAgo(now - 120_000, now)).toBe('2 min ago'));
  it('counts hours', () => expect(formatAgo(now - 3 * 3600_000, now)).toBe('3 h ago'));
});

describe('friendlyDeviceName', () => {
  it('names an iPhone with Safari', () => {
    const ua = 'Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1.15 Version/17.0 Mobile/15E148 Safari/604.1';
    expect(friendlyDeviceName(ua)).toBe('iPhone · Safari');
  });

  it('uses an Android phone’s model', () => {
    const ua = 'Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 Chrome/120.0 Mobile Safari/537.36';
    expect(friendlyDeviceName(ua)).toBe('Pixel 8 · Chrome');
  });

  it('has no name for an empty user agent', () => expect(friendlyDeviceName('')).toBeNull());
});

describe('initialsOf', () => {
  it('takes the first letters of two words', () => expect(initialsOf('Daniel’s Laptop')).toBe('DL'));
  it('takes one letter from a single word', () => expect(initialsOf('iPhone')).toBe('I'));
  it('falls back to a question mark for nothing', () => expect(initialsOf('')).toBe('?'));
});

describe('categoryOf', () => {
  it('treats PDFs as documents', () => expect(categoryOf('application/pdf', false)).toBe('document'));
  it('treats images as media', () => expect(categoryOf('image/png', false)).toBe('media'));
  it('treats folders as archives', () => expect(categoryOf('inode/directory', true)).toBe('archive'));
});
