'use strict';
const fs = require('node:fs');
const path = require('node:path');
const os = require('node:os');
const zlib = require('node:zlib');
const crypto = require('node:crypto');
const { spawnSync } = require('node:child_process');
const pkg = require('../package.json');
const MAX = 128 * 1024 * 1024;
// Glyph release key (minisign); the same list as glyph_protocol::release.
const RELEASE_KEYS = ['RWQ2xWZMYa2l251YUhrekvWRC+kaD698lXz0itKGjvNYAauLRTWCzg9o'];
const ED25519_SPKI = Buffer.from('302a300506032b6570032100','hex');

// Checks a minisign signature of `data` against `keys` and the expected
// trusted comment. Throws with a reason; returns nothing when it passes.
function verifySignature(data, minisig, keys, comment) {
  const lines = minisig.toString().split(/\r?\n/);
  if (lines.length < 4 || !lines[0].startsWith('untrusted comment:') || !lines[2].startsWith('trusted comment: ')) throw new Error('Release signature cannot be read');
  const sig = Buffer.from(lines[1],'base64');
  const trusted = lines[2].slice('trusted comment: '.length);
  const global = Buffer.from(lines[3],'base64');
  if (trusted !== comment) throw new Error(`Release signature is for "${trusted}", not "${comment}"`);
  const alg = sig.subarray(0,2).toString('latin1');
  if (sig.length !== 74 || global.length !== 64 || (alg !== 'ED' && alg !== 'Ed')) throw new Error('Release signature cannot be read');
  const message = alg === 'ED' ? crypto.createHash('blake2b512').update(data).digest() : data;
  for (const k of keys) {
    const pk = Buffer.from(k,'base64');
    if (pk.length !== 42 || !pk.subarray(2,10).equals(sig.subarray(2,10))) continue;
    const key = crypto.createPublicKey({key:Buffer.concat([ED25519_SPKI,pk.subarray(10)]),format:'der',type:'spki'});
    if (crypto.verify(null,message,key,sig.subarray(10)) && crypto.verify(null,Buffer.concat([sig.subarray(10),Buffer.from(trusted)]),key,global)) return;
  }
  throw new Error('Checksum list is not signed by the Glyph release key; installation aborted');
}

function target(platform = process.platform, arch = process.arch) {
  const targets = { 'win32-x64': 'x86_64-pc-windows-msvc', 'linux-x64': 'x86_64-unknown-linux-gnu',
    'linux-arm64': 'aarch64-unknown-linux-gnu', 'darwin-x64': 'x86_64-apple-darwin', 'darwin-arm64': 'aarch64-apple-darwin' };
  const value = targets[`${platform}-${arch}`];
  if (!value) throw new Error(`Unsupported platform ${platform}/${arch}. Download a compatible Glyph release instead.`);
  if (platform === process.platform && platform === 'linux' && process.report?.getReport().header.glibcVersionRuntime === undefined) {
    throw new Error('This Glyph build needs glibc Linux; Alpine/musl is not supported.');
  }
  return value;
}
function checksum(text, name) {
  const entries = text.trim().split(/\r?\n/).map(line => line.match(/^([a-fA-F0-9]{64})\s+\*?(.+)$/))
    .filter(m => m && m[2] === name);
  if (entries.length !== 1) throw new Error(`No unique SHA-256 checksum for ${name}`);
  return entries[0][1].toLowerCase();
}
// Parse into memory, never extract archive paths onto the filesystem.
function unpack(bytes, zip, names) {
  if (zip) {
    let eocd = -1;
    for (let i = bytes.length - 22; i >= Math.max(0,bytes.length - 65557); i--) {
      if (bytes.readUInt32LE(i) === 0x06054b50) { eocd = i; break; }
    }
    if (eocd < 0) throw new Error('Invalid ZIP archive');
    let at = bytes.readUInt32LE(eocd + 16);
    for (let n = 0; n < bytes.readUInt16LE(eocd + 10); n++) {
      if (at + 46 > bytes.length || bytes.readUInt32LE(at) !== 0x02014b50) throw new Error('Invalid ZIP directory');
      const nameLen = bytes.readUInt16LE(at + 28);
      const name = bytes.subarray(at + 46,at + 46 + nameLen).toString();
      const method = bytes.readUInt16LE(at + 10);
      const size = bytes.readUInt32LE(at + 24);
      const compressed = bytes.readUInt32LE(at + 20);
      const local = bytes.readUInt32LE(at + 42);
      if (names.includes(name)) {
        if (size > MAX || local + 30 > bytes.length || bytes.readUInt32LE(local) !== 0x04034b50) throw new Error('Invalid ZIP member');
        const start = local + 30 + bytes.readUInt16LE(local + 26) + bytes.readUInt16LE(local + 28);
        if (start + compressed > bytes.length) throw new Error('Truncated ZIP member');
        const raw = bytes.subarray(start,start + compressed);
        const result = method === 0 ? raw : method === 8 ? zlib.inflateRawSync(raw,{maxOutputLength:MAX}) : null;
        if (!result || result.length !== size) throw new Error('Unsupported or invalid ZIP member');
        return result;
      }
      at += 46 + nameLen + bytes.readUInt16LE(at + 30) + bytes.readUInt16LE(at + 32);
    }
  } else {
    const tar = zlib.gunzipSync(bytes,{maxOutputLength:MAX});
    for (let at = 0; at + 512 <= tar.length;) {
      const name = tar.subarray(at,at + 100).toString().split('\0')[0].replace(/^\.\//,'');
      const prefix = tar.subarray(at + 345,at + 500).toString().split('\0')[0];
      const size = parseInt(tar.subarray(at + 124,at + 136).toString().replace(/\0.*$/,'').trim() || '0',8);
      if (!Number.isSafeInteger(size) || size < 0 || at + 512 + size > tar.length) throw new Error('Invalid TAR member');
      if (!prefix && names.includes(name) && [0,48].includes(tar[at + 156])) return tar.subarray(at + 512,at + 512 + size);
      at += 512 + Math.ceil(size / 512) * 512;
    }
  }
  throw new Error('Glyph executable missing from release archive');
}
async function download(url, max = MAX) {
  const response = await fetch(url,{signal:AbortSignal.timeout(120000)});
  if (!response.ok) throw new Error(`Release download failed (HTTP ${response.status}). This CLI version must have a published GitHub release.`);
  const chunks = []; let size = 0;
  for await (const bytes of response.body) {
    size += bytes.length;
    if (size > max) throw new Error('Release download exceeds size limit');
    chunks.push(bytes);
  }
  return Buffer.concat(chunks);
}
function executable() {
  const base = process.platform === 'win32' ? (process.env.LOCALAPPDATA || os.homedir()) : (process.env.XDG_CACHE_HOME || path.join(os.homedir(),'.cache'));
  return path.join(base,'glyph','cli',pkg.version,target(),process.platform === 'win32' ? 'glyph.exe' : 'glyph');
}
function verify(file, expected = pkg.version) {
  const result = spawnSync(file,['--version'],{encoding:'utf8',timeout:10000,windowsHide:true});
  return result.status === 0 && result.stdout.trim() === `glyph ${expected}`;
}
async function install({keys = RELEASE_KEYS} = {}) {
  const file = executable();
  if (fs.existsSync(file) && verify(file)) return file;
  const name = `glyph-server-${pkg.version}-${target()}.${process.platform === 'win32' ? 'zip' : 'tar.gz'}`;
  const base = `https://github.com/glyphmusic/server/releases/download/v${pkg.version}`;
  const [archive, manifest, signature] = await Promise.all([download(`${base}/${name}`),download(`${base}/SHA256SUMS.txt`,65536),download(`${base}/SHA256SUMS.txt.minisig`,4096)]);
  verifySignature(manifest,signature,keys,`glyph-release repo=glyphmusic/server tag=v${pkg.version}`);
  const hash = crypto.createHash('sha256').update(archive).digest('hex');
  if (hash !== checksum(manifest.toString(),name)) throw new Error('SHA-256 mismatch; installation aborted');
  const binary = unpack(archive,process.platform === 'win32',['glyph','glyph.exe','glyph-server','glyph-server.exe']);
  fs.mkdirSync(path.dirname(file),{recursive:true,mode:0o700});
  const staged = `${file}.${process.pid}.${crypto.randomBytes(6).toString('hex')}.new${process.platform === 'win32' ? '.exe' : ''}`;
  try {
    fs.writeFileSync(staged,binary,{mode:0o755,flag:'wx'});
    if (!verify(staged)) throw new Error('Downloaded executable has the wrong Glyph version');
    fs.renameSync(staged,file);
  } finally { fs.rmSync(staged,{force:true}); }
  return file;
}
module.exports = {target,checksum,unpack,download,executable,verify,verifySignature,install,RELEASE_KEYS};
