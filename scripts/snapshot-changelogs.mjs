// Copy every Glyph repository's CHANGELOG.md next to the site so /changelog
// lists all releases without fetching other repositories at build time. Run
// before a release (any repository); missing sibling checkouts are skipped.
import {existsSync,mkdirSync,copyFileSync} from 'node:fs';
import {fileURLToPath} from 'node:url';
const root=new URL('../',import.meta.url);
const output=new URL('crates/glyph-server-core/src/site/changelogs/',root);
mkdirSync(output,{recursive:true});
const sources={desktop:'../glyph-apps/',server:'./',companion:'../glyph-companion/','r-dlp':'../glyph-r-dlp/'};
for(const [name,dir] of Object.entries(sources)) {
  const file=new URL(`${dir}CHANGELOG.md`,root);
  if(!existsSync(file)) {console.warn(`skipped ${name}: ${fileURLToPath(file)} not found`);continue;}
  copyFileSync(file,new URL(`${name}.md`,output));
  console.log(`${name}: ${fileURLToPath(file)}`);
}
