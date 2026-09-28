// The launch screen lives in index.html, outside React's root: it paints before any script loads, and its animation
// plays on from the first frame instead of restarting when React mounts. The app calls dismissBootSplash() once it
// knows what to show (the saved identity and config have loaded), and the screen fades away over the app.

// Long enough for the arch to finish drawing, so a fast start never flickers a half-drawn picture.
const MIN_VISIBLE_MS = 650;
// The fade takes 320 ms (index.html). Reduced motion has no transition, and a hidden window may never fire
// transitionend, so the node is removed on a timer as well.
const REMOVE_AFTER_MS = 600;

let dismissed = false;

export function dismissBootSplash(): void {
  if (dismissed || typeof document === "undefined") return;
  dismissed = true;
  const el = document.getElementById("boot");
  if (!el) return;
  const wait = Math.max(0, MIN_VISIBLE_MS - performance.now());
  window.setTimeout(() => {
    el.classList.add("boot--out");
    const remove = () => el.remove();
    el.addEventListener("transitionend", remove, { once: true });
    window.setTimeout(remove, REMOVE_AFTER_MS);
  }, wait);
}
