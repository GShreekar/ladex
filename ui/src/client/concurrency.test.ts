import { describe, expect, it } from 'vitest';
import { runLimited } from './concurrency';

describe('runLimited', () => {
  it('never runs more than the limit at once', async () => {
    let running = 0;
    let peak = 0;
    await runLimited([1, 2, 3, 4, 5, 6], 2, async () => {
      running++;
      peak = Math.max(peak, running);
      await new Promise((resolve) => setTimeout(resolve, 5));
      running--;
    });
    expect(peak).toBe(2);
  });

  it('visits every item', async () => {
    const seen: number[] = [];
    await runLimited([1, 2, 3], 2, async (item) => {
      seen.push(item);
    });
    expect(seen.toSorted()).toEqual([1, 2, 3]);
  });

  it('rejects with the first failure and starts nothing new afterwards', async () => {
    const started: number[] = [];
    const work = runLimited([1, 2, 3, 4], 1, async (item) => {
      started.push(item);
      if (item === 2) throw new Error('boom');
    });
    await expect(work).rejects.toThrow('boom');
    expect(started).toEqual([1, 2]);
  });
});
