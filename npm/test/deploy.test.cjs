const test=require('node:test');
const assert=require('node:assert/strict');
const fs=require('node:fs'),os=require('node:os'),path=require('node:path');
const {template,deploy}=require('../lib/cloudflare.cjs');
test('self-host template has no production IDs, routes or account-specific secrets',()=>{
  const config=template('my-glyph');const text=JSON.stringify(config);
  assert.equal(config.name,'my-glyph');assert.equal(config.vars.PUBLIC_ORIGIN,undefined);
  for(const forbidden of ['glyph.my','database_id','api_key','token','routes']) assert.ok(!text.includes(forbidden));
  assert.throws(()=>template('bad name'));assert.throws(()=>template('my-glyph','http://example.com'));
  assert.throws(()=>template('my-glyph','https://user:secret@example.com'));
});
test('deployment migrates before publishing, detects own URL and verifies version/accounts',async()=>{
  const dir=fs.mkdtempSync(path.join(os.tmpdir(),'glyph-deploy-test-'));
  const file=path.join(dir,'wrangler.json');const config=template('my-glyph');
  config.d1_databases=[{binding:'GLYPH_DB',database_id:'own-db'}];fs.writeFileSync(file,JSON.stringify(config));
  const calls=[];
  const run=args=>{calls.push(args);return 'Deployed https://my-glyph.own-account.workers.dev';};
  try {
    await deploy(file,{run,copy:()=>{},healthFetch:async url=>{
      assert.equal(url,'https://my-glyph.own-account.workers.dev/v1/health');
      return Response.json({server_version:'0.4.1',accounts:true});
    }});
    assert.deepEqual(calls[0].slice(0,4),['d1','migrations','apply','GLYPH_DB']);
    assert.equal(calls.filter(args=>args[0]==='deploy').length,2);
    assert.equal(JSON.parse(fs.readFileSync(file)).vars.PUBLIC_ORIGIN,'https://my-glyph.own-account.workers.dev');
    await assert.rejects(deploy(file,{copy:()=>{},run:()=>{throw new Error('migration failed');}}),/migration failed/);
    await assert.rejects(deploy(file,{copy:()=>{},run,healthFetch:async()=>Response.json({server_version:'0.2.6',accounts:true})}),/does not match/);
  } finally {fs.rmSync(dir,{recursive:true,force:true});}
});
test('cloudflare options edit wrangler.json vars, domains and secrets without leaking values',()=>{
  const {configure}=require('../lib/cloudflare.cjs');
  const dir=fs.mkdtempSync(path.join(os.tmpdir(),'glyph-cf-config-'));
  const file=path.join(dir,'wrangler.json');fs.writeFileSync(file,JSON.stringify(template('my-glyph')));
  const logs=[],calls=[];const log=line=>logs.push(line);const run=args=>{calls.push(args);return '';};
  try {
    configure(file,'set','rate_limit_per_min','120',{run,log});
    configure(file,'set','REQUIRE_ACCOUNT','true',{run,log});
    assert.throws(()=>configure(file,'set','RATE_LIMIT_PER_MIN','fast',{run,log}),/invalid value/);
    assert.throws(()=>configure(file,'set','LASTFM_API_KEY','x',{run,log}),/is a secret/);
    assert.throws(()=>configure(file,'set','PUBLIC_ORIGIN','http://insecure.example',{run,log}),/invalid value/);
    configure(file,'domain','Glyph.Example.com',{run,log});
    const config=JSON.parse(fs.readFileSync(file));
    assert.equal(config.vars.RATE_LIMIT_PER_MIN,'120');assert.equal(config.vars.REQUIRE_ACCOUNT,'true');
    assert.equal(config.vars.PUBLIC_ORIGIN,'https://glyph.example.com');
    assert.deepEqual(config.routes,[{pattern:'glyph.example.com',custom_domain:true}]);
    assert.throws(()=>configure(file,'domain','https://x.example/path',{run,log}));
    configure(file,'unset','REQUIRE_ACCOUNT',undefined,{run,log});
    assert.equal(JSON.parse(fs.readFileSync(file)).vars.REQUIRE_ACCOUNT,undefined);
    configure(file,'secret','lastfm_api_key',undefined,{run,log});
    assert.deepEqual(calls,[['secret','put','LASTFM_API_KEY','--config',file]]);
    assert.throws(()=>configure(file,'secret','RATE_LIMIT_PER_MIN',undefined,{run,log}),/plain value/);
    configure(file,'list',undefined,undefined,{run,log});
    assert.ok(logs.some(l=>l.startsWith('RATE_LIMIT_PER_MIN = 120')));
  } finally {fs.rmSync(dir,{recursive:true,force:true});}
});
