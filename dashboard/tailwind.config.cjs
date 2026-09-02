/** @type {import('tailwindcss').Config} */
module.exports = {
  content: ["./index.html"],
  darkMode: "class",
  theme: {
    extend: {
      colors: {
        // ARC uses Tailwind's indigo scale under a product-specific name.
        arc: {
          50: "#eef2ff",
          100: "#e0e7ff",
          200: "#c7d2fe",
          300: "#a5b4fc",
          400: "#818cf8",
          500: "#6366f1",
          600: "#4f46e5",
          700: "#4338ca",
          800: "#3730a3",
          900: "#312e81",
          950: "#1e1b4b"
        },
        surface: {
          1: "#09090f",
          2: "#0f0f19",
          3: "#141420",
          4: "#1a1a2e",
          5: "#22223a"
        }
      }
    }
  }
};
