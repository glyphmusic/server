// Run after worker-build --dev --no-opt. No live providers or Cloudflare account.
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
const require = createRequire(import.meta.url);
const { Miniflare } = require('miniflare');
const calls = [];
const token = 'test-only-companion-token-32-characters';
const sample = '0123456789abcdefghijklmnopqrstuvwxyz';
let delayedComplete = false;
const mf = new Miniflare({
  modules: true,
  scriptPath: fileURLToPath(new URL('../build/index.js', import.meta.url)),
  modulesRules: [{ type: 'CompiledWasm', include: ['**/*.wasm'], fallthrough: true }],
  compatibilityDate: '2025-01-01',
  bindings: { YTDLP_SERVICE_URL: 'https://companion.example', YTDLP_SERVICE_TOKEN: token, RATE_LIMIT_PER_MIN: '1000' },
  outboundService: async request => {
    const u = new URL(request.url);
    calls.push(u.href);
    if (u.hostname === 'api.deezer.com') {
      const id = u.pathname.split('/').at(-1);
      return Response.json({ id: Number(id), title: 'Test', preview: `https://cdn-preview.dzcdn.net/${id}.mp3` });
    }
    if (u.hostname === 'cdn-preview.dzcdn.net') {
      if (u.pathname === '/2.mp3') return new Response('denied', { status: 403 });
      if (u.pathname === '/4.mp3') return new Response(null, { status: 302, headers: { location: 'http://127.0.0.1/private' } });
      if (u.pathname === '/5.mp3') return new Response('x', { headers: { 'content-type': 'audio/mpeg', 'content-length': String(33 * 1024 * 1024) } });
      if (u.pathname === '/3.mp3') {
        const encoder = new TextEncoder();
        const body = new ReadableStream({ start(controller) {
          controller.enqueue(encoder.encode(sample.slice(0, 5)));
          setTimeout(() => { delayedComplete = true; controller.enqueue(encoder.encode(sample.slice(5))); controller.close(); }, 300);
        }});
        return new Response(body, { headers: { 'content-type': 'audio/mpeg', 'content-length': String(sample.length) } });
      }
      return new Response(sample, { headers: { 'content-type': 'audio/mpeg', 'content-length': String(sample.length) } });
    }
    if (u.hostname === 'companion.example') {
      assert.equal(request.headers.get('authorization'), `Bearer ${token}`);
      if (u.pathname.startsWith('/internal/audio/')) return new Response(sample, { headers: { 'content-type': 'audio/mp4', 'content-length': String(sample.length) } });
      if (u.pathname === '/internal/ytdlp') {
        const { args } = await request.json();
        assert.ok(args.includes('--dump-json'));
        return Response.json({ stdout: JSON.stringify({ id: 'abcdefghijk', title: 'Test', channel: 'Artist - Topic', duration: 20 }) });
      }
    }
    if (u.hostname === 'upload.wikimedia.org') return new Response('image', { headers: { 'content-type': u.pathname.endsWith('.svg') ? 'image/svg+xml' : 'image/jpeg', 'content-length': '5' } });
    if (u.hostname === 'api.audius.co' || u.hostname === 'archive.org') return new Response('{}', { status: 404 });
    throw new Error(`Unexpected outbound request: ${u.origin}${u.pathname}`);
  },
});
const fetch = (path, init) => mf.dispatchFetch(`https://glyph.test${path}`, init);
try {
  const cold = await fetch('/v1/audio/deezer/1', { headers: { range: 'bytes=5-9' } });
  assert.equal(cold.status, 206);
  assert.equal(cold.headers.get('content-range'), `bytes 5-9/${sample.length}`);
  assert.equal(await cold.text(), sample.slice(5, 10));
  // Drain the full cache write, then prove the second request does not contact Deezer.
  let warm;
  for (let attempt = 0; attempt < 20; attempt++) {
    warm = await fetch('/v1/audio/deezer/1', { headers: { range: 'bytes=-4' } });
    if (warm.headers.get('x-glyph-cache') === 'HIT') break;
    await warm.arrayBuffer();
    await new Promise(r => setTimeout(r, 10));
  }
  assert.equal(warm.headers.get('x-glyph-cache'), 'HIT');
  assert.equal(await warm.text(), sample.slice(-4));
  const before = calls.length;
  const full = await fetch('/v1/audio/deezer/1');
  assert.equal(await full.text(), sample);
  assert.equal(calls.length, before);
  const head = await fetch('/v1/audio/deezer/1', { method: 'HEAD' });
  assert.equal(head.status, 200);
  assert.equal(await head.text(), '');
  const invalid = await fetch('/v1/audio/deezer/1', { headers: { range: 'bytes=999-' } });
  assert.equal(invalid.status, 416);
  const denied = await fetch('/v1/audio/deezer/2');
  assert.equal(denied.status, 403);
  await denied.arrayBuffer();
  const deniedAgain = await fetch('/v1/audio/deezer/2');
  assert.equal(deniedAgain.headers.get('x-glyph-cache'), null);
  await deniedAgain.arrayBuffer();
  const progressive = await fetch('/v1/audio/deezer/3');
  const reader = progressive.body.getReader();
  const first = await reader.read();
  assert.equal(new TextDecoder().decode(first.value), sample.slice(0, 5));
  assert.equal(delayedComplete, false, 'playback must start before the upstream finishes');
  while (!(await reader.read()).done) {}
  const redirect = await fetch('/v1/audio/deezer/4');
  assert.equal(redirect.status, 502);
  await redirect.arrayBuffer();
  const oversized = await fetch('/v1/audio/deezer/5');
  assert.equal(oversized.status, 413);
  await oversized.arrayBuffer();
  for (const bad of ['http://localhost/secret', 'https://127.0.0.1/', 'https://dzcdn.net.evil.test/', 'https://user:pass@cdn-preview.dzcdn.net/a']) {
    const response = await fetch(`/v1/upstream?url=${encodeURIComponent(bad)}`);
    assert.equal(response.status, 400);
    await response.arrayBuffer();
  }
  const image = await fetch('/v1/upstream?url=' + encodeURIComponent('https://upload.wikimedia.org/a.jpg'));
  assert.equal(await image.text(), 'image');
  const scriptable = await fetch('/v1/upstream?url=' + encodeURIComponent('https://upload.wikimedia.org/evil.svg'));
  assert.equal(scriptable.status, 502);
  await scriptable.arrayBuffer();
  const yt = await fetch('/v1/audio/youtube/abcdefghijk');
  assert.equal(await yt.text(), sample);
  assert.ok(calls.includes('https://companion.example/internal/audio/abcdefghijk'));
  const search = await fetch('/v1/search?q=Artist%20Test');
  assert.match(await search.text(), /"provider":"youtube"/);
  assert.ok(calls.includes('https://companion.example/internal/ytdlp'));
  const tooLarge = await fetch('/v1/resolve', { method: 'POST', body: 'x'.repeat(65537) });
  assert.equal(tooLarge.status, 413);
  await tooLarge.arrayBuffer();
  console.log('Worker integration: cold stream, warm cache, full/suffix ranges, HEAD, denied origins, URL allowlist, companion audio/search, body limit passed.');
} finally {
  await mf.dispose();
}
