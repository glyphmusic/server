#!/usr/bin/env node
'use strict';
const {spawnSync} = require('node:child_process');
const path = require('node:path');
// Same reference the native `glyph --help` ends with.
const DOCS='\nDocs: https://docs.glyph.my/self-hosting/cli\nMarkdown (for AI agents): https://docs.glyph.my/self-hosting/cli.md';
async function main() {
  const args = process.argv.slice(2);
  if((args[0]==='update' && ['cli','--help',undefined].includes(args[1]) || args[0]==='self-update') && args.includes('--help')) {
    console.log('glyph update [cli] [--check]\nUpdates the npm CLI package. Running servers are left running.'+DOCS);return;
  }
  if(['version','check','update'].includes(args[0]) && ['companion','r-dlp'].includes(args[1]) && args.includes('--compose-dir')) {
    if(args.length!==4 || args[2]!=='--compose-dir' || !args[3]) throw new Error('Usage: glyph check|version|update companion|r-dlp --compose-dir /path/to/project');
    require('../lib/companion.cjs').manage(args[3],args[0]==='update');return;
  }
  if ((args[0] === 'update' && (!args[1] || args[1] === 'cli')) || args[0] === 'self-update') {
    const offset=args[1]==='cli'?2:1;
    if(args.slice(offset).some(arg=>arg!=='--check') || args.slice(offset).length>1) throw new Error('Usage: glyph update [cli] [--check]');
    const check = args.includes('--check');
    if (check) {
      const result = process.platform === 'win32'
        ? spawnSync(process.env.ComSpec || 'cmd.exe',['/d','/s','/c','npm view glyph-server version'],{stdio:'inherit'})
        : spawnSync('npm',['view','glyph-server','version'],{stdio:'inherit'});
      if (result.error) throw result.error;
      console.log(`Installed CLI: ${require('../package.json').version}`);
      process.exitCode = result.status ?? 1; return;
    }
    // No native child is running during replacement. Existing servers use
    // their versioned cache copy and keep running until explicitly restarted.
    const result = process.platform === 'win32'
      ? spawnSync(process.env.ComSpec || 'cmd.exe',['/d','/s','/c','npm install --global glyph-server@latest'],{stdio:'inherit'})
      : spawnSync('npm',['install','--global','glyph-server@latest'],{stdio:'inherit'});
    if (result.error) throw result.error;
    if (result.status === 0) {
      const file=await require('../lib/install.cjs').install();
      const verified=spawnSync(file,['--version'],{stdio:'inherit'});
      if(verified.status!==0) throw new Error('npm finished, but the installed CLI could not be verified');
      console.log('CLI updated and verified. Running servers/companions retain their current version until restarted.');
    }
    process.exitCode = result.status ?? 1; return;
  }
  if (args[0] === 'setup' && args[1] === 'cloudflare') {
    if(args.includes('--help')) {console.log('glyph setup cloudflare [--dir project-directory]\nCreates your own Cloudflare resources and deploys the included Worker.'+DOCS);return;}
    if(args.length!==2 && !(args.length===4 && args[2]==='--dir')) throw new Error('Usage: glyph setup cloudflare [--dir project-directory]');
    const readline=require('node:readline/promises').createInterface({input:process.stdin,output:process.stdout});
    const question=text=>readline.question(text); question.close=()=>readline.close();
    try {await require('../lib/cloudflare.cjs').setup({dir:args[3]?path.resolve(args[3]):undefined,question});}
    finally {readline.close();}
    return;
  }
  // `glyph config ... --worker-config wrangler.json`: the Worker's wrangler.json
  // under the same command as server.toml and live settings (--server).
  if (args[0] === 'config' && args.includes('--worker-config')) {
    const rest=args.slice(1);const at=rest.indexOf('--worker-config');
    if(!rest[at+1]) throw new Error('--worker-config needs a path');
    const file=rest[at+1];rest.splice(at,2);
    const [action,name,...value]=rest;
    const map={list:'list',get:'list',set:'set',unset:'unset'};
    if(!map[action] || (action==='set' && value.length!==1) || (action==='unset' && !name)) throw new Error('Usage: glyph config list|set <NAME> <value>|unset <NAME> --worker-config wrangler.json');
    require('../lib/cloudflare.cjs').configure(file,map[action],name,value[0]);return;
  }
  if (args[0] === 'cloudflare') {
    const cf=require('../lib/cloudflare.cjs');
    if(args.length<2 || args.includes('--help') || args[1]==='help') {console.log(cf.configHelp()+DOCS);return;}
    const rest=args.slice(1);const at=rest.indexOf('--config');
    let file=require('node:fs').existsSync('wrangler.json')?'wrangler.json':'glyph-cloudflare/wrangler.json';
    if(at>=0) {if(!rest[at+1]) throw new Error('--config needs a path');file=rest[at+1];rest.splice(at,2);}
    const [action,name,...value]=rest;
    const valid=action==='list'?rest.length===1:action==='set'?Boolean(name)&&value.length===1:['unset','secret','domain'].includes(action)?Boolean(name)&&value.length===0:false;
    if(!valid) throw new Error('Usage:\n'+cf.configHelp());
    cf.configure(file,action,name,value[0]);return;
  }
  if (args[0] === 'deploy' || (args[0] === 'update' && args[1] === 'server' && args.includes('--worker-config'))) {
    const flag=args[0]==='deploy'?'--config':'--worker-config';
    const index=args.indexOf(flag);
    if(args.includes('--help')) {console.log('glyph deploy [--config wrangler.json]\nglyph update server --worker-config wrangler.json'+DOCS);return;}
    const offset=args[0]==='deploy'?1:2;
    if(args.length!==offset && !(args.length===offset+2 && index===offset && args[index+1])) throw new Error(`Usage: glyph deploy --config /path/to/wrangler.json`);
    const fallback=require('node:fs').existsSync('wrangler.json')?'wrangler.json':'glyph-cloudflare/wrangler.json';
    await require('../lib/cloudflare.cjs').deploy(args[index+1] && index>=0 ? args[index+1] : fallback);
    return;
  }
  const file = await require('../lib/install.cjs').install();
  const result = spawnSync(file,args,{stdio:'inherit',env:{...process.env,GLYPH_NPM_MANAGED:'1'}});
  if (result.error) throw result.error;
  process.exitCode = result.status ?? 1;
}
main().catch(error => {console.error(`glyph: ${error.message}`);process.exitCode=1;});
