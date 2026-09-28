/** @type {import('tailwindcss').Config} */
// The dashboard's design tokens: ARC's engraved look, as in the desktop app (desktop/src/styles/engraved.css).
// White line on Arc blue #002dde. Marcellus is for words only; every number is set in Hanken Grotesk (JetBrains Mono for
// hashes, addresses and code), and a figure inside a Marcellus heading comes from Hanken Grotesk ("Arc Figures").
// tailwind.input.css publishes these as CSS custom properties, which app.css consumes; Preflight takes the faces.
module.exports = {
  content: ["./index.html"],
  darkMode: "class",
  theme: {
    extend: {
      colors: {
        arc: {
          // the ground everything is printed on, and the deeper inked areas cut into it
          DEFAULT: "#002dde",
          deep: "#0026c4",
          well: "rgba(0, 16, 120, 0.34)",
          bar: "rgba(0, 36, 184, 0.9)",
          // the logo mark's container, and the pale ink of a second display line
          mark: "#1d4cff",
          paper: "#c9d6ff",
        },
        // muted ink stays at least 4.5:1 on the ground and on plates; faint is for decoration only
        ink: {
          DEFAULT: "#ffffff",
          soft: "rgba(255, 255, 255, 0.88)",
          muted: "rgba(255, 255, 255, 0.74)",
          faint: "rgba(255, 255, 255, 0.5)",
        },
        line: {
          DEFAULT: "rgba(255, 255, 255, 0.2)",
          strong: "rgba(255, 255, 255, 0.42)",
          soft: "rgba(255, 255, 255, 0.11)",
        },
        // states keep their hue so they read at a glance, pitched to sit on the blue
        state: {
          good: "#8af2cb",
          "good-bg": "rgba(138, 242, 203, 0.13)",
          warn: "#ffd98c",
          "warn-bg": "rgba(255, 217, 140, 0.13)",
          bad: "#ffb0b0",
          "bad-bg": "rgba(255, 150, 150, 0.15)",
          info: "#b3dcff",
        },
      },
      borderColor: {
        DEFAULT: "rgba(255, 255, 255, 0.2)",
      },
      fontFamily: {
        display: ['"Arc Figures"', '"Marcellus"', "Georgia", '"Times New Roman"', "serif"],
        sans: ['"Hanken Grotesk"', "-apple-system", "BlinkMacSystemFont", '"Segoe UI"', "Roboto", "sans-serif"],
        mono: ['"JetBrains Mono"', '"SF Mono"', "ui-monospace", "Menlo", "Consolas", "monospace"],
      },
      borderRadius: {
        plate: "6px",
        "plate-sm": "4px",
      },
    },
  },
};
