'use strict';
const fs = require('node:fs');
const path = require('node:path');
const {spawnSync} = require('node:child_process');
const pkg = require('../package.json');
function wrangler(args,{capture=false}={}) {
  let command='npx', prefix=[];
  if(process.platform==='win32') {
    const roots=[path.dirname(process.execPath),...(process.env.PATH||'').split(path.delimiter)];
    const candidates=roots.map(root=>path.join(root,'node_modules/npm/bin/npx-cli.js'));
    if(process.env.npm_execpath) candidates.unshift(path.join(path.dirname(process.env.npm_execpath),'npx-cli.js'));
    const script=candidates.find(file=>fs.existsSync(file));
    if(!script) throw new Error('Cannot locate npx. Install Node.js with npm and reopen your terminal.');
    command=process.execPath;prefix=[script];
  }
  const result=spawnSync(command,[...prefix,'--yes','wrangler@4',...args],{
    stdio:capture?['inherit','pipe','inherit']:'inherit',encoding:'utf8',maxBuffer:4*1024*1024
  });
  if(result.error) throw result.error;
  if(result.status!==0) throw new Error(`Wrangler ${args.slice(0,2).join(' ')} failed. Your configuration/resources were retained; fix the reported error and rerun.`);
  if(capture) {process.stdout.write(result.stdout);return result.stdout;}
  return '';
}
function template(name,origin) {
  if(!/^[a-z][a-z0-9-]{2,47}$/.test(name)) throw new Error('Worker name: 3–48 lowercase letters, digits or hyphens; start with a letter.');
  if(origin) {const u=new URL(origin);if(u.protocol!=='https:' || u.username || u.password || u.pathname!=='/' || u.search || u.hash) throw new Error('Use a public HTTPS origin without a path or credentials.');origin=u.origin;}
  return {name,main:'.glyph/worker/index.js',compatibility_date:'2025-01-01',workers_dev:true,
    ...(origin && !new URL(origin).hostname.endsWith('.workers.dev')?{routes:[{pattern:new URL(origin).hostname,custom_domain:true}]}:{}),
    vars:{RATE_LIMIT_PER_MIN:'60',...(origin?{PUBLIC_ORIGIN:origin}:{})},
    triggers:{crons:['* * * * *']}};
}
function copyPayload(dir) {
  const source=path.resolve(__dirname,'../worker');
  if(!fs.existsSync(path.join(source,'index_bg.wasm'))) throw new Error('Prebuilt Worker missing. Install an official npm release or run the repository prepack build.');
  const output=path.join(dir,'.glyph');fs.mkdirSync(output,{recursive:true});
  fs.cpSync(source,path.join(output,'worker'),{recursive:true});
}
function readConfig(file) {
  try{return JSON.parse(fs.readFileSync(file,'utf8'));}catch {throw new Error(`Cannot read generated JSON config ${file}; use a valid wrangler.json.`);}
}
async function setup({dir=path.resolve('glyph-cloudflare'),question,run=wrangler}={}) {
  const config=path.join(dir,'wrangler.json');
  const existing=fs.existsSync(config)?readConfig(config):null;
  const name=existing?.name || (await question('Worker name [glyph-api]: ')).trim() || 'glyph-api';
  const origin=existing?.vars?.PUBLIC_ORIGIN || (await question('Public HTTPS origin (Enter: detect workers.dev after deployment): ')).trim();
  const data=existing || template(name,origin);
  const r2=!existing && /^y(es)?$/i.test((await question('Enable R2 audio/image storage? Requires R2 enabled in YOUR Cloudflare account [y/N]: ')).trim());
  console.log(`This creates a Worker and D1 database in YOUR Cloudflare account, writes ${config}, and deploys Glyph ${pkg.version}.`);
  if(!/^y(es)?$/i.test((await question('Continue? [y/N]: ')).trim())) return;
  // Release terminal input before Wrangler's own login and secret prompts.
  question.close?.();
  fs.mkdirSync(dir,{recursive:true});copyPayload(dir);
  if(!existing) fs.writeFileSync(config,JSON.stringify(data,null,2)+'\n',{flag:'wx'});
  const gitignore=path.join(dir,'.gitignore');
  if(!fs.existsSync(gitignore)) fs.writeFileSync(gitignore,'.glyph/\n.wrangler/\n.dev.vars\n.env\n');
  const common=['--config',config];
  run(['login',...common]);
  if(!readConfig(config).d1_databases?.some(db=>db.binding==='GLYPH_DB')) {
    run(['d1','create',`${name}-accounts`,'--binding','GLYPH_DB','--update-config',...common]);
  }
  const current=readConfig(config);
  const db=current.d1_databases?.find(db=>db.binding==='GLYPH_DB');
  if(!db?.database_id) throw new Error('Wrangler did not bind GLYPH_DB; add the reported database ID to wrangler.json and rerun.');
  db.migrations_dir='.glyph/worker/migrations';
  fs.writeFileSync(config,JSON.stringify(current,null,2)+'\n');
  if(r2) run(['r2','bucket','create',`${name}-media`,'--binding','GLYPH_COVERS','--update-config',...common]);
  // Existing command prompts without echo and computes Argon2 locally; no
  // plaintext admin password is persisted in the deployment project.
  const binary=await require('./install.cjs').install();
  const hashed=spawnSync(binary,['hash-password'],{stdio:['inherit','pipe','inherit'],encoding:'utf8'});
  if(hashed.status!==0 || !hashed.stdout.trim().startsWith('$argon2')) throw new Error('Admin password hashing failed; rerun setup.');
  const hashFile=path.join(dir,'.glyph',`admin-hash-${require('node:crypto').randomUUID()}.json`);
  let created=false;
  try {
    fs.writeFileSync(hashFile,JSON.stringify({ADMIN_PASSWORD_HASH:hashed.stdout.trim()}),{mode:0o600,flag:'wx'});
    created=true;
    run(['secret','bulk',hashFile,...common]);
  } finally {if(created) fs.rmSync(hashFile,{force:true});}
  await deploy(config,{run});
}
async function deploy(file,{run=wrangler,copy=copyPayload,healthFetch=fetch}={}) {
  file=path.resolve(file);const dir=path.dirname(file);
  copy(dir);
  // Generated projects use the bundled migrations. For existing TOML projects,
  // Wrangler uses the owner's explicitly configured migrations directory.
  if(path.basename(file)==='wrangler.json') {
    const config=readConfig(file);
    const db=config.d1_databases?.find(db=>db.binding==='GLYPH_DB');
    if(db) db.migrations_dir='.glyph/worker/migrations';
    config.main='.glyph/worker/index.js';delete config.build;
    fs.writeFileSync(file,JSON.stringify(config,null,2)+'\n');
  }
  run(['d1','migrations','apply','GLYPH_DB','--remote','--config',file]);
  let output=run(['deploy',path.join(dir,'.glyph/worker/index.js'),'--config',file],{capture:true});
  let config=path.basename(file)==='wrangler.json'?readConfig(file):null;
  let origin=config?.vars?.PUBLIC_ORIGIN;
  if(config && !origin) {
    origin=output.match(/https:\/\/[a-z0-9.-]+\.workers\.dev\b/i)?.[0];
    if(!origin) throw new Error('Deployment completed; set PUBLIC_ORIGIN to its HTTPS URL in wrangler.json and run glyph deploy again to enable accounts.');
    config.vars={...config.vars,PUBLIC_ORIGIN:origin};fs.writeFileSync(file,JSON.stringify(config,null,2)+'\n');
    run(['deploy',path.join(dir,'.glyph/worker/index.js'),'--config',file]);
  }
  if(origin) {
    const response=await healthFetch(`${origin}/v1/health`,{signal:AbortSignal.timeout(15000)});
    const health=await response.json();
    if(!response.ok || health.server_version!==pkg.version) throw new Error(`Deployed, but running version ${health.server_version||'unknown'} does not match ${pkg.version}; verify before retrying.`);
    if(config?.d1_databases?.some(db=>db.binding==='GLYPH_DB') && !health.accounts) throw new Error('Deployed, but accounts are unavailable. Check PUBLIC_ORIGIN and GLYPH_DB before using this instance.');
    console.log(`Verified Glyph ${health.server_version}: ${origin}\nNext: glyph remote login ${origin}`);
  } else console.log('Deployment finished. Check your configured public URL with glyph version server --server <url>.');
}
module.exports={wrangler,template,setup,deploy,copyPayload};
