// Run after worker-build --dev --no-opt. No live providers or Cloudflare account.
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
const require = createRequire(import.meta.url);
const { Miniflare } = require('miniflare');
const { readFileSync, readdirSync } = require('node:fs');
async function schema(mf) {
  const db = await mf.getD1Database('GLYPH_DB');
  const dir = new URL('../migrations/', import.meta.url);
  for (const name of readdirSync(dir).filter(n => n.endsWith('.sql')).sort()) {
    const sql = readFileSync(new URL(name, dir), 'utf8').replace(/--[^\n]*/g, '');
    for (const statement of sql.split(';').map(s => s.trim()).filter(Boolean)) await db.prepare(statement).run();
  }
}
const calls = [];
const token = 'test-only-companion-token-32-characters';
const sample = '0123456789abcdefghijklmnopqrstuvwxyz';
let delayedComplete = false;
const options = {
  modules: true,
  d1Databases: ['GLYPH_DB'],
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
      return new Response(sample, { headers: { 'content-type': 'audio/mpeg', 'content-length': String(sample.length), etag: '"test-audio-v1"', 'last-modified': 'Sun, 18 Oct 2026 12:00:00 GMT' } });
    }
    if (u.hostname === 'companion.example') {
      assert.equal(request.headers.get('authorization'), `Bearer ${token}`);
      if (u.pathname.startsWith('/internal/audio/')) return new Response(sample, { headers: { 'content-type': 'audio/mp4', 'content-length': String(sample.length) } });
      if (u.pathname === '/internal/resolve') {
        const input = await request.json();
        assert.equal(input.youtube_only, true);
        if (input.track.id === 'missing') return Response.json({ matched: input.track, source: { kind: 'preview', provider: 'deezer', url: 'https://cdn-preview.dzcdn.net/1.mp3' }, confidence: 1 });
        return Response.json({ matched: { ...input.track, source: 'youtube', id: 'abcdefghijk' }, source: { kind: 'embed', provider: 'youtube', embed_id: 'abcdefghijk' }, confidence: 1 });
      }
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
};
const mf = new Miniflare(options);
await schema(mf);
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
  assert.equal(head.headers.get('etag'), '"test-audio-v1"');
  assert.equal(head.headers.get('last-modified'), 'Sun, 18 Oct 2026 12:00:00 GMT');
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
  const wanted = { id: 'dz-test', source: 'deezer', title: 'Test', artist: 'Artist', duration_ms: 20000 };
  const lookup = await fetch('/v1/download', { method: 'POST', body: JSON.stringify({ track: wanted, priority: ['youtube'], youtube_only: true }) });
  assert.equal(lookup.status, 200);
  const download = await lookup.json();
  assert.equal(download.provider, 'youtube');
  assert.equal(download.url, '/v1/download/youtube/abcdefghijk');
  const downloaded = await fetch(download.url, { headers: { range: 'bytes=5-9' } });
  assert.equal(downloaded.status, 206);
  assert.equal(downloaded.headers.get('content-range'), `bytes 5-9/${sample.length}`);
  assert.equal(await downloaded.text(), sample.slice(5, 10));
  // A companion that ignores the new flag must not leak a preview fallback.
  const missing = await fetch('/v1/download', { method: 'POST', body: JSON.stringify({ track: { ...wanted, id: 'missing', title: 'Missing' }, youtube_only: true }) });
  assert.equal(missing.status, 200);
  assert.equal(await missing.json(), null);
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

// A second private instance exercises the same streaming adapter and its warm cache.
const privateToken = 'private-instance-bearer-at-least-32-characters';
const privateMf = new Miniflare({ ...options, bindings: { ...options.bindings, GLYPH_SERVER_BEARER: privateToken } });
await schema(privateMf);
try {
  const call = (path, headers = {}) => privateMf.dispatchFetch(`https://glyph.test${path}`, { headers });
  const health = await call('/v1/health');
  assert.equal((await health.json()).server_auth_required, true);
  const before = calls.length;
  for (const path of ['/v1/download/youtube/abcdefghijk','/v1/audio/deezer/1','/v1/download/jamendo/42','/v1/upstream?url=https://upload.wikimedia.org/a.jpg','/v1/profile','/v1/sync/export','/v1/access']) {
    const denied = await call(path);
    assert.equal(denied.status,401,path);
    assert.equal((await denied.json()).code,'server_auth_required');
  }
  assert.equal(calls.length,before,'denied clients must never contact upstream');
  const auth = { 'x-glyph-server-authorization': `Bearer ${privateToken}` };
  assert.equal((await call('/v1/access',auth)).status,200);
  const first = await call('/v1/audio/deezer/1',auth);
  assert.equal(await first.text(),sample);
  let hit = false;
  for (let i=0;i<20;i++) {
    const response = await call('/v1/audio/deezer/1',auth);
    hit ||= response.headers.get('x-glyph-cache') === 'HIT';
    await response.arrayBuffer();
    if(hit) break;
    await new Promise(r=>setTimeout(r,10));
  }
  assert.ok(hit,'private media should still cache');
  const cachedDenied = await call('/v1/audio/deezer/1');
  assert.equal(cachedDenied.status,401,'cached media still requires credentials');
  await cachedDenied.arrayBuffer();
  const invalid = await call('/v1/access',{'x-glyph-server-authorization':'Bearer wrong'});
  assert.equal(invalid.status,401);
  await invalid.arrayBuffer();
  console.log('Private Worker: health discovery, invalid/missing token, cache gate and authorized playback passed.');
} finally { await privateMf.dispose(); }

// Fragmented encodes have no Content-Length on their first request. They must
// stream immediately and only publish a chunk manifest after successful EOF.
let variantComplete = false;
let variantCalls = 0;
const variantSample = sample + '\x00\x00\x00\x21freeglyph-variant-complete-v1';
const variantMf = new Miniflare({ ...options, r2Buckets: ['GLYPH_COVERS'], outboundService: async request => {
  const url = new URL(request.url);
  if (url.hostname === 'companion.example' && url.pathname === '/internal/audio/broken00001') {
    return new Response(new ReadableStream({ start(controller) {
      controller.enqueue(new TextEncoder().encode('partial'));
      setTimeout(() => controller.error(new Error('test encoder failure')),50);
    }}), { headers: { 'content-type':'audio/mp4' } });
  }
  if (url.hostname === 'companion.example' && url.pathname === '/internal/audio/streaming01') {
    variantCalls++;
    assert.equal(request.headers.get('x-glyph-audio-quality'), 'low');
    return new Response(new ReadableStream({ start(controller) {
      controller.enqueue(new TextEncoder().encode(sample.slice(0,5)));
      setTimeout(() => { variantComplete = true; controller.enqueue(new TextEncoder().encode(variantSample.slice(5))); controller.close(); },300);
    }}), { headers: { 'content-type':'audio/mp4' } });
  }
  return options.outboundService(request);
}});
await schema(variantMf);
try {
  const url = 'https://glyph.test/v1/audio/youtube/streaming01?quality=low';
  const cold = await variantMf.dispatchFetch(url);
  assert.equal(cold.status,200);
  assert.equal(cold.headers.get('content-length'),null);
  const reader = cold.body.getReader();
  let text = new TextDecoder().decode((await reader.read()).value);
  assert.equal(variantComplete,false,'first bytes must not wait for full encode');
  for (;;) { const {value,done} = await reader.read(); if(done) break; text += new TextDecoder().decode(value); }
  assert.equal(text,variantSample);
  const db = await variantMf.getD1Database('GLYPH_DB');
  let manifest;
  for(let i=0;i<80;i++) {
    manifest = await db.prepare("SELECT manifest FROM audio_variants WHERE track_id='streaming01' AND quality='low'").first();
    if(manifest) break;
    await new Promise(r=>setTimeout(r,25));
  }
  assert.ok(manifest,'unknown-length successful stream must persist');
  const ranged = await variantMf.dispatchFetch(url,{ headers:{range:'bytes=5-9'} });
  assert.equal(ranged.status,206);
  assert.equal(ranged.headers.get('x-glyph-cache'),'R2-CHUNKS');
  assert.equal(await ranged.text(),sample.slice(5,10));
  assert.equal(variantCalls,1,'warm range must not call companion');
  const head = await variantMf.dispatchFetch(url,{method:'HEAD'});
  assert.equal(head.headers.get('content-length'),String(variantSample.length));
  assert.equal(await head.text(),'');
  const broken = await variantMf.dispatchFetch('https://glyph.test/v1/audio/youtube/broken00001?quality=low');
  // Miniflare's Node outbound adapter loses the transport error at EOF. The
  // missing completion atom must still prevent caching this partial output.
  await broken.arrayBuffer().catch(()=>{});
  await new Promise(r=>setTimeout(r,100));
  assert.equal(await db.prepare("SELECT manifest FROM audio_variants WHERE track_id='broken00001'").first(),null,'failed encode must not publish a manifest');
  console.log('Variant Worker: unknown-length streaming, complete R2 publication, warm range, HEAD and failed-stream exclusion passed.');
} finally { await variantMf.dispose(); }

// Account-only mode applies before metadata providers and media cache reads.
const memberMf = new Miniflare({ ...options, bindings: { ...options.bindings,
  REQUIRE_ACCOUNT:'true', PUBLIC_ORIGIN:'https://glyph.test' } });
await schema(memberMf);
try {
  const db = await memberMf.getD1Database('GLYPH_DB');
  const access = 'ga_member-test-access';
  const hash = value => createHash('sha256').update(value).digest('hex');
  const now = Date.now();
  // Legacy saved limits must not disable the new environment setting.
  await db.prepare("INSERT INTO server_settings(key,value) VALUES('limits',?1)").bind(JSON.stringify({
    rate_limit_per_min:1000,youtube_daily_budget_units:9000,import_max_pages:5
  })).run();
  await db.prepare("INSERT INTO users(id,handle,created_at,recovery_hash) VALUES('member','member',?1,'recovery')").bind(now).run();
  await db.prepare("INSERT INTO devices(id,user_id,name,platform,created_at,last_seen,access_hash,access_exp,refresh_hash,refresh_exp) VALUES('device','member','Test','windows',?1,?1,?2,?3,'refresh',?3)")
    .bind(now,hash(access),now+60000).run();
  const call = (path,token) => memberMf.dispatchFetch(`https://glyph.test${path}`,{
    headers:token ? {authorization:`Bearer ${token}`} : {}
  });
  const health = await (await call('/v1/health')).json();
  assert.equal(health.accounts,true);
  assert.equal(health.account_required,true);
  const before = calls.length;
  for(const path of ['/v1/access','/v1/search?q=test','/v1/resolve','/v1/audio/deezer/1',
    '/v1/download/youtube/abcdefghijk','/v1/upstream?url=https://upload.wikimedia.org/a.jpg']) {
    for(const token of [undefined,'forged','refresh']) {
      const denied = await call(path,token);
      assert.equal(denied.status,401,path);
      assert.equal((await denied.json()).code,'account_required');
    }
  }
  assert.equal(calls.length,before,'guests must not contact providers');
  const login = await memberMf.dispatchFetch('https://glyph.test/v1/account/login/start',{
    method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({device_name:'Test',platform:'windows'})
  });
  assert.equal(login.status,200);
  await login.arrayBuffer();
  const valid = await call('/v1/access',access);
  assert.equal(valid.status,200);
  await valid.arrayBuffer();
  const cold = await call('/v1/audio/deezer/1',access);
  assert.equal(await cold.text(),sample);
  let warm = false;
  for(let i=0;i<30;i++) {
    const response = await call('/v1/audio/deezer/1',access);
    warm ||= response.headers.get('x-glyph-cache')==='HIT';
    await response.arrayBuffer();
    if(warm) break;
    await new Promise(r=>setTimeout(r,10));
  }
  assert.ok(warm);
  assert.equal((await call('/v1/audio/deezer/1')).status,401,'warm cache must not bypass account gate');
  await db.prepare("UPDATE devices SET access_exp=?1 WHERE id='device'").bind(now-1).run();
  assert.equal((await call('/v1/access',access)).status,401,'expired access token');
  await db.prepare("UPDATE devices SET access_exp=?1 WHERE id='device'").bind(now+60000).run();
  await db.prepare("DELETE FROM devices WHERE id='device'").run();
  assert.equal((await call('/v1/audio/deezer/1',access)).status,401,'revoked device must lose cached audio access');
  console.log('Account-only Worker: environment/legacy limits, login bootstrap, valid/forged/expired/revoked sessions and warm-cache denial passed.');
} finally { await memberMf.dispose(); }

for(const [saved,environment,status] of [
  [{require_account:false,rate_limit_per_min:1000,youtube_daily_budget_units:9000,import_max_pages:5},'true',200],
  ['invalid saved limits','false',401],
]) {
  const policyMf = new Miniflare({ ...options, bindings:{...options.bindings,
    REQUIRE_ACCOUNT:environment,PUBLIC_ORIGIN:'https://glyph.test'} });
  await schema(policyMf);
  try {
    const db = await policyMf.getD1Database('GLYPH_DB');
    await db.prepare("INSERT INTO server_settings(key,value) VALUES('limits',?1)")
      .bind(typeof saved==='string' ? saved : JSON.stringify(saved)).run();
    const response = await policyMf.dispatchFetch('https://glyph.test/v1/access');
    assert.equal(response.status,status,'explicit saved choice wins; corrupt saved policy must fail closed');
    await response.arrayBuffer();
  } finally { await policyMf.dispose(); }
}
console.log('Account policy: explicit saved disable and corrupt-policy fail-closed behavior passed.');

// Public test password/hash only; never production credentials.
const maintenanceCalls=[];
const adminMf=new Miniflare({...options,bindings:{...options.bindings,
  ADMIN_PASSWORD_HASH:'$argon2id$v=19$m=19456,t=2,p=1$BVPUtwgX342xHD/TpX5nDA$k20JP0Icgx0aK5FiAO/DNKRj/4HDIN2KzI2VL+pZ2Zo'
},outboundService:async request=>{
  const url=new URL(request.url);
  if(url.hostname==='companion.example') {
    assert.equal(request.headers.get('authorization'),`Bearer ${token}`);
    maintenanceCalls.push(url.pathname);
    if(url.pathname==='/internal/status') {
      assert.equal(request.method,'GET');
      const tool={available:true,configured:true,version:'0.1.1'};
      return Response.json({version:'0.3.0',target:'test',enabled:true,ready:true,r_dlp:tool,yt_dlp:tool,ffmpeg:tool});
    }
    assert.equal(request.method,'POST');
    if(url.pathname==='/internal/update/apply') return new Response(`Use the container update path; private token ${token}`,{status:502});
    return Response.json({info:{current:'0.1.0',latest:'0.1.1',update_available:true,message:'test'},
      applied:url.pathname.endsWith('/apply'),restart_required:false,message:'test'});
  }
  return options.outboundService(request);
}});
await schema(adminMf);
try {
  const base='https://glyph.test/admin/v1/';
  for(const endpoint of ['companion/status','companion/r-dlp/apply','companion/update/apply']) {
    const denied=await adminMf.dispatchFetch(base+endpoint,{method:endpoint.endsWith('/status')?'GET':'POST'});
    assert.equal(denied.status,401);await denied.arrayBuffer();
  }
  assert.equal(maintenanceCalls.length,0,'unauthenticated callers must not reach companion management');
  const login=await adminMf.dispatchFetch(base+'login',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({password:'glyph-test-admin-password-only'})});
  assert.equal(login.status,200,await login.clone().text());
  const {access_token}=await login.json();
  const auth={authorization:`Bearer ${access_token}`};
  const status=await adminMf.dispatchFetch(base+'companion/status',{headers:auth});
  assert.equal(status.status,200);assert.equal((await status.json()).r_dlp.version,'0.1.1');
  for(const endpoint of ['companion/update/check','companion/r-dlp/check','companion/r-dlp/apply']) {
    const response=await adminMf.dispatchFetch(base+endpoint,{method:'POST',headers:auth,body:'{}'});
    assert.equal(response.status,200);const value=await response.json();
    assert.equal(value.applied,endpoint.endsWith('/apply'));assert.ok(!JSON.stringify(value).includes(token));
  }
  const failed=await adminMf.dispatchFetch(base+'companion/update/apply',{method:'POST',headers:auth,body:'{}'});
  assert.equal(failed.status,500);
  const detail=await failed.text();assert.ok(!detail.includes(token));assert.ok(detail.includes('[redacted]'));
  assert.deepEqual(maintenanceCalls,['/internal/status','/internal/update/check','/internal/r-dlp/check','/internal/r-dlp/apply','/internal/update/apply']);
  console.log('Companion management: admin gate, fixed private endpoints, versions, update checks/apply and secret isolation passed.');
} finally {await adminMf.dispose();}
