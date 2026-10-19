'use strict';
const {spawnSync}=require('node:child_process');
const path=require('node:path');
function docker(args,{capture=false,optional=false}={}) {
  const result=spawnSync('docker',args,{encoding:'utf8',stdio:capture?['ignore','pipe','pipe']:'inherit',maxBuffer:4*1024*1024});
  if(result.error || result.status!==0) {
    if(optional) return null;
    throw new Error(`Docker ${args[0]} failed. Check Docker access and your Compose project; no other services are updated.`);
  }
  return capture?result.stdout.trim():'';
}
function manage(dir,apply,{run=docker}={}) {
  const compose=['compose','--project-directory',path.resolve(dir)];
  const config=JSON.parse(run([...compose,'config','--format','json'],{capture:true}));
  const image=config.services?.companion?.image;
  if(!image) throw new Error('This Compose project needs a companion service with an image reference.');
  let expected;
  if(apply) {
    run([...compose,'pull','companion']);
    expected=run(['image','inspect',image,'--format','{{.Id}}'],{capture:true});
    run([...compose,'up','-d','--no-deps','--wait','companion']);
  }
  const id=run([...compose,'ps','-q','companion'],{capture:true});
  if(!id || id.split(/\s+/).length!==1) throw new Error('Expected exactly one running companion container.');
  const state=JSON.parse(run(['inspect',id,'--format','{{json .}}'],{capture:true}));
  if(!state.State?.Running || state.State.Health && state.State.Health.Status!=='healthy') throw new Error('Companion container is not healthy.');
  if(expected && state.Image!==expected) throw new Error('Running companion image does not match the pulled image; update was not verified.');
  const glyph=run(['exec',id,'glyph','--version'],{capture:true,optional:true})
    || run(['exec',id,'glyph-server','--version'],{capture:true});
  const rdlp=run(['exec',id,'r-dlp','--version'],{capture:true,optional:true});
  const health=state.State.Health?.Status;
  console.log(`Companion: ${glyph}\nr-dlp: ${rdlp||'not installed'}\nImage: ${state.Image}\n${apply?'Pulled configured image is running.':'Container is running.'} Health: ${health||'no healthcheck configured'}`);
  return {glyph,rdlp,image:state.Image,health};
}
module.exports={manage,docker};
