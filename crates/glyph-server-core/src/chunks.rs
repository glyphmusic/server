//! Byte chunks retain ordinary MP4/Range compatibility with the native player.
//! Variants are separate representations; bytes from different encodes never mix.
use crate::media::{self, AudioVariant};
use serde::{Serialize,Deserialize};
use sha2::{Digest,Sha256};
pub const CHUNK_BYTES: u64 = 1024*1024;
pub const MAX_BYTES: u64 = 32*1024*1024;
#[derive(Clone,Debug,Serialize,Deserialize)]
pub struct Manifest {
    pub version: u8,
    pub id: String,
    pub quality: AudioVariant,
    pub total: u64,
    pub parts: Vec<Part>,
}
#[derive(Clone,Debug,Serialize,Deserialize)]
pub struct Part { pub key: String, pub size: u64 }
#[derive(Clone,Debug,PartialEq,Eq)]
pub struct Window { pub part: usize, pub offset: u64, pub length: u64 }
impl Manifest {
    pub fn build(id: &str, quality: AudioVariant, bytes: &[u8]) -> Option<Self> {
        if !media::valid_video_id(id) || bytes.is_empty() || bytes.len() as u64 > MAX_BYTES { return None; }
        let digest: String = Sha256::digest(bytes).iter().map(|b|format!("{b:02x}")).collect();
        let parts = bytes.chunks(CHUNK_BYTES as usize).enumerate().map(|(i,b)| Part {
            key:format!("audio/youtube/{id}/{}/{digest}/{i:04}",quality.name()),size:b.len() as u64,
        }).collect();
        Some(Self {version:1,id:id.into(),quality,total:bytes.len() as u64,parts})
    }
    pub fn valid(&self, id: &str, quality: AudioVariant) -> bool {
        self.version==1 && self.id==id && self.quality==quality && self.total>0 && self.total<=MAX_BYTES
        && self.parts.len()==self.total.div_ceil(CHUNK_BYTES) as usize
        && self.parts.iter().enumerate().all(|(i,p)| {
            let prefix=format!("audio/youtube/{id}/{}/",quality.name());
            let rest=p.key.strip_prefix(&prefix).unwrap_or("");
            let hash=rest.split('/').next().unwrap_or("");
            hash.len()==64 && hash.bytes().all(|b| b.is_ascii_hexdigit()) && rest==format!("{hash}/{i:04}")
                && self.parts.first().is_some_and(|p|p.key.starts_with(&format!("{prefix}{hash}/")))
                && p.size==(self.total-i as u64*CHUNK_BYTES).min(CHUNK_BYTES)
        })
    }
    pub fn windows(&self, start: u64, end: u64) -> Option<Vec<Window>> {
        if start>end || end>=self.total { return None; }
        let first=start/CHUNK_BYTES;
        let last=end/CHUNK_BYTES;
        Some((first..=last).map(|part| {
            let base=part*CHUNK_BYTES;
            let offset=start.saturating_sub(base);
            let length=(end+1-base).min(self.parts[part as usize].size)-offset;
            Window {part:part as usize,offset,length}
        }).collect())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn exact_range_across_chunk_boundary_and_last_byte() {
        let m=Manifest::build("abcdefghijk",AudioVariant::Low,&vec![7;(CHUNK_BYTES*2+5) as usize]).unwrap();
        assert!(m.valid("abcdefghijk",AudioVariant::Low));
        assert!(!m.valid("abcdefghijk",AudioVariant::High));
        assert_eq!(m.windows(CHUNK_BYTES-2,CHUNK_BYTES+2).unwrap(),vec![Window{part:0,offset:CHUNK_BYTES-2,length:2},Window{part:1,offset:0,length:3}]);
        assert_eq!(m.windows(m.total-1,m.total-1).unwrap()[0].length,1);
        assert!(m.windows(m.total,m.total).is_none());
        let mut bad=m.clone(); bad.parts[0].key="users/private/image".into();
        assert!(!bad.valid("abcdefghijk",AudioVariant::Low));
    }
}
