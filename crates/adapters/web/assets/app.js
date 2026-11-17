// Live updates. The server renders every page completely, so the UI works without
// this file; it only refreshes the page body when something changes.
(() => {
  const live = document.getElementById('live');
  if (!live || !('EventSource' in window)) return;
  const jobId = live.dataset.job || null; // set on a job page: ignore other jobs' events
  let timer = null;
  let busy = false;

  const refresh = async () => {
    if (busy || document.hidden) return;
    // Never swap the page out from under someone who is selecting rows.
    if (live.querySelector('input:checked')) return;
    busy = true;
    try {
      const oldLog = live.querySelector('.log');
      const stuck = oldLog && oldLog.scrollTop + oldLog.clientHeight >= oldLog.scrollHeight - 8;
      const res = await fetch(location.pathname + location.search, {
        headers: { 'X-Fragment': '1' },
        credentials: 'same-origin',
        cache: 'no-store',
      });
      if (res.ok) {
        const incoming = document.createElement('template');
        incoming.innerHTML = await res.text();
        // The site pill sits above the page content; swap it separately.
        const pill = incoming.content.querySelector('[data-pill]');
        const current = document.getElementById('site');
        if (pill && current) current.replaceWith(pill);
        else if (pill) pill.remove();
        live.replaceChildren(incoming.content);
        const log = live.querySelector('.log');
        if (log && stuck) log.scrollTop = log.scrollHeight;
      }
    } catch (_) {
      /* offline: the next event or visibility change retries */
    } finally {
      busy = false;
    }
  };
  const soon = () => { clearTimeout(timer); timer = setTimeout(refresh, 250); };

  const source = new EventSource('/events');
  source.addEventListener('change', (e) => {
    const [kind, id] = e.data.split(' ');
    if (!jobId || kind === 'resync' || id === jobId) soon();
  });
  source.addEventListener('open', () => { document.documentElement.dataset.live = 'on'; soon(); });
  source.addEventListener('error', () => { document.documentElement.dataset.live = 'off'; });
  document.addEventListener('visibilitychange', () => { if (!document.hidden) soon(); });
  setInterval(soon, 30000); // keeps "5 min ago" honest
  const log = live.querySelector('.log');
  if (log) log.scrollTop = log.scrollHeight;
})();
