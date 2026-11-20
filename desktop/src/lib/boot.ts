const MIN_VISIBLE_MS = 650;
const REMOVE_AFTER_MS = 600;

let dismissal: Promise<void> | null = null;

export function dismissBootSplash(): Promise<void> {
  if (dismissal) return dismissal;
  if (typeof document === "undefined") return Promise.resolve();
  const el = document.getElementById("boot");
  if (!el) return Promise.resolve();
  dismissal = new Promise<void>((resolve) => {
    const wait = Math.max(0, MIN_VISIBLE_MS - performance.now());
    window.setTimeout(() => {
      el.classList.add("boot--out");
      const remove = () => { el.remove(); resolve(); };
      el.addEventListener("transitionend", remove, { once: true });
      window.setTimeout(remove, REMOVE_AFTER_MS);
    }, wait);
  });
  return dismissal;
}
