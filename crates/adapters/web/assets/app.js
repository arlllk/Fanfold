// Fanfold's own script: a few Alpine components and the glue htmx leaves to us.
//
// Every page works without it: forms submit, links navigate. htmx (live updates, search as
// you type) and Alpine (the search box's suggestions, the filter panel on a phone) only make
// that faster. Alpine is its CSP build, so markup only names the methods below; nothing is
// evaluated from attributes, and the Content-Security-Policy stays free of 'unsafe-eval'.
(() => {
  'use strict';

  const root = document.documentElement;

  // The dot in the header says whether live updates are connected.
  document.addEventListener('htmx:sse:after:connection', () => { root.dataset.live = 'on'; });
  for (const name of ['htmx:sse:error', 'htmx:sse:close']) {
    document.addEventListener(name, () => { root.dataset.live = 'off'; });
  }

  // Never refresh a page out from under someone who is selecting rows (the history's
  // "Retry selected"), nor while the tab is hidden: the next event or the timer catches up.
  document.addEventListener('htmx:before:request', (event) => {
    const source = event.detail?.ctx?.sourceElement;
    if (!source || source.id !== 'live') return;
    if (document.hidden || source.querySelector('input[type="checkbox"]:checked')) {
      event.preventDefault();
    }
  });

  // The search box: suggestions as a combobox (arrows, Enter, Escape), and every link that
  // carries a query (`data-q`: chips, filters, suggestions) writes it into the box instead of
  // navigating, so the search runs as if typed. Modified clicks still open the link.
  const search = () => ({
    active: -1,
    focused: false,
    dismissed: false,
    // How many suggestions there are. Kept as state (the DOM is not reactive), so the list
    // shows and hides when htmx brings new ones.
    count: 0,

    init() {
      this.count = this.options.length;
      document.addEventListener('click', (event) => this.follow(event));
      // New suggestions arrived with a search result: nothing is highlighted any more.
      document.addEventListener('htmx:after:swap', () => {
        this.count = this.options.length;
        this.active = -1;
        this.dismissed = false;
      });
    },

    get input() { return this.$root.querySelector('input[type="search"]'); },
    get options() { return Array.from(this.$root.querySelectorAll('#suggest [role="option"]')); },
    get expanded() { return this.focused && !this.dismissed && this.count > 0; },
    get expandedText() { return this.expanded ? 'true' : 'false'; },
    get activeId() {
      const option = this.options[this.active];
      return this.expanded && option ? option.id : '';
    },

    show() { this.focused = true; },
    // A moment's grace, so a click on a suggestion lands before the list closes.
    hide() { setTimeout(() => { this.focused = false; }, 150); },
    hold(event) { event.preventDefault(); },

    mark() {
      this.options.forEach((option, i) => option.setAttribute('aria-selected', i === this.active ? 'true' : 'false'));
      this.options[this.active]?.scrollIntoView({ block: 'nearest' });
    },

    key(event) {
      const count = this.options.length;
      if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
        if (!count) return;
        event.preventDefault();
        this.dismissed = false;
        // -1 is the box itself: Down from the last option, or Up from the first, returns there.
        if (event.key === 'ArrowDown') {
          this.active = this.active + 1 < count ? this.active + 1 : -1;
        } else {
          this.active = this.active - 1 >= -1 ? this.active - 1 : count - 1;
        }
        this.mark();
      } else if (event.key === 'Enter' && this.expanded && this.active >= 0) {
        event.preventDefault();
        this.choose(this.options[this.active]);
      } else if (event.key === 'Escape' && this.expanded) {
        event.preventDefault();
        this.dismissed = true;
        this.active = -1;
        this.mark();
      }
    },

    pick(event) {
      const option = event.target.closest('[role="option"]');
      if (!option) return;
      event.preventDefault();
      this.choose(option);
    },

    choose(option) {
      if (option.dataset.q !== undefined) {
        this.write(option.dataset.q);
      } else if (option.href) {
        window.location.assign(option.href);
      }
    },

    follow(event) {
      if (event.defaultPrevented || event.button !== 0 || event.ctrlKey || event.metaKey || event.shiftKey || event.altKey) return;
      const link = event.target.closest('#live a[data-q]');
      if (!link) return;
      event.preventDefault();
      this.write(link.dataset.q);
    },

    // A finished query (picked, not typed) ends with a space, so its last word is no
    // longer searched as a prefix and suggestions close.
    write(query) {
      const input = this.input;
      input.value = query === '' ? '' : `${query} `;
      this.active = -1;
      this.dismissed = query !== '';
      input.focus();
      input.dispatchEvent(new Event('input', { bubbles: true }));
    },
  });

  // The filter panel folds away on a phone; on a wide screen it is always open.
  const disclosure = () => ({
    open: false,
    toggle() { this.open = !this.open; },
    get openText() { return this.open ? 'true' : 'false'; },
    get panelClass() { return this.open ? '' : 'max-lg:hidden'; },
  });

  document.addEventListener('alpine:init', () => {
    window.Alpine.data('search', search);
    window.Alpine.data('disclosure', disclosure);
  });
})();
