//! Whisper's byte-level BPE tokenizer, read from a Hugging Face
//! `tokenizer.json`.
//!
//! Decoding is a table lookup. Encoding (for the vocabulary prompt) splits
//! text with GPT-2's pre-tokenizer rules and applies the BPE merges. A small
//! implementation instead of the `tokenizers` crate keeps a large dependency
//! (and its C regex library) out of the build.

use std::collections::HashMap;

use serde::Deserialize;

#[derive(Debug)]
pub struct Tokenizer {
    /// Token text in GPT-2's byte-to-unicode alphabet, by id.
    tokens: Vec<String>,
    ids: HashMap<String, u32>,
    ranks: HashMap<(String, String), usize>,
    special: HashMap<String, u32>,
    /// Ids at or above this are special (`<|endoftext|>` and after).
    first_special: u32,
    byte_to_char: [char; 256],
    char_to_byte: HashMap<char, u8>,
}

#[derive(Deserialize)]
struct File {
    #[serde(default)]
    added_tokens: Vec<Added>,
    model: Model,
}

#[derive(Deserialize)]
struct Added {
    id: u32,
    content: String,
}

#[derive(Deserialize)]
struct Model {
    vocab: HashMap<String, u32>,
    #[serde(default)]
    merges: Vec<Merge>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Merge {
    Joined(String),
    Pair((String, String)),
}

impl Tokenizer {
    pub fn from_json(json: &str) -> Result<Self, String> {
        let file: File = sonic_rs::from_str(json).map_err(|e| format!("The tokenizer couldn’t be read: {e}"))?;
        let mut ids = file.model.vocab;
        let mut special = HashMap::new();
        for added in file.added_tokens {
            special.insert(added.content.clone(), added.id);
            ids.insert(added.content, added.id);
        }
        let len = ids.values().copied().max().map_or(0, |m| m as usize + 1);
        let mut tokens = vec![String::new(); len];
        for (text, &id) in &ids {
            tokens[id as usize] = text.clone();
        }
        let ranks = file
            .model
            .merges
            .into_iter()
            .enumerate()
            .filter_map(|(rank, merge)| {
                let pair = match merge {
                    Merge::Joined(s) => {
                        let (a, b) = s.split_once(' ')?;
                        (a.to_owned(), b.to_owned())
                    }
                    Merge::Pair(pair) => pair,
                };
                Some((pair, rank))
            })
            .collect();
        let first_special = *special.get("<|endoftext|>").ok_or("The tokenizer has no <|endoftext|> token.")?;
        let byte_to_char = bytes_to_unicode();
        let char_to_byte = byte_to_char.iter().enumerate().map(|(b, &c)| (c, b as u8)).collect();
        Ok(Self { tokens, ids, ranks, special, first_special, byte_to_char, char_to_byte })
    }

    /// The id of a special token such as `<|startoftranscript|>`.
    pub fn special(&self, token: &str) -> Option<u32> {
        self.special.get(token).copied()
    }

    /// Whether `id` is a special token rather than text.
    pub fn is_special(&self, id: u32) -> bool {
        id >= self.first_special
    }

    pub fn vocab_size(&self) -> usize {
        self.tokens.len()
    }

    /// The text of `ids`, skipping special tokens.
    pub fn decode(&self, ids: &[u32]) -> String {
        let mut bytes = Vec::new();
        for &id in ids {
            if self.is_special(id) {
                continue;
            }
            if let Some(token) = self.tokens.get(id as usize) {
                bytes.extend(token.chars().filter_map(|c| self.char_to_byte.get(&c)));
            }
        }
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// Token ids of `text`.
    pub fn encode(&self, text: &str) -> Vec<u32> {
        let mut out = Vec::new();
        for piece in pre_tokenize(text) {
            let mut word: Vec<String> = piece.bytes().map(|b| self.byte_to_char[b as usize].to_string()).collect();
            loop {
                let best = word
                    .windows(2)
                    .enumerate()
                    .filter_map(|(i, w)| self.ranks.get(&(w[0].clone(), w[1].clone())).map(|r| (*r, i)))
                    .min();
                let Some((_, i)) = best else { break };
                let pair = (word[i].clone(), word[i + 1].clone());
                let mut merged = Vec::with_capacity(word.len());
                let mut j = 0;
                while j < word.len() {
                    if j + 1 < word.len() && word[j] == pair.0 && word[j + 1] == pair.1 {
                        merged.push(format!("{}{}", pair.0, pair.1));
                        j += 2;
                    } else {
                        merged.push(std::mem::take(&mut word[j]));
                        j += 1;
                    }
                }
                word = merged;
            }
            out.extend(word.iter().filter_map(|t| self.ids.get(t)));
        }
        out
    }
}

/// GPT-2's reversible map from bytes to printable characters.
fn bytes_to_unicode() -> [char; 256] {
    let mut map = ['\0'; 256];
    let printable = |b: u32| (33..=126).contains(&b) || (161..=172).contains(&b) || (174..=255).contains(&b);
    let mut extra = 0;
    for b in 0..256u32 {
        map[b as usize] = if printable(b) {
            char::from_u32(b).unwrap_or('\0')
        } else {
            extra += 1;
            char::from_u32(255 + extra).unwrap_or('\0')
        };
    }
    map
}

#[derive(PartialEq, Clone, Copy)]
enum Class {
    Letter,
    Number,
    Space,
    Other,
}

fn class(c: char) -> Class {
    if c.is_alphabetic() {
        Class::Letter
    } else if c.is_numeric() {
        Class::Number
    } else if c.is_whitespace() {
        Class::Space
    } else {
        Class::Other
    }
}

/// Splits `text` as GPT-2's pattern does:
/// `'s|'t|'re|'ve|'m|'ll|'d| ?\p{L}+| ?\p{N}+| ?[^\s\p{L}\p{N}]+|\s+(?!\S)|\s+`.
pub(crate) fn pre_tokenize(text: &str) -> Vec<&str> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let n = chars.len();
    let at = |i: usize| chars.get(i).map_or(text.len(), |c| c.0);
    let mut pieces = Vec::new();
    let mut i = 0;
    while i < n {
        let c = chars[i].1;
        // Contractions.
        if c == '\'' {
            let rest = &text[at(i + 1)..];
            let len = ["re", "ve", "ll", "s", "t", "m", "d"].iter().find(|s| rest.starts_with(*s)).map(|s| s.len());
            if let Some(len) = len {
                pieces.push(&text[at(i)..at(i + 1) + len]);
                i += 1 + len;
                continue;
            }
        }
        // An optional space, then a run of one class.
        let lead = usize::from(c == ' ' && i + 1 < n && class(chars[i + 1].1) != Class::Space);
        let first = chars.get(i + lead).map(|c| class(c.1));
        if let Some(kind @ (Class::Letter | Class::Number | Class::Other)) = first {
            let mut j = i + lead + 1;
            while j < n && class(chars[j].1) == kind {
                j += 1;
            }
            pieces.push(&text[at(i)..at(j)]);
            i = j;
            continue;
        }
        // Whitespace: all of it at the end of the text; before text, all but
        // the last character (which leads the next piece), unless it is the
        // only one.
        let mut j = i;
        while j < n && class(chars[j].1) == Class::Space {
            j += 1;
        }
        if j < n && j - i > 1 {
            j -= 1;
        }
        pieces.push(&text[at(i)..at(j)]);
        i = j;
    }
    pieces
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A tiny tokenizer in the `tokenizer.json` format: bytes, a few merges
    /// and Whisper's special tokens.
    pub(crate) fn fixture() -> Tokenizer {
        let map = bytes_to_unicode();
        let mut vocab: Vec<(String, u32)> = (0..256).map(|b| (map[b].to_string(), b as u32)).collect();
        let merges = [("Ġ", "h"), ("Ġh", "e"), ("l", "l"), ("Ġhe", "ll"), ("Ġhell", "o"), ("o", "r")];
        for (a, b) in merges {
            let id = vocab.len() as u32;
            vocab.push((format!("{a}{b}"), id));
        }
        let mut special = vec![];
        let mut next = vocab.len() as u32;
        for name in [
            "<|endoftext|>",
            "<|startoftranscript|>",
            "<|en|>",
            "<|de|>",
            "<|translate|>",
            "<|transcribe|>",
            "<|startoflm|>",
            "<|startofprev|>",
            "<|nospeech|>",
            "<|notimestamps|>",
        ] {
            special.push(format!(r#"{{"id":{next},"content":"{name}","special":true}}"#));
            next += 1;
        }
        let vocab_json = vocab.iter().map(|(t, id)| format!("{}:{id}", sonic_rs::to_string(t).unwrap())).collect::<Vec<_>>();
        let merges_json = merges.iter().map(|(a, b)| format!("\"{a} {b}\"")).collect::<Vec<_>>();
        let json = format!(
            r#"{{"added_tokens":[{}],"model":{{"type":"BPE","vocab":{{{}}},"merges":[{}]}}}}"#,
            special.join(","),
            vocab_json.join(","),
            merges_json.join(",")
        );
        Tokenizer::from_json(&json).unwrap()
    }

    #[test]
    fn pre_tokenizer_follows_gpt2() {
        assert_eq!(pre_tokenize("Hello world"), ["Hello", " world"]);
        assert_eq!(pre_tokenize("it's 42 keymap.rs"), ["it", "'s", " 42", " keymap", ".", "rs"]);
        assert_eq!(pre_tokenize("a  b"), ["a", " ", " b"]);
        assert_eq!(pre_tokenize("x\ny "), ["x", "\n", "y", " "]);
        assert_eq!(pre_tokenize("Grüße, 世界!"), ["Grüße", ",", " 世界", "!"]);
        assert_eq!(pre_tokenize(""), Vec::<&str>::new());
    }

    #[test]
    fn encode_applies_merges_and_decode_inverts() {
        let t = fixture();
        let ids = t.encode(" hello world");
        let hello = t.ids["Ġhello"];
        assert_eq!(ids[0], hello);
        assert_eq!(t.decode(&ids), " hello world");
        let unicode = t.encode(" naïve ✓");
        assert_eq!(t.decode(&unicode), " naïve ✓");
    }

    #[test]
    fn specials_are_known_and_skipped_when_decoding() {
        let t = fixture();
        let sot = t.special("<|startoftranscript|>").unwrap();
        assert!(t.is_special(sot));
        assert!(!t.is_special(t.ids["Ġhello"]));
        let mut ids = vec![sot];
        ids.extend(t.encode(" hello"));
        ids.push(t.special("<|endoftext|>").unwrap());
        assert_eq!(t.decode(&ids), " hello");
    }

    #[test]
    fn merges_may_be_pairs() {
        let json = r#"{"added_tokens":[{"id":3,"content":"<|endoftext|>"}],"model":{"vocab":{"a":0,"b":1,"ab":2},"merges":[["a","b"]]}}"#;
        let t = Tokenizer::from_json(json).unwrap();
        assert_eq!(t.encode("ab"), vec![2]);
    }
}
