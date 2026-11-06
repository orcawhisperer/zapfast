// Makes the page installable and able to start offline, on the archive it already
// holds. Always asks the network first, so a reload gets the newest build, and uses
// its copy only when the network does not answer. It never sees WhatsApp's traffic:
// only this site's own files pass through it.
const CACHE = "zapfast-shell";

self.addEventListener("install", () => self.skipWaiting());
self.addEventListener("activate", (event) => event.waitUntil(self.clients.claim()));

self.addEventListener("fetch", (event) => {
  const url = new URL(event.request.url);
  if (event.request.method !== "GET" || url.origin !== self.location.origin) {
    return;
  }
  event.respondWith(
    (async () => {
      const cache = await caches.open(CACHE);
      try {
        const answer = await fetch(event.request);
        if (answer.ok) {
          await cache.put(event.request, answer.clone());
        }
        return answer;
      } catch (error) {
        const kept = await cache.match(event.request, { ignoreSearch: true });
        if (kept) {
          return kept;
        }
        throw error;
      }
    })(),
  );
});
