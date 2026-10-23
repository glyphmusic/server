const test=require('node:test'),assert=require('node:assert/strict');
const {spawnSync}=require('node:child_process');
const path=require('node:path');
test('npm-only help never invokes installers, updates or deployment tools',()=>{
  for(const args of [['update','cli','--help'],['self-update','--help'],['setup','cloudflare','--help'],['deploy','--help'],['cloudflare','--help']]) {
    const result=spawnSync(process.execPath,[path.resolve(__dirname,'../bin/glyph.cjs'),...args],{
      encoding:'utf8',timeout:10000,env:{...process.env,PATH:'',GLYPH_CONFIG:'missing-on-purpose.toml'}
    });
    assert.equal(result.status,0,result.stderr);assert.match(result.stdout,/glyph/);
    assert.match(result.stdout,/https:\/\/docs\.glyph\.my\/self-hosting\/cli\.md/);
  }
});
