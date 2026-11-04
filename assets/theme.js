// The theme: auto (the system's), light or dark, kept in this browser.
// Loaded in <head> without defer, so the page is drawn in its theme from the start.
(() => {
  const root = document.documentElement;
  const KEY = "irisqo-theme";
  const CHOICES = ["auto", "light", "dark"];
  const LABELS = { auto: "system", light: "light", dark: "dark" };
  const apply = (choice) => {
    const theme = CHOICES.includes(choice) ? choice : "auto";
    root.dataset.themeChoice = theme;
    if (theme === "auto") delete root.dataset.theme;
    else root.dataset.theme = theme;
    return theme;
  };
  let stored = null;
  try { stored = localStorage.getItem(KEY); } catch { /* storage blocked: auto */ }
  let choice = apply(stored);

  document.addEventListener("DOMContentLoaded", () => {
    const button = document.getElementById("theme-toggle");
    if (!button) return;
    const label = () => {
      const text = `Theme: ${LABELS[choice]}. Switch the theme`;
      button.setAttribute("aria-label", text);
      button.title = text;
    };
    label();
    button.hidden = false;
    button.addEventListener("click", () => {
      choice = apply(CHOICES[(CHOICES.indexOf(choice) + 1) % CHOICES.length]);
      try { localStorage.setItem(KEY, choice); } catch { /* not kept */ }
      label();
    });
  });
})();
