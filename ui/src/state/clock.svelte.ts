const TICK_MS = 15_000;

class Clock {
  now = $state(Date.now());

  constructor() {
    setInterval(() => (this.now = Date.now()), TICK_MS);
  }
}

export const clock = new Clock();
