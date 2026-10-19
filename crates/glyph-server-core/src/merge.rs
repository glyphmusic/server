//! Title/artist normalisation, duplicate detection and merging.
//!
//! Two tracks are the same song when they share an ISRC, or when the
//! normalised title and artist match and the durations differ by at most 3 s
//! (a missing duration is not a mismatch).

use glyph_protocol::{ProviderId, Track};
use std::collections::BTreeSet;

pub const DURATION_TOLERANCE_MS: u64 = 3000;

const NOISE: &[&str] = &[
    "official", "audio", "video", "lyric", "lyrics", "remaster", "remastered", "feat", "ft",
    "featuring", "visualizer", "hd", "hq", "explicit", "mono", "stereo", "version", "radio edit",
    "topic",
];

/// True when `s` contains a noise term as a whole word (or phrase).
fn has_noise(s: &str) -> bool {
    let words = alnum_words(s);
    NOISE.iter().any(|n| contains_words(&words, n))
}

/// Lowercase alphanumeric words separated by single spaces.
pub fn plain_words(s: &str) -> String {
    alnum_words(s)
}

fn alnum_words(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_space = true;
    for c in s.chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
            last_space = false;
        } else if !last_space {
            out.push(' ');
            last_space = true;
        }
    }
    out.trim_end().to_string()
}

/// Lowercase, drop bracketed noise like "(Official Audio)" / "[Remastered 2011]",
/// cut ` feat. X` tails and ` - Remastered` suffixes, strip punctuation.
pub fn normalize_title(title: &str) -> String {
    let mut kept = String::new();
    let mut depth = 0usize;
    let mut inner = String::new();
    for c in title.chars() {
        match c {
            '(' | '[' => {
                if depth == 0 {
                    inner.clear();
                }
                depth += 1;
            }
            ')' | ']' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    // Keep meaningful brackets such as "(Part 2)"; drop noise.
                    if !has_noise(&inner) {
                        kept.push(' ');
                        kept.push_str(&inner);
                        kept.push(' ');
                    }
                }
            }
            _ if depth > 0 => inner.push(c),
            _ => kept.push(c),
        }
    }
    let mut low = kept.to_lowercase();
    for marker in [" feat.", " feat ", " ft.", " ft ", " featuring "] {
        if let Some(i) = low.find(marker) {
            low.truncate(i);
        }
    }
    if let Some(i) = low.find(" - ") {
        if has_noise(&low[i + 3..]) {
            low.truncate(i);
        }
    }
    alnum_words(&low)
}

/// Primary artist only: drops " - Topic", `feat.` tails and trailing collaborators.
pub fn normalize_artist(artist: &str) -> String {
    let mut low = artist.to_lowercase();
    if let Some(stripped) = low.strip_suffix(" - topic") {
        low = stripped.to_string();
    }
    for marker in [" feat.", " feat ", " ft.", " ft ", " featuring ", ",", " & ", " x ", ";"] {
        if let Some(i) = low.find(marker) {
            low.truncate(i);
        }
    }
    alnum_words(&low)
}

fn contains_words(hay: &str, needle: &str) -> bool {
    if needle.is_empty() || needle.len() > hay.len() {
        return false;
    }
    let hb = hay.as_bytes();
    for (i, _) in hay.match_indices(needle) {
        let left_ok = i == 0 || hb[i - 1] == b' ';
        let right_ok = i + needle.len() == hb.len() || hb[i + needle.len()] == b' ';
        if left_ok && right_ok {
            return true;
        }
    }
    false
}

pub fn artists_match(a: &str, b: &str) -> bool {
    let (a, b) = (normalize_artist(a), normalize_artist(b));
    !a.is_empty() && (a == b || contains_words(&a, &b) || contains_words(&b, &a))
}

pub fn durations_match(a: Option<u64>, b: Option<u64>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a.abs_diff(b) <= DURATION_TOLERANCE_MS,
        _ => true,
    }
}

fn isrc_eq(a: &Option<String>, b: &Option<String>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => !a.is_empty() && a.eq_ignore_ascii_case(b),
        _ => false,
    }
}

pub fn is_same_track(a: &Track, b: &Track) -> bool {
    if isrc_eq(&a.isrc, &b.isrc) {
        return true;
    }
    let (ta, tb) = (normalize_title(&a.title), normalize_title(&b.title));
    !ta.is_empty()
        && ta == tb
        && artists_match(&a.artist, &b.artist)
        && durations_match(a.duration_ms, b.duration_ms)
}

/// Jaccard similarity over words, 0.0..=1.0.
pub fn word_similarity(a: &str, b: &str) -> f32 {
    let wa: BTreeSet<&str> = a.split_whitespace().collect();
    let wb: BTreeSet<&str> = b.split_whitespace().collect();
    if wa.is_empty() || wb.is_empty() {
        return 0.0;
    }
    let inter = wa.intersection(&wb).count() as f32;
    let union = wa.union(&wb).count() as f32;
    inter / union
}

/// Preference when choosing which copy of a song is the "primary" one.
pub fn provider_rank(p: ProviderId) -> u8 {
    match p {
        ProviderId::Audius => 0,
        ProviderId::Jamendo => 1,
        ProviderId::SoundCloud => 2,
        ProviderId::YouTube => 3,
        ProviderId::Archive => 4,
        ProviderId::Deezer => 5,
        ProviderId::Spotify => 6,
        _ => 7,
    }
}

/// Fold `other` into `primary`: record its id under `external_ids` and fill gaps.
pub fn merge_into(primary: &mut Track, other: &Track) {
    if other.source != primary.source {
        primary.external_ids.entry(other.source).or_insert_with(|| other.id.clone());
    }
    for (p, id) in &other.external_ids {
        if *p != primary.source {
            primary.external_ids.entry(*p).or_insert_with(|| id.clone());
        }
    }
    macro_rules! fill {
        ($f:ident) => {
            if primary.$f.is_none() {
                primary.$f = other.$f.clone();
            }
        };
    }
    fill!(album);
    fill!(duration_ms);
    fill!(cover_url);
    fill!(isrc);
    fill!(permalink);
}

/// Merge duplicates in one list; the best-ranked provider's copy stays primary.
pub fn dedupe(mut tracks: Vec<Track>) -> Vec<Track> {
    tracks.sort_by_key(|t| provider_rank(t.source));
    let mut merger = Merger::default();
    merger.absorb(tracks);
    merger.seen
}

/// Incremental merger for the streaming search. A track keeps its identity
/// (`source`, `id`) once emitted; later duplicates are folded into it and the
/// updated copy is re-emitted so clients can replace it by (`source`, `id`).
#[derive(Default)]
pub struct Merger {
    pub seen: Vec<Track>,
    norm: Vec<(String, String)>,
}

impl Merger {
    /// Returns new tracks plus previously-seen tracks that gained a source.
    pub fn absorb(&mut self, batch: Vec<Track>) -> Vec<Track> {
        let mut touched: Vec<usize> = Vec::new();
        for t in batch {
            let nt = normalize_title(&t.title);
            let na = normalize_artist(&t.artist);
            let matched = self.seen.iter().enumerate().position(|(idx, s)| {
                if isrc_eq(&s.isrc, &t.isrc) {
                    return true;
                }
                let (st, sa) = &self.norm[idx];
                !nt.is_empty()
                    && *st == nt
                    && !sa.is_empty()
                    && (sa == &na || contains_words(sa, &na) || contains_words(&na, sa))
                    && durations_match(s.duration_ms, t.duration_ms)
            });
            if let Some(i) = matched {
                let before = self.seen[i].clone();
                merge_into(&mut self.seen[i], &t);
                if self.seen[i] != before && !touched.contains(&i) {
                    touched.push(i);
                }
            } else {
                self.seen.push(t);
                self.norm.push((nt, na));
                touched.push(self.seen.len() - 1);
            }
        }
        touched.sort_unstable();
        touched.into_iter().map(|i| self.seen[i].clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::track;
    use ProviderId::*;

    #[test]
    fn normalizes_noise() {
        assert_eq!(normalize_title("Get Lucky (Official Audio)"), "get lucky");
        assert_eq!(normalize_title("Get Lucky [Remastered 2011]"), "get lucky");
        assert_eq!(normalize_title("Song - Remastered 2009"), "song");
        assert_eq!(normalize_title("Hello (feat. Someone)"), "hello");
        assert_eq!(normalize_title("Hello feat. Someone"), "hello");
        assert_eq!(normalize_title("Track (Part 2)"), "track part 2");
        assert_eq!(normalize_title("  Don't  Stop!! "), "don t stop");
        assert_eq!(normalize_artist("Daft Punk - Topic"), "daft punk");
        assert_eq!(normalize_artist("Daft Punk feat. Pharrell"), "daft punk");
        assert_eq!(normalize_artist("A, B"), "a");
    }

    #[test]
    fn same_track_rules() {
        let a = track("1", Audius, "Get Lucky", "Daft Punk", 369);
        let b = track("2", YouTube, "Get Lucky (Official Audio)", "Daft Punk - Topic", 371);
        assert!(is_same_track(&a, &b));
        // +-3s boundary
        let edge = track("3", Deezer, "Get Lucky", "Daft Punk", 372);
        assert!(is_same_track(&a, &edge));
        let far = track("4", Deezer, "Get Lucky", "Daft Punk", 373);
        assert!(!is_same_track(&a, &far));
        // different artist
        assert!(!is_same_track(&a, &track("5", Deezer, "Get Lucky", "Someone Else", 369)));
        // missing duration is tolerated
        let mut nod = track("6", Archive, "Get Lucky", "Daft Punk", 0);
        nod.duration_ms = None;
        assert!(is_same_track(&a, &nod));
    }

    #[test]
    fn isrc_wins_over_text() {
        let mut a = track("1", Spotify, "Totally Different", "X", 100);
        let mut b = track("2", Deezer, "Another Name", "Y", 300);
        a.isrc = Some("GBAYE0601498".into());
        b.isrc = Some("gbaye0601498".into());
        assert!(is_same_track(&a, &b));
    }

    #[test]
    fn dedupe_prefers_best_source_and_merges_ids() {
        let yt = track("yt1", YouTube, "Song", "Artist", 200);
        let mut sp = track("sp1", Spotify, "Song", "Artist", 201);
        sp.cover_url = Some("http://cover".into());
        sp.isrc = Some("ISRC1".into());
        let au = track("au1", Audius, "Song", "Artist", 200);
        let other = track("x", Audius, "Other", "Artist", 100);
        let out = dedupe(vec![yt, sp, other, au]);
        assert_eq!(out.len(), 2);
        let song = out.iter().find(|t| t.title == "Song").unwrap();
        assert_eq!(song.source, Audius);
        assert_eq!(song.external_ids.get(&YouTube).unwrap(), "yt1");
        assert_eq!(song.external_ids.get(&Spotify).unwrap(), "sp1");
        assert_eq!(song.cover_url.as_deref(), Some("http://cover"));
        assert_eq!(song.isrc.as_deref(), Some("ISRC1"));
        assert_eq!(song.sources(), vec![Audius, YouTube, Spotify]);
    }

    #[test]
    fn merger_reemits_updated_tracks_only() {
        let mut m = Merger::default();
        let first = m.absorb(vec![track("a", Audius, "S", "A", 100), track("b", Audius, "T", "A", 100)]);
        assert_eq!(first.len(), 2);
        let second = m.absorb(vec![track("y", YouTube, "S", "A", 101), track("z", YouTube, "U", "A", 90)]);
        assert_eq!(second.len(), 2);
        assert_eq!(second[0].id, "a");
        assert_eq!(second[0].external_ids.get(&YouTube).unwrap(), "y");
        assert_eq!(second[1].id, "z");
        // Exact repeat changes nothing -> nothing re-emitted.
        assert!(m.absorb(vec![track("y", YouTube, "S", "A", 101)]).is_empty());
        assert_eq!(m.seen.len(), 3);
    }

    #[test]
    fn similarity() {
        assert_eq!(word_similarity("a b c", "a b c"), 1.0);
        assert_eq!(word_similarity("a b", "c d"), 0.0);
        assert!((word_similarity("a b", "b c") - 1.0 / 3.0).abs() < 1e-6);
    }
}
