use super::{super::OrderedVocabIter, Cache, Error, Pair, Word, DEFAULT_CACHE_CAPACITY};
use crate::tokenizer::{Model, Offsets, Result, Token};
use crate::utils::iter::ResultShunt;
use serde_json::Value;
use std::borrow::Cow;
use std::{
    collections::HashMap,
    fs::File,
    io::prelude::*,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

type Vocab = HashMap<String, u32>;
type VocabR = HashMap<u32, String>;
type Merges = HashMap<Pair, (u32, u32)>;

struct Config {
    files: Option<(String, String)>,
    vocab: Vocab,
    merges: Merges,
    cache_capacity: usize,
    dropout: Option<f32>,
    unk_token: Option<String>,
    continuing_subword_prefix: Option<String>,
    end_of_word_suffix: Option<String>,
}

/// A `BpeBuilder` can be used to create a `BPE` model with a custom configuration.
pub struct BpeBuilder {
    config: Config,
}

impl Default for BpeBuilder {
    fn default() -> Self {
        Self {
            config: Config {
                files: None,
                vocab: HashMap::new(),
                merges: HashMap::new(),
                cache_capacity: DEFAULT_CACHE_CAPACITY,
                dropout: None,
                unk_token: None,
                continuing_subword_prefix: None,
                end_of_word_suffix: None,
            },
        }
    }
}

impl BpeBuilder {
    /// Constructs a new `BpeBuilder`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the input files.
    pub fn files(mut self, vocab: String, merges: String) -> Self {
        self.config.files = Some((vocab, merges));
        self
    }

    /// Set the vocab (token -> ID) and merges mappings.
    pub fn vocab_and_merges(mut self, vocab: Vocab, merges: Merges) -> Self {
        self.config.vocab = vocab;
        self.config.merges = merges;
        self
    }

    /// Set the cache's capacity. Set to 0 if you want to disable caching.
    pub fn cache_capacity(mut self, capacity: usize) -> Self {
        self.config.cache_capacity = capacity;
        self
    }

    /// Use [dropout](https://arxiv.org/abs/1910.13267) with the model.
    pub fn dropout(mut self, dropout: f32) -> Self {
        self.config.dropout = Some(dropout);
        self
    }

    /// Set the `UNK` token for the vocab.
    pub fn unk_token(mut self, unk_token: String) -> Self {
        self.config.unk_token = Some(unk_token);
        self
    }

    /// Set the `continuing_subword_prefix` option.
    pub fn continuing_subword_prefix(mut self, prefix: String) -> Self {
        self.config.continuing_subword_prefix = Some(prefix);
        self
    }

    /// Set the `end_of_word_suffix` option.
    pub fn end_of_word_suffix(mut self, prefix: String) -> Self {
        self.config.end_of_word_suffix = Some(prefix);
        self
    }

    /// Returns a `BPE` model that uses the `BpeBuilder`'s configuration.
    pub fn build(mut self) -> Result<BPE> {
        // Validate dropout.
        if let Some(p) = self.config.dropout {
            if p <= 0.0 || p > 1.0 {
                return Err(Error::InvalidDropout.into());
            }
        }

        // Read files if necessary
        if let Some((vocab, merges)) = self.config.files {
            let (v, m) = BPE::read_files(&vocab, &merges)?;
            self.config.vocab = v;
            self.config.merges = m;
        }

        let vocab_r = self
            .config
            .vocab
            .iter()
            .map(|(key, val)| (*val, key.to_owned()))
            .collect();
        let cache = match self.config.cache_capacity {
            0 => None,
            capacity => Some(Cache::new(capacity)),
        };

        Ok(BPE {
            vocab: self.config.vocab,
            vocab_r,
            merges: self.config.merges,
            cache,
            dropout: self.config.dropout,
            unk_token: self.config.unk_token,
            continuing_subword_prefix: self.config.continuing_subword_prefix,
            end_of_word_suffix: self.config.end_of_word_suffix,
        })
    }
}

/// A [Byte Pair Encoding](https://www.aclweb.org/anthology/P16-1162/) model.
#[derive(PartialEq)]
pub struct BPE {
    /// The vocabulary assigns a number to each token.
    pub(crate) vocab: Vocab,
    /// Reversed vocabulary, to rebuild sentences.
    pub(crate) vocab_r: VocabR,
    /// Contains the mapping between Pairs and their (rank, new_id).
    pub(crate) merges: Merges,
    /// Contains the cache for optimizing the encoding step.
    cache: Option<Cache<String, Word>>,
    /// Dropout probability for merges. 0 = no dropout is the default. At 1.0, tokenization will
    /// perform no merges, so the result will just be characters.
    pub(super) dropout: Option<f32>,
    /// The unknown token to be used when we encounter an unknown char
    pub(super) unk_token: Option<String>,
    /// An optional prefix to use on any subword that exist only behind another one
    pub(super) continuing_subword_prefix: Option<String>,
    /// An optional suffix to caracterize and end-of-word subword
    pub(super) end_of_word_suffix: Option<String>,
}

impl std::fmt::Debug for BPE {
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> std::fmt::Result {
        fmt.debug_struct("BPE")
            .field("dropout", &self.dropout)
            .field("unk_token", &self.unk_token)
            .field("continuing_subword_prefix", &self.continuing_subword_prefix)
            .field("end_of_word_suffix", &self.end_of_word_suffix)
            .field("vocab", &self.vocab.len())
            .field("merges", &self.merges.len())
            .finish()
    }
}

impl Default for BPE {
    fn default() -> Self {
        Self::builder().build().unwrap()
    }
}

impl Clone for BPE {
    // `Clone` can't be derive because it's not implemented for `Cache`.
    // To keep things simple when we clone, the new BPE will start with a fresh cache.
    fn clone(&self) -> Self {
        let fresh_cache = self.cache.as_ref().map(|cache| cache.fresh());
        Self {
            vocab: self.vocab.clone(),
            vocab_r: self.vocab_r.clone(),
            merges: self.merges.clone(),
            cache: fresh_cache,
            dropout: self.dropout,
            unk_token: self.unk_token.clone(),
            continuing_subword_prefix: self.continuing_subword_prefix.clone(),
            end_of_word_suffix: self.end_of_word_suffix.clone(),
        }
    }
}

/// Converts the merges strings (for example from `merges.txt` file) with the format
/// "{pair_a} {pair_b}" into the format expected by the BPE struct
pub(crate) fn convert_merges_to_hashmap<I: Iterator<Item = String>>(
    iter: I,
    vocab: &Vocab,
) -> Result<Merges> {
    let mut merges = HashMap::new();

    let lines = iter.filter(|l| !l.starts_with("#version"));
    for (rank, line) in lines.enumerate() {
        let parts = line.split(' ').collect::<Vec<_>>();
        if parts.len() != 2 {
            return Err(Error::BadMerges(rank + 1).into());
        }

        let a = vocab
            .get(parts[0])
            .ok_or_else(|| Error::MergeTokenOutOfVocabulary(parts[0].to_owned()))?;
        let b = vocab
            .get(parts[1])
            .ok_or_else(|| Error::MergeTokenOutOfVocabulary(parts[1].to_owned()))?;
        let pair = (*a, *b);
        let new_token = format!("{}{}", parts[0], parts[1]);
        let new_id = vocab
            .get(&new_token)
            .ok_or(Error::MergeTokenOutOfVocabulary(new_token))?;

        merges.insert(pair, (rank as u32, *new_id));
    }

    Ok(merges)
}

impl BPE {
    /// Initialize a `BpeBuilder`.
    pub fn builder() -> BpeBuilder {
        BpeBuilder::new()
    }

    /// Create a new BPE model with the given vocab and merges.
    pub fn new(vocab: Vocab, merges: Merges) -> Self {
        Self::builder()
            .vocab_and_merges(vocab, merges)
            .build()
            .unwrap()
    }

    /// Initialize a BpeBuilder model from vocab and merges files
    pub fn from_files(vocab: &str, merges: &str) -> BpeBuilder {
        BPE::builder().files(vocab.to_owned(), merges.to_owned())
    }

    /// Read the given files to extract the vocab and merges
    pub fn read_files(vocab: &str, merges: &str) -> Result<(Vocab, Merges)> {
        // Read vocab.json
        let vocab_file = File::open(vocab)?;
        let mut vocab_file = BufReader::new(vocab_file);

        let mut buffer = String::new();
        vocab_file.read_to_string(&mut buffer)?;
        let json: Value = serde_json::from_str(&buffer)?;
        let mut vocab = HashMap::new();
        match json {
            Value::Object(m) => {
                for (token, id) in m {
                    if let Value::Number(id) = id {
                        let id = id.as_u64().ok_or(Error::BadVocabulary)? as u32;
                        vocab.insert(token, id);
                    }
                }
            }
            _ => return Err(Box::new(Error::BadVocabulary)),
        };

        // Read merges file
        let merge_file = File::open(merges)?;
        let merge_file = BufReader::new(merge_file);
        let merges = ResultShunt::process(merge_file.lines(), |iter| {
            convert_merges_to_hashmap(iter, &vocab)
        })??;

        Ok((vocab, merges))
    }

    /// Reset the cache.
    pub fn clear_cache(&self) {
        if let Some(ref cache) = self.cache {
            cache.clear()
        }
    }

    pub fn get_vocab(&self) -> &Vocab {
        &self.vocab
    }

    pub fn get_unk_token(&self) -> &Option<String> {
        &self.unk_token
    }

    pub fn get_continuing_subword_prefix(&self) -> &Option<String> {
        &self.continuing_subword_prefix
    }

    fn merge_word(&self, w: &str) -> Result<Word> {
        let mut indices = w.char_indices().map(|(idx, _)| idx).peekable();
        let mut word = Word::with_capacity(w.len());
        while let Some(i) = indices.next() {
            let s = if let Some(&end) = indices.peek() {
                match (i, self.continuing_subword_prefix.as_ref()) {
                    (0, Some(prefix)) => Cow::Owned(format!("{}{}", prefix, &w[i..end])),
                    _ => Cow::Borrowed(&w[i..end]),
                }
            } else {
                self.end_of_word_suffix
                    .as_ref()
                    .map(|suffix| format!("{}{}", &w[i..], suffix).into())
                    .unwrap_or_else(|| Cow::Borrowed(&w[i..]))
            };

            if let Some(id) = self.vocab.get(s.as_ref()) {
                word.add(*id);
            } else if let Some(unk) = &self.unk_token {
                let unk_id = self
                    .vocab
                    .get(unk)
                    .ok_or_else(|| Error::UnkTokenOutOfVocabulary(unk.to_owned()))?;
                // Handle UNK token
                word.add(*unk_id);
            }
        }

        word.merge_all(&self.merges, self.dropout);

        Ok(word)
    }

    fn word_to_tokens<'a, 'b: 'a>(
        &'a self,
        index: u32,
        word: &'b Word,
        initial_offsets: (usize, usize),
    ) -> impl Iterator<Item = Token> + 'a {
        word.get_chars_iter()
            .zip(word.get_offsets_iter())
            .map(move |(id, offsets)| {
                assert!(initial_offsets.0 + offsets.0 < initial_offsets.1);
                Token::new(
                    id,
                    self.vocab_r[&id].clone(),
                    (initial_offsets.0 + offsets.0, initial_offsets.0 + offsets.1),
                    index,
                )
            })
    }

    fn tokenize_with_cache(
        &self,
        sentence: Vec<(String, Offsets)>,
        cached: Vec<Option<Word>>,
    ) -> Result<Vec<Token>> {
        let mut misses = Vec::new();
        let mut encoded: Vec<Token> = Vec::with_capacity(sentence.len());
        for (i, ((w, initial_offsets), maybe_hit)) in sentence.into_iter().zip(cached).enumerate() {
            if let Some(hit) = maybe_hit {
                encoded.extend(self.word_to_tokens(i as u32, &hit, initial_offsets));
            } else {
                let word = self.merge_word(&w)?;
                encoded.extend(self.word_to_tokens(i as u32, &word, initial_offsets));
                misses.push((w, word));
            }
        }
        if !misses.is_empty() {
            self.cache.as_ref().unwrap().set_values(misses)
        }
        Ok(encoded)
    }
}

#[typetag::serde]
impl Model for BPE {
    fn get_vocab(&self) -> &HashMap<String, u32> {
        &self.vocab
    }

    fn get_vocab_size(&self) -> usize {
        self.vocab.len()
    }

    fn tokenize(&self, sentence: Vec<(String, Offsets)>) -> Result<Vec<Token>> {
        if sentence.is_empty() {
            return Ok(vec![]);
        }

        let mut encoded: Vec<Token> = Vec::with_capacity(sentence.len());
        if self.dropout.is_none() {
            if let Some(cached_words) = self
                .cache
                .as_ref()
                .and_then(|c| c.get_values(sentence.iter().map(|(s, _)| s)))
            {
                return self.tokenize_with_cache(sentence, cached_words);
            }
        }

        for (i, (w, initial_offsets)) in sentence.into_iter().enumerate() {
            let word = self.merge_word(&w)?;
            encoded.extend(self.word_to_tokens(i as u32, &word, initial_offsets));
        }

        Ok(encoded)
    }

    fn token_to_id(&self, token: &str) -> Option<u32> {
        self.vocab.get(token).copied()
    }

    fn id_to_token(&self, id: u32) -> Option<&str> {
        self.vocab_r.get(&id).map(String::as_ref)
    }

    fn save(&self, folder: &Path, name: Option<&str>) -> Result<Vec<PathBuf>> {
        let vocab_file_name = match name {
            Some(name) => format!("{}-vocab.json", name),
            None => "vocab.json".to_string(),
        };

        // Write vocab.json
        let vocab_path: PathBuf = [folder, Path::new(vocab_file_name.as_str())]
            .iter()
            .collect();
        let mut vocab_file = File::create(&vocab_path)?;
        let order_vocab_iter = OrderedVocabIter::new(&self.vocab_r);
        let serialized = serde_json::to_string(&order_vocab_iter)?;
        vocab_file.write_all(&serialized.as_bytes())?;

        // Write merges.txt
        let merges_file_name = match name {
            Some(name) => format!("{}-merges.txt", name),
            None => "merges.txt".to_string(),
        };

        let merges_path: PathBuf = [folder, Path::new(merges_file_name.as_str())]
            .iter()
            .collect();
        let mut merges_file = File::create(&merges_path)?;
        let mut merges: Vec<(&Pair, &u32)> = self
            .merges
            .iter()
            .map(|(pair, (rank, _))| (pair, rank))
            .collect();
        merges.sort_unstable_by_key(|k| *k.1);
        merges_file.write_all(b"#version: 0.2 - Trained by `huggingface/tokenizers`\n")?;
        merges_file.write_all(
            &merges
                .into_iter()
                .flat_map(|(pair, _)| {
                    format!("{} {}\n", self.vocab_r[&pair.0], self.vocab_r[&pair.1]).into_bytes()
                })
                .collect::<Vec<_>>()[..],
        )?;

        Ok(vec![vocab_path, merges_path])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn test_ordered_vocab_iter() {
        let vocab_r: VocabR = [
            (0, "a".into()),
            (1, "b".into()),
            (2, "c".into()),
            (3, "ab".into()),
        ]
        .iter()
        .cloned()
        .collect();
        let order_vocab_iter = OrderedVocabIter::new(&vocab_r);
        let serialized = serde_json::to_string(&order_vocab_iter).unwrap();
        assert_eq!(serialized, "{\"a\":0,\"b\":1,\"c\":2,\"ab\":3}");
    }

    #[test]
    // Test tokenization. With dropout set to 0 tokenization is deterministic,
    // so we know exactly what the result should be.
    //
    // To test this, we'll build a simple model to tokenize the word 'unrelated'.
    fn test_tokenize_with_and_without_dropout() {
        let vocab: Vocab = [
            ("u".into(), 0),
            ("n".into(), 1),
            ("r".into(), 2),
            ("e".into(), 3),
            ("l".into(), 4),
            ("a".into(), 5),
            ("t".into(), 6),
            ("d".into(), 7),
            ("re".into(), 8),
            ("at".into(), 9),
            ("ed".into(), 10),
            ("un".into(), 11),
            ("ated".into(), 12),
            ("rel".into(), 13),
            ("related".into(), 14),
            ("unrelated".into(), 15),
        ]
        .iter()
        .cloned()
        .collect();
        let merges: Merges = [
            ((vocab["r"], vocab["e"]), (1u32, vocab["re"])), // 'r-e' -> 're'
            ((vocab["a"], vocab["t"]), (2u32, vocab["at"])), // 'a-t' -> 'at'
            ((vocab["e"], vocab["d"]), (3u32, vocab["ed"])), // 'e-d' -> 'ed'
            ((vocab["u"], vocab["n"]), (4u32, vocab["un"])), // 'u-n' -> 'un'
            ((vocab["at"], vocab["ed"]), (5u32, vocab["ated"])), // 'at-ed' -> 'ated'
            ((vocab["re"], vocab["l"]), (6u32, vocab["rel"])), // 're-l' -> 'rel'
            ((vocab["rel"], vocab["ated"]), (7u32, vocab["related"])), // 'rel-ated' -> 'related'
            ((vocab["un"], vocab["related"]), (8u32, vocab["unrelated"])), // 'un-related' -> 'unrelated'
        ]
        .iter()
        .cloned()
        .collect();
        let mut bpe = BPE::new(vocab, merges);

        let sentence: Vec<(String, Offsets)> = vec![("unrelated".into(), (0, 9))];

        // With no dropout:
        let tokens = bpe.tokenize(sentence.clone()).unwrap();
        assert_eq!(
            tokens,
            vec![Token::new(15u32, "unrelated".into(), (0, 9), 0)]
        );

        // Now set dropout to 1.0. Result should be no merges performed.
        bpe.dropout = Some(1.0);
        let tokens = bpe.tokenize(sentence.clone()).unwrap();
        assert_eq!(
            tokens,
            vec![
                Token::new(0u32, "u".into(), (0, 1), 0),
                Token::new(1u32, "n".into(), (1, 2), 0),
                Token::new(2u32, "r".into(), (2, 3), 0),
                Token::new(3u32, "e".into(), (3, 4), 0),
                Token::new(4u32, "l".into(), (4, 5), 0),
                Token::new(5u32, "a".into(), (5, 6), 0),
                Token::new(6u32, "t".into(), (6, 7), 0),
                Token::new(3u32, "e".into(), (7, 8), 0),
                Token::new(7u32, "d".into(), (8, 9), 0),
            ]
        );

        // Now try with dropout between 0 and 1.
        bpe.dropout = Some(0.5);
        let tokens = bpe.tokenize(sentence).unwrap();
        assert!(!tokens.is_empty() && tokens.len() <= 9);
    }

    #[test]
    // Ensure `BPE::from_files` works as expected.
    fn test_bpe_from_files() {
        // Set up vocab file.
        let mut vocab_file = NamedTempFile::new().unwrap();
        vocab_file
            .write_all(b"{\"a\": 0, \"b\": 1, \"c\": 2, \"ab\": 3}")
            .unwrap();

        // Set up merges file.
        let mut merges_file = NamedTempFile::new().unwrap();
        merges_file.write_all(b"#version: 0.2\na b").unwrap();

        // Make sure we can instantiate a BPE model from the files.
        let builder = BPE::from_files(
            vocab_file.path().to_str().unwrap(),
            merges_file.path().to_str().unwrap(),
        );
        let bpe = builder.build().unwrap();

        // Check merges.
        assert_eq!(bpe.merges.get(&(0, 1)).unwrap(), &(0u32, 3u32));

        // Check vocab.
        assert_eq!(bpe.vocab.get("a").unwrap(), &0u32);
        assert_eq!(bpe.vocab.get("b").unwrap(), &1u32);
        assert_eq!(bpe.vocab.get("c").unwrap(), &2u32);
        assert_eq!(bpe.vocab.get("ab").unwrap(), &3u32);
    }

    #[test]
    // Ensure `MergeTokenOutOfVocabulary` error is returned when it should be.
    fn test_bpe_from_files_merge_token_oov() {
        // Set up vocab file.
        let mut vocab_file = NamedTempFile::new().unwrap();
        vocab_file
            .write_all(b"{\"a\": 0, \"b\": 1, \"c\": 2, \"ab\": 3}")
            .unwrap();

        // Set up merges file.
        let mut merges_file = NamedTempFile::new().unwrap();
        merges_file.write_all(b"#version: 0.2\na b\na d").unwrap();

        // Ensure the result of BPE::from_files is a MergeTokenOutOfVocabulary error.
        match BPE::from_files(
            vocab_file.path().to_str().unwrap(),
            merges_file.path().to_str().unwrap(),
        )
        .build()
        {
            Ok(_) => unreachable!(),
            Err(err) => match err.downcast_ref::<Error>() {
                Some(Error::MergeTokenOutOfVocabulary(token)) => {
                    assert_eq!(*token, String::from("d"))
                }
                _ => unreachable!(),
            },
        }
    }

    #[test]
    // Ensure `BadMerges` error is returned when there is an invalid line in the
    // merges.txt file.
    fn test_bpe_from_files_bad_merges() {
        // Set up vocab file.
        let mut vocab_file = NamedTempFile::new().unwrap();
        vocab_file
            .write_all("{\"a\": 0, \"b\": 1, \"c\": 2, \"ab\": 3}".as_bytes())
            .unwrap();

        // Set up merges file with a bad line.
        let mut merges_file = NamedTempFile::new().unwrap();
        merges_file.write_all(b"#version: 0.2\na b\nc").unwrap();

        // Ensure the result of BPE::from_files is a BadMerges error.
        match BPE::from_files(
            vocab_file.path().to_str().unwrap(),
            merges_file.path().to_str().unwrap(),
        )
        .build()
        {
            Ok(_) => unreachable!(),
            Err(err) => match err.downcast_ref::<Error>() {
                Some(Error::BadMerges(line)) => assert_eq!(*line, 2),
                _ => unreachable!(),
            },
        }
    }
}
