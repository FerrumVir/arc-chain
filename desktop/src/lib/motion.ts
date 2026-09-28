// Motion constants for framer-motion, matching the CSS tokens in styles/tokens.css (--ease-*, --dur-*), so animated
// components and CSS transitions move alike. framer-motion takes seconds; the CSS tokens are in milliseconds.

export const EASE_OUT: [number, number, number, number] = [0.22, 1, 0.36, 1];
export const EASE_IN_OUT: [number, number, number, number] = [0.65, 0, 0.35, 1];

export const DUR_FAST = 0.12;
export const DUR_BASE = 0.2;
export const DUR_SLOW = 0.36;
