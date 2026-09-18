/**
 * The optional half of the web UI (SPEC §11).
 *
 * Everything here is an enhancement. The pages are server-rendered and every
 * action is a form that posts and redirects, so a browser that never runs this
 * file can still read mail, reply, compose and pair. Nothing below may become
 * the only way to do something.
 */

/** Wire up whatever this page happens to have. */
function main(): void {
  liveInbox();
  dropzone();
  unreadBadge();
}

/**
 * Nudge the inbox when mail arrives.
 *
 * Deliberately not a rendering path: the server already knows how to render a
 * message list, and a second implementation here would be one to keep in step.
 * This asks the page to reload itself, which costs a request and cannot drift.
 */
function liveInbox(): void {
  const list = document.querySelector<HTMLElement>('.messages[data-live="true"]');
  if (!list) return;

  const events = new EventSource('/api/v1/events');

  events.addEventListener('message.received', () => {
    // Only when the reader is at the top. Reloading under somebody who has
    // scrolled down to read something would lose their place.
    if (window.scrollY < 40) {
      window.location.reload();
    } else {
      showNewMailBanner();
    }
  });

  // A dropped stream is not worth a message: EventSource reconnects on its
  // own, and the page is still correct as rendered.
  events.addEventListener('error', () => {});
}

/** A quiet "there is new mail" for a reader who has scrolled away. */
function showNewMailBanner(): void {
  if (document.getElementById('new-mail')) return;

  const banner = document.createElement('p');
  banner.id = 'new-mail';
  banner.className = 'problem';
  banner.setAttribute('role', 'status');

  const link = document.createElement('a');
  link.href = window.location.href;
  link.textContent = 'New mail has arrived — reload';
  banner.appendChild(link);

  document.querySelector('main')?.prepend(banner);
}

/** Let files be dropped on the compose form. */
function dropzone(): void {
  const zone = document.getElementById('dropzone');
  const input = document.getElementById('files');
  if (!zone || !(input instanceof HTMLInputElement)) return;

  const stop = (event: DragEvent): void => {
    event.preventDefault();
    event.stopPropagation();
  };

  zone.addEventListener('dragover', (event) => {
    stop(event);
    zone.classList.add('over');
  });

  zone.addEventListener('dragleave', (event) => {
    stop(event);
    zone.classList.remove('over');
  });

  zone.addEventListener('drop', (event) => {
    stop(event);
    zone.classList.remove('over');

    const dropped = event.dataTransfer?.files;
    if (!dropped || dropped.length === 0) return;

    // Assigning to `input.files` is what makes the ordinary form submission
    // carry them, so the no-JavaScript path stays the only path.
    const box = new DataTransfer();
    for (const existing of input.files ?? []) box.items.add(existing);
    for (const file of dropped) box.items.add(file);
    input.files = box.files;

    input.dispatchEvent(new Event('change', { bubbles: true }));
  });
}

/** Keep the unread count in the header honest without a reload. */
function unreadBadge(): void {
  const link = document.querySelector<HTMLAnchorElement>('nav a[href="/"]');
  if (!link) return;

  const events = new EventSource('/api/v1/events');
  const refresh = async (): Promise<void> => {
    try {
      const response = await fetch('/api/v1/me');
      if (!response.ok) return;
      const me = (await response.json()) as { unread?: number };
      const count = me.unread ?? 0;

      let badge = link.querySelector<HTMLSpanElement>('.count');
      if (count === 0) {
        badge?.remove();
        return;
      }
      if (!badge) {
        badge = document.createElement('span');
        badge.className = 'count';
        link.append(' ', badge);
      }
      badge.textContent = String(count);
    } catch {
      // The header is decoration. A failed refresh leaves the number the
      // server rendered, which was true when the page loaded.
    }
  };

  for (const name of ['message.received', 'message.read']) {
    events.addEventListener(name, () => void refresh());
  }
  events.addEventListener('error', () => {});
}

if (document.readyState === 'loading') {
  document.addEventListener('DOMContentLoaded', main);
} else {
  main();
}
