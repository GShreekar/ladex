export type ThemePreference = 'system' | 'light' | 'dark';

const STORAGE_KEY = 'ladex_theme';

function loadPreference(): ThemePreference {
  try {
    const saved = localStorage.getItem(STORAGE_KEY);
    return saved === 'light' || saved === 'dark' ? saved : 'system';
  } catch {
    return 'system';
  }
}

function applyTheme(preference: ThemePreference): void {
  if (preference === 'system') delete document.documentElement.dataset.theme;
  else document.documentElement.dataset.theme = preference;
}

class Theme {
  preference = $state<ThemePreference>(loadPreference());

  constructor() {
    applyTheme(this.preference);
  }

  choose(preference: ThemePreference): void {
    this.preference = preference;
    applyTheme(preference);
    try {
      localStorage.setItem(STORAGE_KEY, preference);
    } catch {
      // The choice still applies for this visit.
    }
  }
}

export const theme = new Theme();
