const APPROVAL_CACHE = 'ardur-web-client-v1';

self.addEventListener('install', (event) => {
  event.waitUntil(
    caches.open(APPROVAL_CACHE).then((cache) =>
      cache.addAll(['./index.html', './styles.css', './app.js', './manifest.webmanifest', './icons/icon.svg']),
    ),
  );
  self.skipWaiting();
});

self.addEventListener('activate', (event) => {
  event.waitUntil(self.clients.claim());
});

self.addEventListener('fetch', (event) => {
  const request = event.request;
  if (request.method !== 'GET') return;
  event.respondWith(fetch(request).catch(() => caches.match(request)));
});

self.addEventListener('push', (event) => {
  let payload = {};
  try {
    payload = event.data ? event.data.json() : {};
  } catch (_) {
    payload = { title: 'Ardur approval requested', body: event.data?.text() ?? '' };
  }
  const approvalId = payload.approval_id || payload.approvalId || '';
  const url = approvalId ? `./index.html?approval_id=${encodeURIComponent(approvalId)}` : './index.html';
  event.waitUntil(
    self.registration.showNotification(payload.title || 'Ardur approval requested', {
      body: payload.body || 'Review and approve or reject this action.',
      tag: approvalId || 'ardur-approval',
      data: { url, approvalId },
      actions: [
        { action: 'open', title: 'Review' },
        { action: 'approve', title: 'Approve' },
        { action: 'reject', title: 'Reject' },
      ],
    }),
  );
});

self.addEventListener('notificationclick', (event) => {
  event.notification.close();
  const data = event.notification.data || {};
  const url = data.url || './index.html';
  event.waitUntil(
    self.clients.matchAll({ type: 'window', includeUncontrolled: true }).then((clients) => {
      for (const client of clients) {
        if ('focus' in client) {
          client.postMessage({ type: 'approval-action', action: event.action || 'open', approvalId: data.approvalId });
          return client.focus();
        }
      }
      return self.clients.openWindow(url);
    }),
  );
});
