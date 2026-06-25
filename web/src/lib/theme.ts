// Fleet-wide dark/light theme, from the tide service. First paint is set
// synchronously by the inline cookie script in index.html; this fetches the
// source of truth and polls so the tab flips live when `b dark`/`b light`
// changes it. Best-effort: if tide is unreachable (e.g. off-tailnet), the
// cookie/default theme stands. Copied per fleet UI (shared pattern, not a
// shared package).

const TIDE_THEME_URL = "https://tide.internal.deepwa7er.com/theme";
const POLL_MS = 5000;

type Theme = "dark" | "light";

function apply(theme: Theme): void {
  const el = document.documentElement;
  el.dataset.theme = theme;
  el.style.colorScheme = theme;
}

export function startTheme(): void {
  const sync = async () => {
    try {
      const res = await fetch(TIDE_THEME_URL, { cache: "no-store" });
      if (!res.ok) return;
      const { theme } = (await res.json()) as { theme: Theme };
      if (theme === "dark" || theme === "light") apply(theme);
    } catch {
      // keep the current (cookie/default) theme
    }
  };
  void sync();
  setInterval(() => void sync(), POLL_MS);
}
