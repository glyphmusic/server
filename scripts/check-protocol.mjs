// CI: the protocol snapshot must be an unmodified copy of glyphmusic/apps at
// the commit in release-protocol/SOURCE, so both sides build from the same
// wire types and nobody edits the server's copy by hand.
import {readFileSync,readdirSync} from 'node:fs';
const root=new URL('../',import.meta.url);
const rev=readFileSync(new URL('release-protocol/SOURCE',root),'utf8').trim();
if(!/^[0-9a-f]{40}$/.test(rev)) throw new Error('release-protocol/SOURCE must hold a full apps commit hash');
const repo=process.env.GLYPH_APPS_REPO||'glyphmusic/apps';
const headers={'user-agent':'glyph-protocol-check',...(process.env.GH_TOKEN?{authorization:`Bearer ${process.env.GH_TOKEN}`}:{})};
const listing=await fetch(`https://api.github.com/repos/${repo}/contents/crates/glyph-protocol/src?ref=${rev}`,{headers});
if(!listing.ok) throw new Error(`Cannot list ${repo}@${rev}: HTTP ${listing.status}`);
const upstream=(await listing.json()).map(f=>f.name).filter(n=>n.endsWith('.rs')).sort();
const local=readdirSync(new URL('release-protocol/src/',root)).filter(n=>n.endsWith('.rs')).sort();
if(JSON.stringify(upstream)!==JSON.stringify(local)) throw new Error(`Files differ: apps has ${upstream.join(', ')}; snapshot has ${local.join(', ')}`);
for(const name of local){
  const r=await fetch(`https://raw.githubusercontent.com/${repo}/${rev}/crates/glyph-protocol/src/${name}`,{headers});
  if(!r.ok) throw new Error(`Cannot fetch ${name}: HTTP ${r.status}`);
  // Git may check text out with CRLF on Windows; compare source content.
  if((await r.text()).replace(/\r\n/g,'\n')!==readFileSync(new URL(`release-protocol/src/${name}`,root),'utf8').replace(/\r\n/g,'\n')) throw new Error(`release-protocol/src/${name} differs from ${repo}@${rev}; run scripts/snapshot-protocol.mjs`);
}
console.log(`Protocol snapshot matches ${repo}@${rev} (${local.length} files).`);
