// Copy the built web player (glyph-apps/glyph-ui: `npm run build:web` ->
// dist-web) into glyph-web/, the Worker's static assets (wrangler.toml
// [assets]) served at glyph.my/web. Run before `wrangler deploy`. Pass
// --build to run the build first. The copied files are not committed.
import {cpSync,existsSync,readdirSync,rmSync} from 'node:fs';
import {execSync} from 'node:child_process';
import {fileURLToPath} from 'node:url';
const root=new URL('../',import.meta.url);
const ui=new URL('../glyph-apps/glyph-ui/',root);
const built=new URL('dist-web/',ui);
const output=new URL('glyph-web/',root);
const keep=new Set(['README.md','.assetsignore','.gitignore']);
if(process.argv.includes('--build')) execSync('npm run build:web',{cwd:fileURLToPath(ui),stdio:'inherit'});
if(!existsSync(new URL('index.html',built))) {
  console.error(`no web player at ${fileURLToPath(built)}: run \`npm run build:web\` in glyph-apps/glyph-ui (or pass --build)`);
  process.exit(1);
}
for(const name of readdirSync(output)) if(!keep.has(name)) rmSync(new URL(name,output),{recursive:true,force:true});
cpSync(fileURLToPath(built),fileURLToPath(output),{recursive:true});
console.log(`web player: ${fileURLToPath(built)} -> ${fileURLToPath(output)}`);
