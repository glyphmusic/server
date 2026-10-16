//! `--check-update` / `self-update` against GitHub releases of the server repo.
//! Asset: `glyph-server-{version}-{target}.zip` (Windows) or `.tar.gz`, plus a
//! `.sha256` file. The archive must contain the `glyph-server[.exe]` binary.

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

/// Prints whether a newer release exists. Returns the newer version, if any.
pub async fn check(repo: &str) -> Result<Option<Version>, String> {
    let rel = latest(repo).await?;
    let remote = parse_version(&rel.tag_name).ok_or_else(|| format!("unparsable tag `{}`", rel.tag_name))?;
    let current = Version::parse(env!("CARGO_PKG_VERSION")).unwrap();
    if remote > current {
        println!("update available: {current} -> {remote} ({repo})\nrun `glyph-server self-update` to install it");
        Ok(Some(remote))
    } else {
        println!("glyph-server {current} is up to date");
        Ok(None)
    }
}

async fn download(url: &str) -> Result<Vec<u8>, String> {
    let r = client().get(url).send().await.map_err(|e| e.without_url().to_string())?;
    if !r.status().is_success() {
        return Err(format!("download failed: HTTP {}", r.status().as_u16()));
    }
    Ok(r.bytes().await.map_err(|e| e.without_url().to_string())?.to_vec())
}

pub fn extract_binary(archive: &[u8], is_zip: bool) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    if is_zip {
        let mut z = zip::ZipArchive::new(std::io::Cursor::new(archive)).map_err(|e| e.to_string())?;
        for i in 0..z.len() {
            let mut f = z.by_index(i).map_err(|e| e.to_string())?;
            if f.is_file() && f.name().ok().and_then(|n| n.rsplit('/').next().map(|x| x == BIN)).unwrap_or(false) {
                f.read_to_end(&mut out).map_err(|e| e.to_string())?;
                return Ok(out);
            }
        }
    } else {
        let mut t = tar::Archive::new(flate2::read::GzDecoder::new(archive));
        for e in t.entries().map_err(|e| e.to_string())? {
            let mut e = e.map_err(|e| e.to_string())?;
            let is_bin = e.path().map_err(|e| e.to_string())?.file_name().map(|n| n == BIN).unwrap_or(false);
            if is_bin {
                e.read_to_end(&mut out).map_err(|e| e.to_string())?;
                return Ok(out);
            }
        }
    }
    Err(format!("`{BIN}` not found in the archive"))
}

pub async fn self_update(repo: &str) -> Result<(), String> {
    let rel = latest(repo).await?;
    let remote = parse_version(&rel.tag_name).ok_or_else(|| format!("unparsable tag `{}`", rel.tag_name))?;
    let current = Version::parse(env!("CARGO_PKG_VERSION")).unwrap();
    if remote <= current {
        println!("glyph-server {current} is already the latest version");
        return Ok(());
    }
    let name = archive_name(&remote, TARGET);
    let find = |n: &str| rel.assets.iter().find(|a| a.name == n).map(|a| a.browser_download_url.clone());
    let url = find(&name).ok_or_else(|| format!("release {remote} has no asset `{name}` for this platform"))?;
    let sum_url = find(&format!("{name}.sha256")).ok_or_else(|| format!("release {remote} has no `{name}.sha256`; refusing to install unverified code"))?;

    println!("downloading {name} ...");
    let archive = download(&url).await?;
    let expected = parse_sha256(&String::from_utf8_lossy(&download(&sum_url).await?)).ok_or("malformed .sha256 file")?;
    let actual = sha256_hex(&archive);
    if actual != expected {
        return Err("SHA-256 mismatch; aborting update".into());
    }
    let bin = extract_binary(&archive, name.ends_with(".zip"))?;

    // Stage next to the running executable so the final swap stays on one volume.
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let staged = exe.with_file_name(format!(".glyph-server-{remote}.new"));
    std::fs::write(&staged, &bin).map_err(|e| format!("cannot write {}: {e}", staged.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755));
    }
    let res = self_replace::self_replace(&staged).map_err(|e| e.to_string());
    let _ = std::fs::remove_file(&staged);
    res?;
    println!("updated {current} -> {remote}; restart the server to use the new version");
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
    fn checksum_parsing_and_verification() {
        let data = b"hello";
        let h = sha256_hex(data);
        assert_eq!(h, "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824");
        assert_eq!(parse_sha256(&format!("{h}  file.zip\n")), Some(h.clone()));
        assert_eq!(parse_sha256(&h.to_uppercase()), Some(h));
        assert_eq!(parse_sha256("nothex"), None);
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
