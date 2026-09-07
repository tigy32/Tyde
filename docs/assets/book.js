const root = document.documentElement;
const themeButton = document.querySelector('.theme-toggle');
let savedTheme;
try { savedTheme = localStorage.getItem('tyde-book-theme'); } catch {}
root.dataset.theme = savedTheme === 'light' ? 'light' : 'dark';
themeButton.hidden = false;
themeButton.addEventListener('click', () => {
  root.dataset.theme = root.dataset.theme === 'dark' ? 'light' : 'dark';
  try { localStorage.setItem('tyde-book-theme', root.dataset.theme); } catch {}
});
const menu = document.querySelector('.chapter-menu');
const narrow = matchMedia('(max-width: 760px)');
const updateMenu = () => { menu.open = !narrow.matches; };
updateMenu();
narrow.addEventListener('change', updateMenu);
const dialog = document.querySelector('#search-dialog');
const input = document.querySelector('#search-input');
const results = document.querySelector('#search-results');
const status = document.querySelector('#search-status');
const searchButton = document.querySelector('.search-open');
searchButton.hidden = false;
function search() {
  const query = input.value.trim().toLowerCase();
  const terms = query.split(/\s+/).filter(Boolean);
  const matches = window.TYDE_BOOK_SEARCH.filter(page => terms.every(term => (page.title + ' ' + page.text).toLowerCase().includes(term)));
  results.replaceChildren();
  status.textContent = query ? `${matches.length} matching chapter${matches.length === 1 ? '' : 's'}` : 'Browse all chapters, or type to search.';
  for (const page of matches) {
    const link = document.createElement('a');
    link.href = page.url;
    const heading = document.createElement('strong');
    heading.textContent = page.title;
    const excerpt = document.createElement('p');
    const location = terms.length ? page.text.toLowerCase().indexOf(terms[0]) : 0;
    const start = Math.max(0, location - 65);
    excerpt.textContent = (start ? '…' : '') + page.text.slice(start, start + 190) + '…';
    link.append(heading, excerpt);
    results.append(link);
  }
}
function openSearch() { if (!dialog.open) { dialog.showModal(); search(); input.focus(); } }
searchButton.addEventListener('click', openSearch);
document.querySelector('.search-close').addEventListener('click', () => dialog.close());
input.addEventListener('input', search);
dialog.addEventListener('keydown', event => {
  if (event.key === 'Escape') {
    event.preventDefault();
    event.stopPropagation();
    dialog.close();
  }
});
document.addEventListener('keydown', event => {
  if (event.key === '/' && !event.ctrlKey && !event.metaKey && !event.altKey && !event.target.closest('input,textarea,[contenteditable]')) {
    event.preventDefault(); openSearch();
  }
});
for (const diagram of document.querySelectorAll('.studio-map')) {
  const button = diagram.querySelector('.connection-toggle');
  if (!button) continue;
  button.hidden = false;
  button.addEventListener('click', () => {
    const disconnected = diagram.dataset.disconnected !== 'true';
    diagram.dataset.disconnected = String(disconnected);
    button.textContent = disconnected ? 'Reconnect laptop' : 'Disconnect laptop';
    diagram.querySelector('.map-client small').textContent = disconnected ? 'Disconnected' : 'Connected to both hosts';
    diagram.querySelector('.map-status').textContent = disconnected ? 'The connection is closed. Agents keep running on their hosts.' : 'Connected. Tyde shows the hosts’ current state again.';
  });
}

for (const diagram of document.querySelectorAll('.studio-map')) {
  const motion = diagram.querySelector('.motion-toggle');
  if (!motion) continue;
  motion.hidden = false;
  motion.addEventListener('click', () => {
    const paused = motion.getAttribute('aria-pressed') !== 'true';
    motion.setAttribute('aria-pressed', String(paused));
    motion.textContent = paused ? 'Resume motion' : 'Pause motion';
    diagram.dataset.paused = String(paused);
  });
}
