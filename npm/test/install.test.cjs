const test=require('node:test');
const assert=require('node:assert/strict');
const zlib=require('node:zlib');
const fs=require('node:fs');
const os=require('node:os');
const path=require('node:path');
const crypto=require('node:crypto');
const {target,checksum,unpack,install,executable}=require('../lib/install.cjs');
function tar(name,data,type=48) {
  const header=Buffer.alloc(512);header.write(name);header.write(data.length.toString(8),124);header[156]=type;
  return zlib.gzipSync(Buffer.concat([header,data,Buffer.alloc((512-data.length%512)%512),Buffer.alloc(1024)]));
}
function zip(name,data) {
  const encoded=Buffer.from(name), compressed=zlib.deflateRawSync(data);
  const local=Buffer.alloc(30);local.writeUInt32LE(0x04034b50);local.writeUInt16LE(8,8);local.writeUInt16LE(encoded.length,26);
  const central=Buffer.alloc(46);central.writeUInt32LE(0x02014b50);central.writeUInt16LE(8,10);
  central.writeUInt32LE(compressed.length,20);central.writeUInt32LE(data.length,24);central.writeUInt16LE(encoded.length,28);
  const end=Buffer.alloc(22);end.writeUInt32LE(0x06054b50);end.writeUInt16LE(1,10);
  end.writeUInt32LE(central.length+encoded.length,12);end.writeUInt32LE(local.length+encoded.length+compressed.length,16);
  return Buffer.concat([local,encoded,compressed,central,encoded,end]);
}
test('platforms are explicit and unsupported architectures fail',()=>{
  assert.equal(target('win32','x64'),'x86_64-pc-windows-msvc');
  assert.equal(target('darwin','arm64'),'aarch64-apple-darwin');
  assert.throws(()=>target('win32','arm64'),/Unsupported/);
});

test('verified native download is installed atomically and reused',{skip:!process.env.GLYPH_TEST_BINARY},async()=>{
  const dir=fs.mkdtempSync(path.join(os.tmpdir(),'glyph-native-install-test-'));
  const previous={local:process.env.LOCALAPPDATA,cache:process.env.XDG_CACHE_HOME,fetch:global.fetch};
  process.env.LOCALAPPDATA=dir;process.env.XDG_CACHE_HOME=dir;
  const binary=fs.readFileSync(process.env.GLYPH_TEST_BINARY);
  const isWindows=process.platform==='win32';
  const archive=isWindows?zip('glyph.exe',binary):tar('glyph',binary);
  const hash=crypto.createHash('sha256').update(archive).digest('hex');
  const version=require('../package.json').version;
  const name=`glyph-server-${version}-${target()}.${isWindows?'zip':'tar.gz'}`;
  let calls=0;
  global.fetch=async url=>{calls++;assert.ok(String(url).startsWith(`https://github.com/glyphmusic/server/releases/download/v${version}/`));
    return new Response(String(url).endsWith('SHA256SUMS.txt')?`${hash}  ${name}\n`:archive);};
  try {
    const file=await install();assert.ok(fs.existsSync(file));assert.equal(calls,2);
    assert.equal(await install(),file);assert.equal(calls,2,'valid cached binary must skip downloads');
    assert.deepEqual(fs.readFileSync(file),binary);
  } finally {
    global.fetch=previous.fetch;
    for(const [key,value] of [['LOCALAPPDATA',previous.local],['XDG_CACHE_HOME',previous.cache]]) {if(value===undefined) delete process.env[key];else process.env[key]=value;}
    fs.rmSync(dir,{recursive:true,force:true});
  }
});
test('manifest requires an exact unique archive entry',()=>{
  const hash='a'.repeat(64);assert.equal(checksum(`${hash}  exact.zip\n`,'exact.zip'),hash);
  assert.throws(()=>checksum(`${hash}  other.zip`,'exact.zip'));
  assert.throws(()=>checksum(`${hash}  exact.zip\n${hash}  exact.zip`,'exact.zip'));
});
test('TAR/ZIP select only the executable and never follow archive paths',()=>{
  const data=Buffer.from('native binary');
  assert.deepEqual(unpack(tar('./glyph',data),false,['glyph']),data);
  assert.deepEqual(unpack(zip('glyph.exe',data),true,['glyph.exe']),data);
  assert.throws(()=>unpack(tar('../glyph',data),false,['glyph']));
  assert.throws(()=>unpack(tar('glyph',data,50),false,['glyph']));
  assert.throws(()=>unpack(zip('../../glyph.exe',data),true,['glyph.exe']));
  assert.throws(()=>unpack(Buffer.from('bad archive'),true,['glyph.exe']));
});
test('installer rejects altered release bytes before writing any executable',async()=>{
  const dir=fs.mkdtempSync(path.join(os.tmpdir(),'glyph-install-test-'));
  const previous={local:process.env.LOCALAPPDATA,cache:process.env.XDG_CACHE_HOME,fetch:global.fetch};
  process.env.LOCALAPPDATA=dir;process.env.XDG_CACHE_HOME=dir;
  global.fetch=async url=>new Response(String(url).endsWith('SHA256SUMS.txt')
    ? `${'a'.repeat(64)}  glyph-server-0.3.0-${target()}.${process.platform==='win32'?'zip':'tar.gz'}\n`:'tampered');
  try {await assert.rejects(install(),/SHA-256 mismatch/);assert.equal(fs.existsSync(executable()),false);}
  finally {
    global.fetch=previous.fetch;
    for(const [key,value] of [['LOCALAPPDATA',previous.local],['XDG_CACHE_HOME',previous.cache]]) {if(value===undefined) delete process.env[key];else process.env[key]=value;}
    fs.rmSync(dir,{recursive:true,force:true});
  }
});
