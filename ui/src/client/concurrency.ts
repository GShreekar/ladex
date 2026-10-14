/** Runs `work` over `items`, at most `limit` at a time; stops starting new work after the first failure. */
export async function runLimited<T>(items: T[], limit: number, work: (item: T, index: number) => Promise<void>): Promise<void> {
  let nextIndex = 0;
  let failure: unknown = null;
  let hasFailed = false;

  async function worker(): Promise<void> {
    while (nextIndex < items.length && !hasFailed) {
      const index = nextIndex++;
      try {
        await work(items[index] as T, index);
      } catch (error) {
        hasFailed = true;
        failure = error;
      }
    }
  }

  await Promise.all(Array.from({ length: Math.min(limit, items.length) }, worker));
  if (hasFailed) throw failure;
}

export function sleep(milliseconds: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, milliseconds));
}
