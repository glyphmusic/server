'use strict';
const fs = require('node:fs');
const path = require('node:path');
const root = path.resolve(__dirname,'../..');
for (const name of ['LICENSE-MIT','LICENSE-APACHE']) fs.copyFileSync(path.join(root,name),path.resolve(__dirname,'..',name));
const worker = path.join(root,'crates/glyph-server-worker/build');
fs.mkdirSync(path.resolve(__dirname,'../worker'),{recursive:true});
for (const name of ['index.js','index_bg.wasm']) {
  if (!fs.existsSync(path.join(worker,name))) throw new Error('Build the Worker before packing the npm release');
  fs.copyFileSync(path.join(worker,name),path.resolve(__dirname,'../worker',name));
}

fs.cpSync(path.join(root,'crates/glyph-server-worker/migrations'),path.resolve(__dirname,'../worker/migrations'),{recursive:true});
fs.writeFileSync(path.resolve(__dirname,'../worker/package.json'),JSON.stringify({type:'module'}));
