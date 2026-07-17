/** @type {import('tailwindcss').Config} */
module.exports = {
  darkMode: ["class", '[data-theme="dark"]'],
  content: [
    "./layouts/**/*.html",
    "./content/**/*.md",
    "./assets/js/**/*.js",
  ],
  theme: {
    extend: {
      colors: {
        base: {
          900: "#0a0e17",
          800: "#11161f",
          700: "#1a1f2e",
          600: "#252b3b",
        },
        accent: {
          blue: "#3b82f6",
          violet: "#8b5cf6",
          cyan: "#22d3ee",
        },
        ink: {
          DEFAULT: "#ece8df",
          muted: "#a8a39a",
          faint: "#6b6962",
        },
      },
      fontFamily: {
        sans: ["Inter", "system-ui", "sans-serif"],
        mono: ["JetBrains Mono", "ui-monospace", "monospace"],
      },
      backgroundImage: {
        "accent-gradient":
          "linear-gradient(120deg, #3b82f6 0%, #8b5cf6 50%, #22d3ee 100%)",
      },
      maxWidth: {
        content: "72rem",
      },
    },
  },
  plugins: [],
};
