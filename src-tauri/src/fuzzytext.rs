//! Forgiving matching for text read from images.
//!
//! OCR output is messy: words run together ("IWILL BE THERE"), letters get misread
//! ("THEFE"), punctuation comes and goes. So besides exact word matches (FTS), search
//! also compares a "squashed" form of the query (lowercase letters and digits only, no
//! spaces) against the squashed text of every meme, allowing a few typos. That makes
//! "i will be there" find "IWILL BE THERE", and "be there" find "BE THEFE".
//!
//! Failing that, a query of several words still matches if *every* word turns up
//! somewhere in the text, in any order (each allowed its own few typos). OCR often
//! picks up stray text between the words you remember (usernames, "Reply" buttons), so
//! "will it work hi lol" finds "WILLIT WORK? wavy.khOlil 2 hi lol Reply". These rank a
//! little below whole-phrase matches.
//!
//! The squashed texts live in memory (a few KB per thousand memes), so this stays
//! instant on every keystroke.

use std::collections::{HashMap, HashSet};

/// Lowercase letters and digits only: drops spaces, punctuation and case.
pub fn squash(s: &str) -> Vec<char> {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

/// How many typos to tolerate for a squashed query of `len` characters.
/// Short queries must match exactly, or "cat" would match half the library.
pub fn allowed_errors(len: usize) -> usize {
    match len {
        0..=4 => 0,
        5..=8 => 1,
        9..=14 => 2,
        _ => 3,
    }
}

/// The fewest edits (insertions, deletions, substitutions) that turn `pattern` into
/// some substring of `text`, if that's at most `max`. (Sellers' approximate substring
/// matching: like Levenshtein distance, but the match may start anywhere in `text`.)
pub fn substring_distance(pattern: &[char], text: &[char], max: usize) -> Option<usize> {
    let m = pattern.len();
    if m == 0 {
        return Some(0);
    }
    if !could_match(pattern, text, max) {
        return None;
    }
    // col[i] = edits to match pattern[..i] ending at the current text position.
    let mut col: Vec<usize> = (0..=m).collect();
    let mut best = col[m];
    for &t in text {
        let mut diagonal = col[0]; // previous column's value at i-1
        col[0] = 0; // a match may start at any text position for free
        for i in 1..=m {
            let substitution = diagonal + usize::from(pattern[i - 1] != t);
            diagonal = col[i];
            col[i] = substitution.min(col[i] + 1).min(col[i - 1] + 1);
        }
        best = best.min(col[m]);
        if best == 0 {
            break;
        }
    }
    (best <= max).then_some(best)
}

/// Cheap pre-check (the pigeonhole filter): if `pattern` matches with at most `max`
/// edits, then cutting it into `max + 1` pieces leaves at least one piece untouched, so
/// it must appear exactly in `text`. Most memes fail this instantly, skipping the full
/// comparison.
fn could_match(pattern: &[char], text: &[char], max: usize) -> bool {
    let pieces = (max + 1).min(pattern.len());
    let size = pattern.len() / pieces;
    (0..pieces).any(|p| {
        let start = p * size;
        let end = if p + 1 == pieces { pattern.len() } else { start + size };
        let piece = &pattern[start..end];
        text.windows(piece.len()).any(|w| w == piece)
    })
}

/// Total typos if every word appears somewhere in `text` (any order, overlaps allowed),
/// each within its own typo allowance.
fn all_words(words: &[Vec<char>], text: &[char]) -> Option<usize> {
    words.iter().try_fold(0, |total, w| Some(total + substring_distance(w, text, allowed_errors(w.len()))?))
}

/// Squashed image text (and tags) per meme, keyed by library-relative path.
#[derive(Default)]
pub struct TextCache {
    texts: HashMap<String, Vec<char>>,
}

impl TextCache {
    pub fn set(&mut self, rel: &str, text: &str, tags: &[String]) {
        let mut squashed = squash(text);
        for tag in tags {
            squashed.extend(squash(tag));
        }
        if squashed.is_empty() {
            self.texts.remove(rel);
        } else {
            self.texts.insert(rel.to_string(), squashed);
        }
    }

    pub fn remove(&mut self, rel: &str) {
        self.texts.remove(rel);
    }

    pub fn retain(&mut self, keep: &HashSet<String>) {
        self.texts.retain(|rel, _| keep.contains(rel));
    }

    /// Memes whose text loosely contains `query`, with how many typos it took (an
    /// all-the-words match counts one more, so it ranks below the same phrase match).
    pub fn matches(&self, query: &str) -> Vec<(String, usize)> {
        let q = squash(query);
        if q.len() < 3 {
            return Vec::new(); // too short to say anything useful; word search covers it
        }
        let max = allowed_errors(q.len());
        let words: Vec<Vec<char>> = query.split_whitespace().map(squash).filter(|w| !w.is_empty()).collect();
        // A couple of one-letter words ("a b") would match nearly everything.
        let by_words = words.len() >= 2 && q.len() >= 4;
        self.texts
            .iter()
            .filter_map(|(rel, text)| {
                let typos = substring_distance(&q, text, max)
                    .or_else(|| by_words.then(|| all_words(&words, text)).flatten().map(|t| t + 1))?;
                Some((rel.clone(), typos))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dist(pattern: &str, text: &str, max: usize) -> Option<usize> {
        substring_distance(&squash(pattern), &squash(text), max)
    }

    #[test]
    fn squash_ignores_spaces_case_and_punctuation() {
        assert_eq!(squash("I WILL, be-there!").iter().collect::<String>(), "iwillbethere");
    }

    #[test]
    fn your_example_matches_exactly() {
        assert_eq!(dist("i will be there", "IWILL BE THERE", 3), Some(0));
        assert_eq!(dist("be there", "IWILL BE THERE", 1), Some(0), "partial phrase");
    }

    #[test]
    fn merged_words_from_ocr_still_match() {
        assert_eq!(dist("first try", "ONTHEFIRSTTRY", 2), Some(0));
    }

    #[test]
    fn tolerates_misread_letters() {
        assert_eq!(dist("be there", "IWILL BE THEFE", 1), Some(1), "one substitution");
        assert_eq!(dist("compiles", "COMPLIES", 2), Some(2), "transposed letters");
        assert_eq!(dist("hello", "WORLD", 1), None, "unrelated text doesn't match");
    }

    #[test]
    fn every_word_somewhere_matches_in_any_order() {
        let mut cache = TextCache::default();
        cache.set("will.png", "WILLIT\nWORK?\nwavy.khOlil 2\nhi lol\nReply", &[]);
        cache.set("other.png", "will it blend", &[]);
        assert_eq!(cache.matches("will it work hi lol"), vec![("will.png".to_string(), 1)], "stray text in between");
        assert_eq!(cache.matches("lol will work").len(), 1, "any order");
        assert!(cache.matches("will it work tomorrow").is_empty(), "every word must be there");
        let mut phrase = cache.matches("will it blend");
        phrase.sort();
        assert_eq!(phrase, vec![("other.png".to_string(), 0)], "a whole phrase still counts as 0 typos");
    }

    #[test]
    fn short_queries_must_be_exact() {
        assert_eq!(allowed_errors(3), 0);
        assert_eq!(allowed_errors(6), 1);
        assert_eq!(allowed_errors(12), 2);
        assert_eq!(allowed_errors(20), 3);
    }

    #[test]
    fn cache_finds_and_forgets() {
        let mut cache = TextCache::default();
        cache.set("a.png", "IWILL BE THERE", &[]);
        cache.set("b.png", "nobody: absolutely nobody", &["cat".into()]);
        cache.set("c.png", "", &[]);
        let hits = cache.matches("i will be there");
        assert_eq!(hits, vec![("a.png".to_string(), 0)]);
        assert_eq!(cache.matches("cat").len(), 1, "tags count too");
        assert!(cache.matches("xy").is_empty(), "too short");
        cache.retain(&HashSet::from(["b.png".to_string()]));
        assert!(cache.matches("will be there").is_empty());
    }
}

#[cfg(test)]
mod bench {
    /// `cargo test fuzzy_speed -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn fuzzy_speed() {
        let lines = [
            "WHEN YOU FINALLY FIX THE BUG BUT THEN REALIZE IT WAS IN PRODUCTION THE WHOLE TIME",
            "NOBODY: ABSOLUTELY NOBODY: ME AT 3AM REWRITING MY ENTIRE CONFIG",
            "THIS IS FINE",
            "ME EXPLAINING TO MY MOM WHY I NEED ANOTHER MECHANICAL KEYBOARD",
            "WHEN THE BUILD PASSES ON THE FIRST TRY",
        ];
        let mut cache = super::TextCache::default();
        for i in 0..2000 {
            // A realistic library: varied captions, few containing any given phrase.
            let text = if i % 100 == 0 { lines[0] } else { lines[1 + i % 4] };
            cache.set(&format!("m{i}.png"), &format!("{text} {i}"), &[]);
        }
        for query in ["realize it was in production", "realise it waz in prodution", "keyboard"] {
            let started = std::time::Instant::now();
            let hits = cache.matches(query);
            println!("2000 memes, {query:?}: {} hits in {:?}", hits.len(), started.elapsed());
        }
    }
}
