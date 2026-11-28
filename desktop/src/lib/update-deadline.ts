export interface UpdateDeadlineTimers {
  setTimeout: (callback: () => void, delayMs: number) => unknown;
  clearTimeout: (handle: unknown) => void;
}

export class UpdateDeadline {
  private timer: unknown | null = null;
  private dueAt: number | null = null;
  private fire: (() => void) | null = null;

  constructor(
    private readonly timers: UpdateDeadlineTimers,
    private readonly now: () => number,
  ) {}

  get pending(): boolean {
    return this.fire !== null;
  }

  schedule(callback: () => void, delayMs: number): void {
    this.clear();
    const fire = () => {
      if (this.fire !== fire) return;
      this.clear();
      callback();
    };
    this.dueAt = this.now() + delayMs;
    this.fire = fire;
    const timer = this.timers.setTimeout(fire, delayMs);
    if (this.fire === fire) this.timer = timer;
    else this.timers.clearTimeout(timer);
  }

  runOverdue(now: number): void {
    if (this.dueAt !== null && this.dueAt <= now) this.fire?.();
  }

  clear(): void {
    if (this.timer !== null) this.timers.clearTimeout(this.timer);
    this.timer = null;
    this.dueAt = null;
    this.fire = null;
  }
}
