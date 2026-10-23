//! Operator bans (`glyph remote ban ...`): accounts, IP addresses and ranges,
//! hardware ids and install ids. Checked in [`crate::Server::admit`] before any
//! other work, so a banned client costs one list lookup.
//!
//! Privacy: the server never keeps a client's IP in the clear. Devices of an
//! account remember a salted hash of their last IP ([`ip_hash`]) so banning an
//! account can also ban the addresses it used; only addresses the operator
//! types in (`ban ip 203.0.113.7/24`) are stored as written. Hardware ids
//! arrive already hashed by the app and are hashed again here.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::net::IpAddr;

/// More would make every request's lookup noticeably slower.
pub const MAX_BANS: usize = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BanKind {
    /// Account id (sessions of this account are refused).
    Account,
    /// IP address or CIDR range as the operator typed it.
    Ip,
    /// [`ip_hash`] of an address a banned account used.
    IpHash,
    /// [`hardware_hash`] of the app's hardware id.
    Hardware,
    /// [`install_hash`] of an install id.
    Install,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ban {
    pub id: String,
    pub kind: BanKind,
    pub value: String,
    /// What the operator sees, e.g. `@handle` or `@handle / Laptop`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub created_at: u64,
    /// `None` = until removed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    /// Account id a derived ban (its IPs, its hardware) belongs to, so
    /// unbanning the account removes them too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BanList {
    /// Random per server; keys [`ip_hash`]. Empty until the host creates it.
    #[serde(default)]
    pub salt: String,
    #[serde(default)]
    pub bans: Vec<Ban>,
}

/// Who is asking. Every field is optional; any match bans.
#[derive(Debug, Default, Clone, Copy)]
pub struct Subject<'a> {
    pub ip: Option<&'a str>,
    pub install: Option<&'a str>,
    pub hardware: Option<&'a str>,
    pub account: Option<&'a str>,
}

fn sha(s: &str) -> String {
    format!("{:x}", Sha256::digest(s.as_bytes()))
}

/// Salted hash of a client address; without the salt it cannot be reversed
/// by trying every IPv4 address.
pub fn ip_hash(salt: &str, ip: &str) -> Option<String> {
    let ip: IpAddr = ip.trim().parse().ok()?;
    (!salt.is_empty()).then(|| sha(&format!("glyph-ban-ip|{salt}|{ip}")))
}

/// Server-side hash of the app's hardware id header.
pub fn hardware_hash(header: &str) -> Option<String> {
    valid_id(header).then(|| sha(&format!("glyph-ban-hw|{}", header.trim().to_ascii_lowercase())))
}

pub fn install_hash(id: &str) -> Option<String> {
    valid_id(id).then(|| sha(&format!("glyph-ban-install|{}", id.trim())))
}

fn valid_id(v: &str) -> bool {
    let v = v.trim();
    (8..=128).contains(&v.len()) && v.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// `203.0.113.7`, `203.0.113.0/24`, `2001:db8::/32` -> canonical text, or `None`.
pub fn parse_net(s: &str) -> Option<String> {
    let (addr, prefix) = match s.trim().split_once('/') {
        Some((a, p)) => (a.parse::<IpAddr>().ok()?, Some(p.parse::<u8>().ok()?)),
        None => (s.trim().parse::<IpAddr>().ok()?, None),
    };
    let max = if addr.is_ipv4() { 32 } else { 128 };
    match prefix {
        Some(p) if p > max => None,
        Some(p) if p < max => Some(format!("{}/{p}", masked(addr, p))),
        _ => Some(addr.to_string()),
    }
}

fn masked(addr: IpAddr, prefix: u8) -> IpAddr {
    match addr {
        IpAddr::V4(a) => {
            let mask = if prefix == 0 { 0 } else { u32::MAX << (32 - u32::from(prefix)) };
            IpAddr::V4((u32::from(a) & mask).into())
        }
        IpAddr::V6(a) => {
            let mask = if prefix == 0 { 0 } else { u128::MAX << (128 - u32::from(prefix)) };
            IpAddr::V6((u128::from(a) & mask).into())
        }
    }
}

fn net_contains(net: &str, ip: IpAddr) -> bool {
    match net.split_once('/') {
        Some((a, p)) => {
            let (Ok(a), Ok(p)) = (a.parse::<IpAddr>(), p.parse::<u8>()) else { return false };
            a.is_ipv4() == ip.is_ipv4() && masked(ip, p) == a
        }
        None => net.parse::<IpAddr>().is_ok_and(|a| a == ip),
    }
}

impl Ban {
    pub fn live(&self, now: u64) -> bool {
        self.expires_at.map_or(true, |t| t > now)
    }
}

impl BanList {
    /// The first live ban that matches `who`.
    pub fn find(&self, who: &Subject, now: u64) -> Option<&Ban> {
        if self.bans.is_empty() {
            return None;
        }
        let ip = who.ip.and_then(|i| i.trim().parse::<IpAddr>().ok());
        let ip_h = who.ip.and_then(|i| ip_hash(&self.salt, i));
        let hw = who.hardware.and_then(hardware_hash);
        let install = who.install.and_then(install_hash);
        self.bans.iter().filter(|b| b.live(now)).find(|b| match b.kind {
            BanKind::Account => who.account == Some(b.value.as_str()),
            BanKind::Ip => ip.is_some_and(|ip| net_contains(&b.value, ip)),
            BanKind::IpHash => ip_h.as_deref() == Some(b.value.as_str()),
            BanKind::Hardware => hw.as_deref() == Some(b.value.as_str()),
            BanKind::Install => install.as_deref() == Some(b.value.as_str()),
        })
    }

    /// Drops expired bans; true when something was removed.
    pub fn prune(&mut self, now: u64) -> bool {
        let before = self.bans.len();
        self.bans.retain(|b| b.live(now));
        self.bans.len() != before
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ban(kind: BanKind, value: &str) -> Ban {
        Ban { id: "b".into(), kind, value: value.into(), label: None, reason: None, created_at: 0, expires_at: None, origin: None }
    }

    #[test]
    fn networks_are_canonical_and_matched() {
        assert_eq!(parse_net("203.0.113.77/24").as_deref(), Some("203.0.113.0/24"));
        assert_eq!(parse_net(" 203.0.113.7 ").as_deref(), Some("203.0.113.7"));
        assert_eq!(parse_net("203.0.113.7/32").as_deref(), Some("203.0.113.7"));
        assert_eq!(parse_net("2001:db8::1/32").as_deref(), Some("2001:db8::/32"));
        assert!(parse_net("203.0.113.7/33").is_none() && parse_net("example.com").is_none());
        let list = BanList { salt: "s".into(), bans: vec![ban(BanKind::Ip, "203.0.113.0/24")] };
        let at = |ip| list.find(&Subject { ip: Some(ip), ..Default::default() }, 0).is_some();
        assert!(at("203.0.113.200"));
        assert!(!at("203.0.114.1"));
        assert!(!at("::ffff:1"));
    }

    #[test]
    fn hashed_kinds_match_only_their_subject_and_expire() {
        let salt = "salt";
        let mut list = BanList {
            salt: salt.into(),
            bans: vec![
                ban(BanKind::IpHash, &ip_hash(salt, "198.51.100.4").unwrap()),
                ban(BanKind::Hardware, &hardware_hash("ab12cd34ef56").unwrap()),
                ban(BanKind::Install, &install_hash("install-0001").unwrap()),
                Ban { expires_at: Some(10), ..ban(BanKind::Account, "u_1") },
            ],
        };
        let find = |w: Subject, now| list.find(&w, now).map(|b| b.kind);
        assert_eq!(find(Subject { ip: Some("198.51.100.4"), ..Default::default() }, 0), Some(BanKind::IpHash));
        assert_eq!(find(Subject { ip: Some("198.51.100.5"), ..Default::default() }, 0), None);
        assert_eq!(find(Subject { hardware: Some("AB12CD34EF56"), ..Default::default() }, 0), Some(BanKind::Hardware));
        assert_eq!(find(Subject { install: Some("install-0001"), ..Default::default() }, 0), Some(BanKind::Install));
        assert_eq!(find(Subject { account: Some("u_1"), ..Default::default() }, 5), Some(BanKind::Account));
        assert_eq!(find(Subject { account: Some("u_1"), ..Default::default() }, 11), None);
        assert!(list.prune(11));
        assert_eq!(list.bans.len(), 3);
        // A different salt gives different IP hashes.
        assert_ne!(ip_hash("a", "198.51.100.4"), ip_hash("b", "198.51.100.4"));
        assert_eq!(ip_hash("", "198.51.100.4"), None);
    }
}
