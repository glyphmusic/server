//! `--check-update` / `self-update` against GitHub releases of the server repo.
//! Asset: `glyph-server-{version}-{target}.zip` (Windows) or `.tar.gz`, plus
//! `SHA256SUMS.txt` and its signature `SHA256SUMS.txt.minisig`. A checksum is
//! used only after the signature by the Glyph release key for this repository
//! and tag passed (`glyph_protocol::release`); r-dlp updates likewise. The
//! archive must contain the `glyph-server[.exe]` binary.

use glyph_server_core::admin::types::{UpdateApplyResponse, UpdateInfo};
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::io::Read;

pub const TARGET: &str = env!("GLYPH_TARGET");
const BIN: &str = if cfg!(windows) { "glyph-server.exe" } else { "glyph-server" };

#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    assets: Vec<Asset>,
}
#[derive(Debug, Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

pub fn parse_version(tag: &str) -> Option<Version> {
    Version::parse(tag.trim().trim_start_matches('v')).ok()
}

pub fn archive_name(version: &Version, target: &str) -> String {
    let ext = if target.contains("windows") { "zip" } else { "tar.gz" };
    format!("glyph-server-{version}-{target}.{ext}")
}

/// First whitespace-separated token of a `.sha256` file (`<hex>  <name>` or just `<hex>`).
pub fn parse_sha256(text: &str) -> Option<String> {
    let h = text.split_whitespace().next()?.to_ascii_lowercase();
    (h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit())).then_some(h)
}

fn checksum_for_asset(text: &str, filename: &str) -> Option<String> {
    let mut found = None;
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let mut parts = line.splitn(2, char::is_whitespace);
        let hash = parts.next()?;
        let name = parts.next().map(|n| n.trim_start().trim_start_matches('*'));
        if name.is_some_and(|n| n != filename) { continue; }
        if name.is_none() && text.split_whitespace().count() != 1 { continue; }
        if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) || found.is_some() { return None; }
        found = Some(hash.to_ascii_lowercase());
    }
    found
}

pub fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data).iter().map(|b| format!("{b:02x}")).collect()
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(concat!("glyph-server/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .expect("http client")
}

async fn latest(repo: &str) -> Result<Release, String> {
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let r = client().get(&url).header("accept", "application/vnd.github+json").send().await.map_err(|e| e.without_url().to_string())?;
    if !r.status().is_success() {
        return Err(format!("GitHub answered HTTP {} for {repo}", r.status().as_u16()));
    }
    let b = r.bytes().await.map_err(|e| e.without_url().to_string())?;
    serde_json::from_slice(&b).map_err(|e| e.to_string())
}

/// Compare the running version with the latest GitHub release.
pub async fn check_info(repo: &str) -> Result<UpdateInfo, String> {
    check_component(repo, env!("CARGO_PKG_VERSION")).await
}

pub async fn check_component(repo: &str, installed: &str) -> Result<UpdateInfo, String> {
    let rel = latest(repo).await?;
    let remote = parse_version(&rel.tag_name).ok_or_else(|| format!("unparsable tag `{}`", rel.tag_name))?;
    let current = Version::parse(installed).map_err(|_| "installed component has no semantic version")?;
    let available = remote > current;
    Ok(UpdateInfo {
        current: current.to_string(),
        latest: Some(remote.to_string()),
        update_available: available,
        message: if available {
            format!("update available: {current} -> {remote} ({repo})")
        } else {
            if remote < current {format!("installed {current} is newer than published {remote} ({repo})")}
            else {format!("{} {current} is up to date",repo.rsplit('/').next().unwrap_or("component"))}
        },
    })
}

/// CLI: print whether a newer release exists.
pub async fn check(repo: &str) -> Result<Option<Version>, String> {
    let info = check_info(repo).await?;
    println!("{}", info.message);
    if info.update_available {
        println!("run `glyph update` to install it");
    }
    Ok(info.latest.filter(|_| info.update_available).and_then(|v| Version::parse(&v).ok()))
}

async fn download(url: &str) -> Result<Vec<u8>, String> {
    let r = client().get(url).send().await.map_err(|e| e.without_url().to_string())?;
    if !r.status().is_success() {
        return Err(format!("download failed: HTTP {}", r.status().as_u16()));
    }
    use futures::StreamExt;
    let mut stream=r.bytes_stream(); let mut bytes=Vec::new();
    while let Some(chunk)=stream.next().await {
        let chunk=chunk.map_err(|e|e.without_url().to_string())?;
        if bytes.len()+chunk.len()>128*1024*1024 {return Err("release download exceeds size limit".into());}
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

/// The checksum list of `release`, after its signature by the Glyph release
/// key for `repo` and the release's tag passed.
async fn signed_sums(release: &Release, repo: &str) -> Result<String, String> {
    use glyph_protocol::release::{CHECKSUM_ASSET, SIGNATURE_ASSET};
    let find = |n: &str| release.assets.iter().find(|a| a.name == n).map(|a| a.browser_download_url.clone());
    let sums_url = find(CHECKSUM_ASSET).ok_or("release has no SHA256SUMS.txt; refusing to install unverified code")?;
    let sig_url = find(SIGNATURE_ASSET).ok_or("release is not signed; refusing to install unverified code")?;
    let sums = download(&sums_url).await?;
    let sig = download(&sig_url).await?;
    glyph_protocol::release::verify_checksums(&sums, &String::from_utf8_lossy(&sig), repo, &release.tag_name)
        .map_err(|e| format!("{e}; update aborted"))?;
    Ok(String::from_utf8_lossy(&sums).into_owned())
}

pub fn extract_binary(archive: &[u8], is_zip: bool) -> Result<Vec<u8>, String> {
    extract_named(archive,is_zip,&[BIN,"glyph","glyph.exe"])
}
fn extract_named(archive: &[u8], is_zip: bool, names: &[&str]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    if is_zip {
        let mut z = zip::ZipArchive::new(std::io::Cursor::new(archive)).map_err(|e| e.to_string())?;
        for i in 0..z.len() {
            let mut f = z.by_index(i).map_err(|e| e.to_string())?;
            if f.is_file() && f.name().ok().and_then(|n| n.rsplit('/').next().map(|x| names.contains(&x))).unwrap_or(false) {
                f.read_to_end(&mut out).map_err(|e| e.to_string())?;
                return Ok(out);
            }
        }
    } else {
        let mut t = tar::Archive::new(flate2::read::GzDecoder::new(archive));
        for e in t.entries().map_err(|e| e.to_string())? {
            let mut e = e.map_err(|e| e.to_string())?;
            let is_bin = e.path().map_err(|e| e.to_string())?.file_name().map(|n| names.iter().any(|name| n == *name)).unwrap_or(false);
            if is_bin {
                e.read_to_end(&mut out).map_err(|e| e.to_string())?;
                return Ok(out);
            }
        }
    }
    Err(format!("`{BIN}` not found in the archive"))
}

/// Download, verify (signature, SHA-256) and install the latest release over the running binary.
pub async fn apply(repo: &str) -> Result<UpdateApplyResponse, String> {
    if std::path::Path::new("/.dockerenv").exists() {
        return Err("Container-managed companion: on its Docker host run `glyph update companion --compose-dir /path/to/compose-project` to pull, recreate and verify the image".into());
    }
    if std::env::var_os("GLYPH_NPM_MANAGED").is_some() {
        return Err("npm-managed executable: run `glyph update cli`, then restart the server using the updated CLI".into());
    }
    let rel = latest(repo).await?;
    let remote = parse_version(&rel.tag_name).ok_or_else(|| format!("unparsable tag `{}`", rel.tag_name))?;
    let current = Version::parse(env!("CARGO_PKG_VERSION")).unwrap();
    if remote <= current {
        return Ok(UpdateApplyResponse {
            message: format!("glyph {current} is already the latest version"),
            restart_required: false,
        });
    }
    let name = archive_name(&remote, TARGET);
    let find = |n: &str| rel.assets.iter().find(|a| a.name == n).map(|a| a.browser_download_url.clone());
    let url = find(&name).ok_or_else(|| format!("release {remote} has no asset `{name}` for this platform"))?;
    let sums = signed_sums(&rel, repo).await?;

    let archive = download(&url).await?;
    let expected = checksum_for_asset(&sums, &name)
        .ok_or("checksum manifest has no valid, unique entry for this archive")?;
    if sha256_hex(&archive) != expected {
        return Err("SHA-256 mismatch; aborting update".into());
    }
    let bin = extract_binary(&archive, name.ends_with(".zip"))?;

    // Stage next to the running executable so the final swap stays on one volume.
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let staged = exe.with_file_name(format!(".glyph-server-{remote}.new{}",if cfg!(windows){".exe"}else{""}));
    std::fs::write(&staged, &bin).map_err(|e| format!("cannot write {}: {e}", staged.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755));
    }
    let reported=crate::diagnostics::tool_version(staged.to_str().ok_or("staged path is not UTF-8")?,false).await;
    if reported.as_deref().and_then(|s|s.split_whitespace().last())!=Some(remote.to_string().as_str()) {
        let _=std::fs::remove_file(&staged);
        return Err("downloaded Glyph executable reports an unexpected version".into());
    }
    let res = self_replace::self_replace(&staged).map_err(|e| e.to_string());
    let _ = std::fs::remove_file(&staged);
    res?;
    Ok(UpdateApplyResponse {
        message: format!("updated {current} -> {remote}; restart the server (or the service) to use the new version"),
        restart_required: true,
    })
}

pub async fn self_update(repo: &str) -> Result<(), String> {
    println!("{}", apply(repo).await?.message);
    Ok(())
}

pub async fn install_rdlp(program: &str, expected: &str) -> Result<(),String> {
    let destination = if std::path::Path::new(program).is_file() { std::path::PathBuf::from(program) } else {
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).flat_map(|p| {
            let plain=p.join(program); vec![plain.clone(),plain.with_extension("exe")]
        }).find(|p|p.is_file()).ok_or("cannot locate the installed r-dlp executable")?
    }.canonicalize().map_err(|_|"cannot resolve r-dlp path")?;
    let release = latest("glyphmusic/r-dlp").await?;
    if parse_version(&release.tag_name).map(|v|v.to_string()).as_deref()!=Some(expected) { return Err("r-dlp release changed during update; retry".into()); }
    let name=format!("r-dlp-{expected}-{TARGET}.{}",if cfg!(windows){"zip"}else{"tar.gz"});
    let asset=|name:&str| release.assets.iter().find(|a|a.name==name).map(|a|a.browser_download_url.clone());
    let bytes=download(&asset(&name).ok_or("r-dlp has no release binary for this platform")?).await?;
    let sums=signed_sums(&release,"glyphmusic/r-dlp").await?;
    let hash=checksum_for_asset(&sums,&name).ok_or("r-dlp checksum missing or ambiguous")?;
    if sha256_hex(&bytes)!=hash {return Err("r-dlp SHA-256 mismatch; update aborted".into());}
    let binary=extract_named(&bytes,cfg!(windows),&["r-dlp","r-dlp.exe"])?;
    let dir=tempfile::Builder::new().prefix(".glyph-rdlp-").tempdir_in(destination.parent().ok_or("r-dlp path has no parent")?).map_err(|e|e.to_string())?;
    let staged=dir.path().join(if cfg!(windows){"r-dlp.exe"}else{"r-dlp"});
    std::fs::write(&staged,binary).map_err(|e|e.to_string())?;
    #[cfg(unix)] {use std::os::unix::fs::PermissionsExt;std::fs::set_permissions(&staged,std::fs::Permissions::from_mode(0o755)).map_err(|e|e.to_string())?;}
    let version=crate::diagnostics::tool_version(staged.to_str().ok_or("r-dlp path is not UTF-8")?,false).await;
    if version.as_deref()!=Some(expected) {return Err("downloaded r-dlp reports an unexpected version".into());}
    let backup=dir.path().join("previous");
    std::fs::rename(&destination,&backup).map_err(|e|format!("cannot replace r-dlp (read-only image or executable in use): {e}"))?;
    if let Err(error)=std::fs::rename(&staged,&destination) {
        if let Err(restore)=std::fs::rename(&backup,&destination) {
            let recovery=dir.keep();
            return Err(format!("r-dlp replacement failed: {error}; restore failed: {restore}; original retained at {}",recovery.display()));
        }
        return Err(format!("r-dlp replacement failed; original restored: {error}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_and_names() {
        assert_eq!(parse_version("v1.2.3"), Version::parse("1.2.3").ok());
        assert!(parse_version("nightly").is_none());
        let v = Version::parse("0.2.0").unwrap();
        assert_eq!(archive_name(&v, "x86_64-pc-windows-msvc"), "glyph-server-0.2.0-x86_64-pc-windows-msvc.zip");
        assert_eq!(archive_name(&v, "aarch64-apple-darwin"), "glyph-server-0.2.0-aarch64-apple-darwin.tar.gz");
        assert!(parse_version("v0.10.0") > parse_version("v0.9.9"));
    }

    #[test]
    fn release_key_matches_the_published_file() {
        let file = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../release-key.pub")).unwrap();
        assert_eq!(glyph_protocol::release::PUBLIC_KEYS, [file.lines().nth(1).unwrap()]);
    }

    #[test]
    fn checksum_parsing_and_verification() {
        let data = b"hello";
        let h = sha256_hex(data);
        assert_eq!(h, "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824");
        assert_eq!(parse_sha256(&format!("{h}  file.zip\n")), Some(h.clone()));
        assert_eq!(parse_sha256(&h.to_uppercase()), Some(h));
        assert_eq!(parse_sha256("nothex"), None);
        let h = "a".repeat(64);
        let other = "b".repeat(64);
        let manifest = format!("{other}  other.zip\n{h}  file.zip\n");
        assert_eq!(checksum_for_asset(&manifest, "file.zip"), Some(h));
        assert_eq!(checksum_for_asset(&manifest, "missing.zip"), None);
        assert_eq!(checksum_for_asset(&manifest.repeat(2), "file.zip"), None);
    }

    #[test]
    fn extracts_from_tar_gz() {
        let mut buf = Vec::new();
        {
            let gz = flate2::write::GzEncoder::new(&mut buf, flate2::Compression::fast());
            let mut tb = tar::Builder::new(gz);
            let mut h = tar::Header::new_gnu();
            h.set_size(3);
            h.set_mode(0o755);
            h.set_cksum();
            tb.append_data(&mut h, format!("pkg/{BIN}"), &b"abc"[..]).unwrap();
            tb.into_inner().unwrap().finish().unwrap();
        }
        assert_eq!(extract_binary(&buf, false).unwrap(), b"abc");
        assert!(extract_binary(&buf[..10], false).is_err());
    }
}
