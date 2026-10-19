const test=require('node:test'),assert=require('node:assert/strict');
const {manage}=require('../lib/companion.cjs');
function runner(calls,{wrong=false,unhealthy=false}={}) {
  return args=>{
    calls.push(args);
    if(args.includes('config')) return JSON.stringify({services:{companion:{image:'own-image:latest',environment:{TOKEN:'secret-must-not-be-logged'}}}});
    if(args[0]==='image') return 'sha256:expected';
    if(args.includes('ps')) return 'container';
    if(args[0]==='inspect') return JSON.stringify({Image:wrong?'sha256:old':'sha256:expected',State:{Running:true,Health:{Status:unhealthy?'unhealthy':'healthy'}}});
    if(args.includes('glyph')) return 'glyph 0.3.0';
    if(args.includes('r-dlp')) return '0.1.1';
    return '';
  };
}
test('check is read-only and update touches only companion and verifies pulled image',()=>{
  const checks=[];manage('own compose project',false,{run:runner(checks)});
  assert.ok(!checks.some(args=>args.includes('pull')||args.includes('up')));
  const updates=[];const result=manage('own compose project',true,{run:runner(updates)});
  assert.equal(result.rdlp,'0.1.1');
  assert.equal(updates.find(args=>args.includes('pull')).at(-1),'companion');
  assert.ok(updates.find(args=>args.includes('up')).includes('--no-deps'));
  assert.throws(()=>manage('own compose project',true,{run:runner([],{wrong:true})}),/does not match/);
  assert.throws(()=>manage('own compose project',false,{run:runner([],{unhealthy:true})}),/not healthy/);
});
