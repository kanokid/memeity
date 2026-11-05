//! Duplicate finder: exact copies (same bytes, by MD5) and near-duplicates (the same
//! meme resized, recompressed or re-saved), found with a perceptual "difference hash".
//!
//! dHash: shrink the image to 9x8 greyscale and record, for each row, whether each pixel
//! is brighter than its right neighbour: 64 bits describing the image's overall
//! structure. Resizing or JPEG artifacts barely change it, so near-identical images
//! differ in only a few bits (Hamming distance), while unrelated images differ in ~32.

use std::collections::HashMap;
use std::path::Path;

/// Images whose hashes differ in at most this many of 64 bits count as near-duplicates.
pub const SIMILAR_BITS: u32 = 6;

/// The 64-bit difference hash of an image file.
pub fn dhash(path: &Path) -> Result<u64, String> {
    let img = image::DynamicImage::ImageRgba8(crate::clipboard::decode(path)?);
    Ok(dhash_of(&img))
}

fn dhash_of(img: &image::DynamicImage) -> u64 {
    let small = img.resize_exact(9, 8, image::imageops::FilterType::Triangle).into_luma8();
    let mut hash = 0u64;
    for y in 0..8 {
        for x in 0..8 {
            let brighter = small.get_pixel(x, y).0[0] > small.get_pixel(x + 1, y).0[0];
            hash = (hash << 1) | u64::from(brighter);
        }
    }
    hash
}

pub struct Candidate {
    pub rel: String,
    pub md5: String,
    pub phash: Option<u64>,
}

/// A set of memes that look like copies of each other.
#[derive(Debug, PartialEq)]
pub struct Group {
    /// True if every item has identical bytes; false if some are only visually similar.
    pub exact: bool,
    pub rels: Vec<String>,
}

/// Groups identical files together, then joins groups whose images look alike.
pub fn group(candidates: &[Candidate]) -> Vec<Group> {
    // 1. Exact copies: one bucket per MD5.
    let mut by_md5: HashMap<&str, Vec<&Candidate>> = HashMap::new();
    for c in candidates {
        by_md5.entry(&c.md5).or_default().push(c);
    }
    let buckets: Vec<Vec<&Candidate>> = by_md5.into_values().collect();

    // 2. Near-duplicates: union buckets whose representative hashes are close.
    let hashes: Vec<Option<u64>> = buckets.iter().map(|b| b.iter().find_map(|c| c.phash)).collect();
    let mut parent: Vec<usize> = (0..buckets.len()).collect();
    fn find(parent: &mut [usize], i: usize) -> usize {
        let mut root = i;
        while parent[root] != root {
            root = parent[root];
        }
        let mut node = i;
        while parent[node] != root {
            let next = parent[node];
            parent[node] = root;
            node = next;
        }
        root
    }
    for a in 0..buckets.len() {
        let Some(ha) = hashes[a] else { continue };
        for (b, hb) in hashes.iter().enumerate().skip(a + 1) {
            if let Some(hb) = *hb
                && (ha ^ hb).count_ones() <= SIMILAR_BITS
            {
                let (ra, rb) = (find(&mut parent, a), find(&mut parent, b));
                if ra != rb {
                    parent[rb] = ra;
                }
            }
        }
    }

    let mut components: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..buckets.len() {
        let root = find(&mut parent, i);
        components.entry(root).or_default().push(i);
    }
    let mut groups: Vec<Group> = components
        .into_values()
        .filter_map(|members| {
            let rels: Vec<String> = members.iter().flat_map(|&i| buckets[i].iter().map(|c| c.rel.clone())).collect();
            (rels.len() > 1).then_some(Group { exact: members.len() == 1, rels })
        })
        .collect();
    // Exact copies first (easy wins), then bigger groups; stable order for the UI.
    groups.sort_by(|a, b| b.exact.cmp(&a.exact).then(b.rels.len().cmp(&a.rels.len())).then(a.rels.cmp(&b.rels)));
    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(rel: &str, md5: &str, phash: Option<u64>) -> Candidate {
        Candidate { rel: rel.into(), md5: md5.into(), phash }
    }

    #[test]
    fn groups_exact_and_similar() {
        let base = 0xF0F0_F0F0_F0F0_F0F0u64;
        let candidates = vec![
            cand("a.png", "m1", Some(base)),
            cand("a copy.png", "m1", Some(base)),        // exact copy of a
            cand("a small.jpg", "m2", Some(base ^ 0b111)), // 3 bits off: near-duplicate
            cand("other.png", "m3", Some(!base)),        // unrelated
            cand("solo.png", "m4", Some(0x1234)),
            cand("twin1.gif", "m5", None),
            cand("twin2.gif", "m5", None), // exact pair without a phash yet
        ];
        let groups = group(&candidates);
        assert_eq!(groups.len(), 2);
        assert!(groups[0].exact && groups[0].rels.len() == 2, "exact pair first: {groups:?}");
        let similar = groups.iter().find(|g| !g.exact).unwrap();
        let mut rels = similar.rels.clone();
        rels.sort();
        assert_eq!(rels, ["a copy.png", "a small.jpg", "a.png"]);
    }

    #[test]
    fn dhash_survives_resizing_and_tells_images_apart() {
        let gradient = image::RgbImage::from_fn(400, 300, |x, y| image::Rgb([(x % 256) as u8, (y % 256) as u8, 128]));
        let big = image::DynamicImage::ImageRgb8(gradient);
        let small = big.resize_exact(120, 90, image::imageops::FilterType::Lanczos3);
        let flipped = big.fliph();
        let (a, b, c) = (dhash_of(&big), dhash_of(&small), dhash_of(&flipped));
        assert!((a ^ b).count_ones() <= SIMILAR_BITS, "resized copy matches");
        assert!((a ^ c).count_ones() > SIMILAR_BITS, "a different image doesn't");
    }
}
