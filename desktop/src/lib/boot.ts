// The launch screen lives in index.html, outside React's root: it paints before any script loads, and its animation
// plays on from the first frame instead of restarting when React mounts. The app calls dismissBootSplash() once it
// knows what to show (the saved identity and config have loaded), and the screen fades away over the app.

// Long enough for the arch to finish drawing, so a fast start never flickers a half-drawn picture.
const MIN_VISIBLE_MS = 650;
// The fade takes 320 ms (index.html). Reduced motion has no transition, and a hidden window may never fire
// transitionend, so the node is removed on a timer as well.
const REMOVE_AFTER_MS = 600;

let dismissal: Promise<void> | null = null;

/** Resolves only when the boot overlay has actually been removed. */
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
