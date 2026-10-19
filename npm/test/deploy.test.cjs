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
      return Response.json({server_version:'0.3.0',accounts:true});
    }});
    assert.deepEqual(calls[0].slice(0,4),['d1','migrations','apply','GLYPH_DB']);
    assert.equal(calls.filter(args=>args[0]==='deploy').length,2);
    assert.equal(JSON.parse(fs.readFileSync(file)).vars.PUBLIC_ORIGIN,'https://my-glyph.own-account.workers.dev');
    await assert.rejects(deploy(file,{copy:()=>{},run:()=>{throw new Error('migration failed');}}),/migration failed/);
    await assert.rejects(deploy(file,{copy:()=>{},run,healthFetch:async()=>Response.json({server_version:'0.2.6',accounts:true})}),/does not match/);
  } finally {fs.rmSync(dir,{recursive:true,force:true});}
});
