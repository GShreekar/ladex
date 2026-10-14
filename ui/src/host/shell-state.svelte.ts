export type Page = 'files' | 'transfers' | 'guests' | 'settings';
export type FileFilter = 'all' | 'documents' | 'media';

const PAGES: Page[] = ['files', 'transfers', 'guests', 'settings'];

function pageFromHash(): Page {
  const name = location.hash.slice(1);
  return PAGES.find((page) => page === name) ?? 'files';
}

class ShellState {
  page = $state<Page>(pageFromHash());
  query = $state('');
  filter = $state<FileFilter>('all');

  constructor() {
    window.addEventListener('hashchange', () => (this.page = pageFromHash()));
  }

  go(page: Page): void {
    location.hash = page;
    this.page = page;
  }
}

export const shell = new ShellState();
