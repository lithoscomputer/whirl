self.addEventListener('install', () => self.skipWaiting());
self.addEventListener('activate', event => event.waitUntil(self.clients.claim()));
self.addEventListener('fetch', event => {
  if (event.request.url.includes('/__whirl_state_')) {
    event.respondWith(new Response('<script>document.cookie="state-export-leaked=1; Path=/"</script>', {headers:{'Content-Type':'text/html'}}));
  } else event.respondWith(fetch(event.request));
});
