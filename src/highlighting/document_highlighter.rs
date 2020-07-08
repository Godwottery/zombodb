use self::pg_catalog::*;
use crate::elasticsearch::analyze::*;
use crate::elasticsearch::Elasticsearch;
use levenshtein::*;
use pgx::PgRelation;
use pgx::*;
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::marker::PhantomData;
use std::ops::Deref;

#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct TokenEntry {
    pub type_: String,
    pub position: u32,
    pub start_offset: u64,
    pub end_offset: u64,
}

mod pg_catalog {
    use pgx::*;
    use serde::*;
    #[derive(Debug, Serialize, Deserialize, PostgresType)]
    pub struct ProximityPart {
        pub words: Vec<String>,
        pub distance: u32,
        pub in_order: bool,
    }
}

pub struct DocumentHighlighter<'a> {
    lookup: HashMap<String, Vec<TokenEntry>>,
    __marker: PhantomData<&'a TokenEntry>,
}

impl<'a> DocumentHighlighter<'a> {
    pub fn new() -> Self {
        DocumentHighlighter {
            lookup: HashMap::with_capacity(150),
            __marker: PhantomData,
        }
    }

    pub fn analyze_document(&mut self, index: &PgRelation, field: &str, text: &str) {
        let es = Elasticsearch::new(index);
        let results = es
            .analyze_with_field(field, text)
            .execute()
            .expect("failed to analyze text for highlighting");

        for token in results.tokens {
            let entry = self
                .lookup
                .entry(token.token)
                .or_insert(Vec::with_capacity(5));

            entry.push(TokenEntry {
                type_: token.type_,
                position: token.position as u32,
                start_offset: token.start_offset as u64,
                end_offset: token.end_offset as u64,
            });
        }
    }

    pub fn highlight_token(&'a self, token: &str) -> Option<Vec<(String, &'a TokenEntry)>> {
        let mut result = Vec::new();
        let token_entries_vec = self.lookup.get(token);
        match token_entries_vec {
            Some(vec) => {
                for token_entry in vec {
                    result.push((String::from(token), token_entry))
                }
                Some(result)
            }
            None => None,
        }
    }

    pub fn highlight_wildcard(&'a self, token: &str) -> Option<Vec<(String, &'a TokenEntry)>> {
        let _char_looking_for_asterisk = '*';
        let _char_looking_for_question = '?';
        let mut new_regex = String::from("^");
        for char in token.chars() {
            if char == _char_looking_for_question {
                new_regex.push('.')
            } else if char == _char_looking_for_asterisk {
                new_regex.push('.');
                new_regex.push(char);
            } else {
                new_regex.push(char);
            }
        }
        new_regex.push_str("$");
        self.highlight_regex(new_regex.deref())
    }

    pub fn highlight_regex(&'a self, regex: &str) -> Option<Vec<(String, &'a TokenEntry)>> {
        let regex = Regex::new(regex).unwrap();
        let mut result = Vec::new();
        for (key, token_entries) in self.lookup.iter() {
            if regex.is_match(key) {
                for token_entry in token_entries {
                    result.push((key.clone(), token_entry));
                }
            }
        }
        if result.is_empty() {
            None
        } else {
            Some(result)
        }
    }

    pub fn highlight_fuzzy(
        &'a self,
        fuzzy_key: &str,
        prefix: usize,
    ) -> Option<Vec<(String, &'a TokenEntry)>> {
        let mut result = Vec::new();
        let fuzzy_low = 3;
        let fuzzy_high = 6;
        if prefix >= fuzzy_key.len() {
            return self.highlight_token(fuzzy_key);
        }
        let prefix_string = &fuzzy_key[0..prefix];
        for (token, token_entries) in self.lookup.iter() {
            if token.starts_with(prefix_string.deref()) {
                if fuzzy_key.len() < fuzzy_low {
                    if levenshtein(token, fuzzy_key) as i32 == 0 {
                        for token_entry in token_entries {
                            result.push((String::from(token), token_entry));
                        }
                    }
                } else if fuzzy_key.len() >= fuzzy_low && fuzzy_key.len() < fuzzy_high {
                    if levenshtein(token, fuzzy_key) as i32 <= 1 {
                        for token_entry in token_entries {
                            result.push((String::from(token), token_entry));
                        }
                    }
                } else {
                    if levenshtein(token, fuzzy_key) as i32 <= 2 {
                        for token_entry in token_entries {
                            result.push((String::from(token), token_entry));
                        }
                    }
                }
            };
        }
        if result.is_empty() {
            None
        } else {
            Some(result)
        }
    }

    pub fn highlight_phrase(
        &'a self,
        index: PgRelation,
        field: &str,
        phrase_str: &str,
    ) -> Option<Vec<(String, &'a TokenEntry)>> {
        if phrase_str.is_empty() {
            return None;
        }

        let phrase = analyze_with_field(index, field, phrase_str)
            .map(|parts| ProximityPart {
                words: vec![parts.1],
                distance: 0,
                in_order: true,
            })
            .collect::<Vec<_>>();
        if phrase.is_empty() {
            None
        } else {
            self.highlight_proximity(phrase)
        }
    }

    // 'drinking green beer is better than drinking yellow beer which wine is worse than drinking yellow wine'
    //                                     ^^^^^^^^^^^^^^^                          ^^^^^^^^^^^^^^^
    // [ "drinking", "yellow" ]   query= drinking wo/1 yellow
    //
    // query= than w/2 wine
    // query= than wo/2 (wine or beer or cheese or food) w/5 cowbell
    pub fn highlight_proximity(
        &'a self,
        phrase: Vec<ProximityPart>,
    ) -> Option<Vec<(String, &'a TokenEntry)>> {
        if phrase.len() == 0 {
            return None;
        }

        let first_words = phrase.get(0).unwrap();
        let mut final_matches = HashSet::new();

        for word in &first_words.words {
            let first_word_entries = self.highlight_token(&word);

            if phrase.len() == 1 || first_word_entries.is_none() {
                return first_word_entries;
            }

            let first_word_entries = first_word_entries.unwrap().into_iter();
            for e in first_word_entries {
                let mut start = vec![e.1.position]; // 0
                let mut possibilities = Vec::new();
                let mut is_valid = true;

                possibilities.push(e);

                let mut iter = phrase.iter().peekable();
                while let Some(current) = iter.next() {
                    let next = iter.peek();
                    if next.is_none() {
                        break;
                    }
                    let next = next.unwrap();

                    let distance = current.distance;
                    let order = current.in_order;
                    let word = &next.words;

                    match self.look_for_match(word, distance, order, start) {
                        None => {
                            is_valid = false;
                            break;
                        }
                        Some(next_entries) => {
                            start = next_entries
                                .iter()
                                .map(|e| e.1.position)
                                .collect::<Vec<u32>>();
                            next_entries.into_iter().for_each(|e| possibilities.push(e));
                        }
                    }
                }

                if is_valid {
                    possibilities.into_iter().for_each(|e| {
                        final_matches.insert(e);
                    });
                }
            }
        }

        if final_matches.is_empty() {
            None
        } else {
            Some(
                final_matches
                    .into_iter()
                    .collect::<Vec<(String, &TokenEntry)>>(),
            )
        }
    }

    fn look_for_match(
        &self,
        words: &Vec<String>,
        distance: u32,
        order: bool,
        starting_point: Vec<u32>,
    ) -> Option<HashSet<(String, &TokenEntry)>> {
        let mut matches = HashSet::new();
        for word in words {
            match self.highlight_token(word) {
                None => {}
                Some(entries) => {
                    for e in entries {
                        for point in &starting_point {
                            if order {
                                if *point < e.1.position && e.1.position - point <= distance + 1 {
                                    matches.insert(e.clone());
                                }
                            } else {
                                if (*point as i32 - e.1.position as i32).abs()
                                    <= distance as i32 + 1
                                {
                                    matches.insert(e.clone());
                                }
                            }
                        }
                    }
                }
            }
        }
        return if matches.is_empty() {
            None
        } else {
            Some(matches)
        };
    }
}

#[pg_extern(imutable, parallel_safe)]
fn highlight_term(
    index: PgRelation,
    field_name: &str,
    text: &str,
    token_to_highlight: String,
) -> impl std::iter::Iterator<
    Item = (
        name!(field_name, String),
        name!(term, String),
        name!(type, String),
        name!(position, i32),
        name!(start_offset, i64),
        name!(end_offset, i64),
    ),
> {
    let mut highlighter = DocumentHighlighter::new();
    highlighter.analyze_document(&index, field_name, text);
    let highlights = highlighter.highlight_token(&token_to_highlight);

    match highlights {
        Some(vec) => vec
            .iter()
            .map(|e| {
                (
                    field_name.clone().to_owned(),
                    String::from(e.0.clone()),
                    String::from(e.1.type_.clone()),
                    e.1.position as i32,
                    e.1.start_offset as i64,
                    e.1.end_offset as i64,
                )
            })
            .collect::<Vec<(String, String, String, i32, i64, i64)>>()
            .into_iter(),
        None => Vec::<(String, String, String, i32, i64, i64)>::new().into_iter(),
    }
}

#[pg_extern(imutable, parallel_safe)]
fn highlight_phrase(
    index: PgRelation,
    field_name: &str,
    text: &str,
    tokens_to_highlight: &str,
) -> impl std::iter::Iterator<
    Item = (
        name!(field_name, String),
        name!(term, String),
        name!(type, String),
        name!(position, i32),
        name!(start_offset, i64),
        name!(end_offset, i64),
    ),
> {
    let mut highlighter = DocumentHighlighter::new();
    highlighter.analyze_document(&index, field_name, text);
    let highlights = highlighter.highlight_phrase(index, field_name, tokens_to_highlight);

    match highlights {
        Some(vec) => vec
            .iter()
            .map(|e| {
                (
                    field_name.clone().to_owned(),
                    String::from(e.0.clone()),
                    String::from(e.1.type_.clone()),
                    e.1.position as i32,
                    e.1.start_offset as i64,
                    e.1.end_offset as i64,
                )
            })
            .collect::<Vec<(String, String, String, i32, i64, i64)>>()
            .into_iter(),
        None => Vec::<(String, String, String, i32, i64, i64)>::new().into_iter(),
    }
}

#[pg_extern(imutable, parallel_safe)]
fn highlight_wildcard(
    index: PgRelation,
    field_name: &str,
    text: &str,
    token_to_highlight: &str,
) -> impl std::iter::Iterator<
    Item = (
        name!(field_name, String),
        name!(term, String),
        name!(type, String),
        name!(position, i32),
        name!(start_offset, i64),
        name!(end_offset, i64),
    ),
> {
    let mut highlighter = DocumentHighlighter::new();
    highlighter.analyze_document(&index, field_name, text);
    let highlights = highlighter.highlight_wildcard(token_to_highlight);

    match highlights {
        Some(vec) => vec
            .iter()
            .map(|e| {
                (
                    field_name.clone().to_owned(),
                    String::from(e.0.clone()),
                    String::from(e.1.type_.clone()),
                    e.1.position as i32,
                    e.1.start_offset as i64,
                    e.1.end_offset as i64,
                )
            })
            .collect::<Vec<(String, String, String, i32, i64, i64)>>()
            .into_iter(),
        None => Vec::<(String, String, String, i32, i64, i64)>::new().into_iter(),
    }
}

#[pg_extern(imutable, parallel_safe)]
fn highlight_regex(
    index: PgRelation,
    field_name: &str,
    text: &str,
    token_to_highlight: &str,
) -> impl std::iter::Iterator<
    Item = (
        name!(field_name, String),
        name!(term, String),
        name!(type, String),
        name!(position, i32),
        name!(start_offset, i64),
        name!(end_offset, i64),
    ),
> {
    let mut highlighter = DocumentHighlighter::new();
    highlighter.analyze_document(&index, field_name, text);
    let highlights = highlighter.highlight_regex(token_to_highlight);

    match highlights {
        Some(vec) => vec
            .iter()
            .map(|e| {
                (
                    field_name.clone().to_owned(),
                    String::from(e.0.clone()),
                    String::from(e.1.type_.clone()),
                    e.1.position as i32,
                    e.1.start_offset as i64,
                    e.1.end_offset as i64,
                )
            })
            .collect::<Vec<(String, String, String, i32, i64, i64)>>()
            .into_iter(),
        None => Vec::<(String, String, String, i32, i64, i64)>::new().into_iter(),
    }
}

#[pg_extern(imutable, parallel_safe)]
fn highlight_fuzzy(
    index: PgRelation,
    field_name: &str,
    text: &str,
    token_to_highlight: &str,
    prefix: i32,
) -> impl std::iter::Iterator<
    Item = (
        name!(field_name, String),
        name!(term, String),
        name!(type, String),
        name!(position, i32),
        name!(start_offset, i64),
        name!(end_offset, i64),
    ),
> {
    if prefix < 0 {
        panic!("negative prefixes not allowed");
    }
    let mut highlighter = DocumentHighlighter::new();
    highlighter.analyze_document(&index, field_name, text);
    let highlights = highlighter.highlight_fuzzy(token_to_highlight, prefix as usize);

    match highlights {
        Some(vec) => vec
            .iter()
            .map(|e| {
                (
                    field_name.clone().to_owned(),
                    String::from(e.0.clone()),
                    String::from(e.1.type_.clone()),
                    e.1.position as i32,
                    e.1.start_offset as i64,
                    e.1.end_offset as i64,
                )
            })
            .collect::<Vec<(String, String, String, i32, i64, i64)>>()
            .into_iter(),
        None => Vec::<(String, String, String, i32, i64, i64)>::new().into_iter(),
    }
}

//  select zdb.highlight_proximity('idx_test','test','this is a test', ARRAY['{"word": "this", distance:2, in_order: false}'::proximitypart, '{"word": "test", distance: 0, in_order: false}'::proximitypart]);
#[pg_extern(immutable, parallel_safe)]
fn highlight_proximity(
    index: PgRelation,
    field_name: &str,
    text: &str,
    prox_clause: Vec<Option<ProximityPart>>,
) -> impl std::iter::Iterator<
    Item = (
        name!(field_name, String),
        name!(term, String),
        name!(type, String),
        name!(position, i32),
        name!(start_offset, i64),
        name!(end_offset, i64),
    ),
> {
    let prox_clause = prox_clause
        .into_iter()
        .map(|e| e.unwrap())
        .collect::<Vec<ProximityPart>>();
    let mut highlighter = DocumentHighlighter::new();
    highlighter.analyze_document(&index, field_name, text);
    let highlights = highlighter.highlight_proximity(prox_clause);

    match highlights {
        Some(vec) => vec
            .iter()
            .map(|e| {
                (
                    field_name.clone().to_owned(),
                    String::from(e.0.clone()),
                    String::from(e.1.type_.clone()),
                    e.1.position as i32,
                    e.1.start_offset as i64,
                    e.1.end_offset as i64,
                )
            })
            .collect::<Vec<(String, String, String, i32, i64, i64)>>()
            .into_iter(),
        None => Vec::<(String, String, String, i32, i64, i64)>::new().into_iter(),
    }
}

#[cfg(any(test, feature = "pg_test"))]
mod tests {
    use crate::highlighting::document_highlighter::{DocumentHighlighter, TokenEntry};
    use pgx::*;
    use std::collections::HashSet;

    #[pg_test(error = "no matches found")]
    #[initialize(es = true)]
    fn test_look_for_match_none() {
        let title = "look_for_match_none";
        start_table_and_index(title);
        let mut dh = DocumentHighlighter::new();

        let index = unsafe {
            PgRelation::open_with_name("idxtest_highlighting_look_for_match_none").unwrap()
        };
        dh.analyze_document(&index, "test_field", "this is a test");

        let matches = dh
            .look_for_match(&vec![String::from("test")], 1, true, vec![0])
            .expect("no matches found");
        matches.is_empty();
    }

    #[pg_test]
    #[initialize(es = true)]
    fn test_look_for_match_in_order_one() {
        let title = "look_for_match_in_order";
        start_table_and_index(title);
        let mut dh = DocumentHighlighter::new();

        let index = unsafe {
            PgRelation::open_with_name("idxtest_highlighting_look_for_match_in_order").unwrap()
        };
        dh.analyze_document(&index, "test_field", "this is a test");

        let matches = dh
            .look_for_match(&vec![String::from("is")], 0, true, vec![0])
            .expect("no matches found");
        let mut expected = HashSet::new();
        let value = (
            "is".to_string(),
            &TokenEntry {
                type_: "<ALPHANUM>".to_string(),
                position: 1,
                start_offset: 5,
                end_offset: 7,
            },
        );
        expected.insert(value);
        assert_eq!(matches, expected)
    }

    #[pg_test]
    #[initialize(es = true)]
    fn test_look_for_match_out_of_order_one() {
        let title = "look_for_match_out_of_order";
        start_table_and_index(title);
        let mut dh = DocumentHighlighter::new();
        let index = unsafe {
            PgRelation::open_with_name("idxtest_highlighting_look_for_match_out_of_order").unwrap()
        };
        dh.analyze_document(&index, "test_field", "this is a test");

        let matches = dh
            .look_for_match(&vec![String::from("this")], 0, false, vec![1])
            .expect("no matches found");
        let mut expected = HashSet::new();
        let value_one = (
            "this".to_string(),
            &TokenEntry {
                type_: "<ALPHANUM>".to_string(),
                position: 0,
                start_offset: 0,
                end_offset: 4,
            },
        );
        expected.insert(value_one);
        assert_eq!(matches, expected)
    }

    #[pg_test]
    #[initialize(es = true)]
    fn test_look_for_match_in_order_two() {
        let title = "look_for_match_out_of_order_two";
        start_table_and_index(title);
        let mut dh = DocumentHighlighter::new();
        let index = unsafe {
            PgRelation::open_with_name("idxtest_highlighting_look_for_match_out_of_order_two")
                .unwrap()
        };
        dh.analyze_document(
            &index,
            "test_field",
            "this is a test and this is also a test",
        );

        let matches = dh
            .look_for_match(&vec![String::from("is")], 0, false, vec![0, 5])
            .expect("no matches found");
        let mut expect = HashSet::new();
        let value_one = (
            "is".to_string(),
            &TokenEntry {
                type_: "<ALPHANUM>".to_string(),
                position: 1,
                start_offset: 5,
                end_offset: 7,
            },
        );
        let value_two = (
            "is".to_string(),
            &TokenEntry {
                type_: "<ALPHANUM>".to_string(),
                position: 6,
                start_offset: 24,
                end_offset: 26,
            },
        );
        expect.insert(value_one);
        expect.insert(value_two);
        assert_eq!(matches, expect)
    }

    #[pg_test]
    #[initialize(es = true)]
    fn test_look_for_match_out_of_order_two() {
        let title = "look_for_match_out_of_order_two";
        start_table_and_index(title);
        let mut dh = DocumentHighlighter::new();
        let index = unsafe {
            PgRelation::open_with_name("idxtest_highlighting_look_for_match_out_of_order_two")
                .unwrap()
        };
        dh.analyze_document(
            &index,
            "test_field",
            "this is a test and this is also a test",
        );

        let matches = dh
            .look_for_match(&vec![String::from("this")], 0, false, vec![1, 6])
            .expect("no matches found");
        let mut expect = HashSet::new();
        let value_one = (
            "this".to_string(),
            &TokenEntry {
                type_: "<ALPHANUM>".to_string(),
                position: 0,
                start_offset: 0,
                end_offset: 4,
            },
        );
        let value_two = (
            "this".to_string(),
            &TokenEntry {
                type_: "<ALPHANUM>".to_string(),
                position: 5,
                start_offset: 19,
                end_offset: 23,
            },
        );
        expect.insert(value_one);
        expect.insert(value_two);
        assert_eq!(matches, expect)
    }

    #[pg_test]
    #[initialize(es = true)]
    fn test_look_for_match_in_order_two_different_dist() {
        let title = "look_for_match_in_order_two_diff_dist";
        start_table_and_index(title);
        let mut dh = DocumentHighlighter::new();
        let index = unsafe {
            PgRelation::open_with_name("idxtest_highlighting_look_for_match_in_order_two_diff_dist")
                .unwrap()
        };
        dh.analyze_document(
            &index,
            "test_field",
            "this is a test and this is also a test",
        );

        let matches = dh
            .look_for_match(&vec![String::from("test")], 3, true, vec![0, 5])
            .expect("no matches found");
        let mut expect = HashSet::new();
        let value_one = (
            "test".to_string(),
            &TokenEntry {
                type_: "<ALPHANUM>".to_string(),
                position: 3,
                start_offset: 10,
                end_offset: 14,
            },
        );
        let value_two = (
            "test".to_string(),
            &TokenEntry {
                type_: "<ALPHANUM>".to_string(),
                position: 9,
                start_offset: 34,
                end_offset: 38,
            },
        );
        expect.insert(value_one);
        expect.insert(value_two);
        assert_eq!(matches, expect)
    }

    #[pg_test]
    #[initialize(es = true)]
    fn test_look_for_match_out_of_order_two_diff_dist() {
        let title = "look_for_match_out_of_order_two_diff_dist";
        start_table_and_index(title);
        let mut dh = DocumentHighlighter::new();
        let index = unsafe {
            PgRelation::open_with_name(
                "idxtest_highlighting_look_for_match_out_of_order_two_diff_dist",
            )
            .unwrap()
        };
        dh.analyze_document(
            &index,
            "test_field",
            "this is a test and this is also a test",
        );

        let matches = dh
            .look_for_match(&vec![String::from("this")], 3, false, vec![3, 9])
            .expect("no matches found");
        let mut expect = HashSet::new();
        let value_one = (
            "this".to_string(),
            &TokenEntry {
                type_: "<ALPHANUM>".to_string(),
                position: 0,
                start_offset: 0,
                end_offset: 4,
            },
        );
        let value_two = (
            "this".to_string(),
            &TokenEntry {
                type_: "<ALPHANUM>".to_string(),
                position: 5,
                start_offset: 19,
                end_offset: 23,
            },
        );
        expect.insert(value_one);
        expect.insert(value_two);
        assert_eq!(matches, expect)
    }

    #[pg_test]
    #[initialize(es = true)]
    fn test_highlighter_term() {
        let title = "term";
        start_table_and_index(title);
        let select: String = format!(
            "select * from zdb.highlight_term('idxtest_highlighting_{}', 'test_field', 'it is a test and it is a good one', 'it') order by position;",title
        );
        Spi::connect(|client| {
            let table = client.select(&select, None, None);

            // field_name | term |    type    | position | start_offset | end_offset
            // ------------+------+------------+----------+--------------+------------
            // name       | it   | <ALPHANUM> |        0 |            0 |          2
            // name       | it   | <ALPHANUM> |        5 |           17 |         19
            let expect = vec![
                ("<ALPHANUM>", "it", 0, 0, 2),
                ("<ALPHANUM>", "it", 5, 17, 19),
            ];

            test_table(table, expect);

            Ok(Some(()))
        });
    }

    #[pg_test]
    #[initialize(es = true)]
    fn test_highlighter_phrase() {
        let title = "phrase";
        start_table_and_index(title);
        let select:String = format!("select * from zdb.highlight_phrase('idxtest_highlighting_{}', 'test_field', 'it is a test and it is a good one', 'it is a') order by position;",title);
        Spi::connect(|client| {
            let table = client.select(&select, None, None);

            // field_name | term |    type    | position | start_offset | end_offset
            // ------------+------+------------+----------+--------------+------------
            // test_field | it   | <ALPHANUM> |        0 |            0 |          2
            // test_field | is   | <ALPHANUM> |        1 |            3 |          5
            // test_field | a    | <ALPHANUM> |        2 |            6 |          7
            // test_field | it   | <ALPHANUM> |        5 |           17 |         19
            // test_field | is   | <ALPHANUM> |        6 |           20 |         22
            // test_field | a    | <ALPHANUM> |        7 |           23 |         24
            let expect = vec![
                ("<ALPHANUM>", "it", 0, 0, 2),
                ("<ALPHANUM>", "is", 1, 3, 5),
                ("<ALPHANUM>", "a", 2, 6, 7),
                ("<ALPHANUM>", "it", 5, 17, 19),
                ("<ALPHANUM>", "is", 6, 20, 22),
                ("<ALPHANUM>", "a", 7, 23, 24),
            ];

            test_table(table, expect);

            Ok(Some(()))
        });
    }

    #[pg_test]
    #[initialize(es = true)]
    fn test_highlighter_phrase_as_one_word() {
        let title = "phrase_one_word";
        start_table_and_index(title);
        let select:String = format!("select * from zdb.highlight_phrase('idxtest_highlighting_{}', 'test_field', 'it is a test and it is a good one', 'it') order by position;",title);
        Spi::connect(|client| {
            let table = client.select(&select, None, None);

            // field_name | term |    type    | position | start_offset | end_offset
            // ------------+------+------------+----------+--------------+------------
            // test_field | it   | <ALPHANUM> |        0 |            0 |          2
            // test_field | it   | <ALPHANUM> |        5 |           17 |         19
            let expect = vec![
                ("<ALPHANUM>", "it", 0, 0, 2),
                ("<ALPHANUM>", "it", 5, 17, 19),
            ];

            test_table(table, expect);

            Ok(Some(()))
        });
    }

    #[pg_test]
    #[initialize(es = true)]
    fn test_highlighter_phrase_with_phrase_not_in_text() {
        let title = "phrase_not_in_text";
        start_table_and_index(title);
        let select :String = format!("select * from zdb.highlight_phrase('idxtest_highlighting_{}', 'test_field', 'it is a test and it is a good one', 'banana') order by position;",title);
        Spi::connect(|client| {
            let table = client.select(&select, None, None);

            // field_name | term |    type    | position | start_offset | end_offset
            // ------------+------+------------+----------+--------------+------------
            let expect = vec![];

            test_table(table, expect);

            Ok(Some(()))
        });
    }

    #[pg_test]
    #[initialize(es = true)]
    fn test_highlighter_wildcard_with_asterisk() {
        let title = "wildcard_ast";
        start_table_and_index(title);
        let select = format!("select * from zdb.highlight_wildcard('idxtest_highlighting_{}', 'test_field', 'Mom landed a man on the moon', 'm*n') order by position;",title);
        Spi::connect(|client| {
            let table = client.select(&select, None, None);

            // field_name  | term |    type    | position | start_offset | end_offset
            // ------------+------+------------+----------+--------------+------------
            //  test_field | man  | <ALPHANUM> |        3 |           13 |         16
            //  test_field | moon | <ALPHANUM> |        6 |           24 |         28
            let expect = vec![
                ("<ALPHANUM>", "man", 3, 13, 16),
                ("<ALPHANUM>", "moon", 6, 24, 28),
            ];

            test_table(table, expect);

            Ok(Some(()))
        });
    }

    #[pg_test]
    #[initialize(es = true)]
    fn test_highlighter_wildcard_with_question_mark() {
        let title = "wildcard_question";
        start_table_and_index(title);
        let select = format!("select * from zdb.highlight_wildcard('idxtest_highlighting_{}', 'test_field', 'Mom landed a man on the moon', 'm?n') order by position;",title);
        Spi::connect(|client| {
            let table = client.select(&select, None, None);

            // field_name  | term |    type    | position | start_offset | end_offset
            // ------------+------+------------+----------+--------------+------------
            //  test_field | man  | <ALPHANUM> |        3 |           13 |         16
            let expect = vec![("<ALPHANUM>", "man", 3, 13, 16)];

            test_table(table, expect);

            Ok(Some(()))
        });
    }

    #[pg_test]
    #[initialize(es = true)]
    fn test_highlighter_wildcard_with_no_match() {
        let title = "wildcard_no_match";
        start_table_and_index(title);
        let select = format!("select * from zdb.highlight_wildcard('idxtest_highlighting_{}', 'test_field', 'Mom landed a man on the moon', 'n*n') order by position;",title);
        Spi::connect(|client| {
            let table = client.select(&select, None, None);

            // field_name  | term |    type    | position | start_offset | end_offset
            // ------------+------+------------+----------+--------------+------------
            let expect = vec![];

            test_table(table, expect);

            Ok(Some(()))
        });
    }

    #[pg_test]
    #[initialize(es = true)]
    fn test_highlighter_regex() {
        let title = "regex";
        start_table_and_index(title);
        let select = format!("select * from zdb.highlight_wildcard('idxtest_highlighting_{}', 'test_field', 'Mom landed a man on the moon', '^m.*$') order by position;", title);
        Spi::connect(|client| {
            let table = client.select(&select, None, None);

            // field_name | term |    type    | position | start_offset | end_offset
            // -----------+------+------------+----------+--------------+------------
            // test_field | mom  | <ALPHANUM> |        0 |            0 |          3
            // test_field | man  | <ALPHANUM> |        3 |           13 |         16
            // test_field | moon | <ALPHANUM> |        6 |           24 |         28
            let expect = vec![
                ("<ALPHANUM>", "mom", 0, 0, 3),
                ("<ALPHANUM>", "man", 3, 13, 16),
                ("<ALPHANUM>", "moon", 6, 24, 28),
            ];

            test_table(table, expect);

            Ok(Some(()))
        });
    }

    #[pg_test]
    #[initialize(es = true)]
    fn test_highlighter_fuzzy_correct_three_char_term() {
        let title = "fuzzy_three";
        start_table_and_index(title);
        let select = format!("select * from zdb.highlight_fuzzy('idxtest_highlighting_{}', 'test_field', 'coal colt cot cheese beer co beer colter cat bolt c', 'cot', 1) order by position;",title);
        Spi::connect(|client| {
            let table = client.select(&select, None, None);

            // field_name  | term |    type    | position | start_offset | end_offset
            // ------------+------+------------+----------+--------------+------------
            // test_field | coal | <ALPHANUM> |        0 |            0 |          4
            // test_field | colt | <ALPHANUM> |        1 |            5 |          9
            // test_field | cot  | <ALPHANUM> |        2 |           10 |         13
            // test_field | co   | <ALPHANUM> |        5 |           26 |         28
            let expect = vec![
                ("<ALPHANUM>", "colt", 1, 5, 9),
                ("<ALPHANUM>", "cot", 2, 10, 13),
                ("<ALPHANUM>", "co", 5, 26, 28),
                ("<ALPHANUM>", "cat", 8, 41, 44),
            ];

            test_table(table, expect);

            Ok(Some(()))
        });
    }

    #[pg_test]
    #[initialize(es = true)]
    fn test_highlighter_fuzzy_correct_two_char_string() {
        let title = "fuzzy_two";
        start_table_and_index(title);
        let select = format!("select * from zdb.highlight_fuzzy('idxtest_highlighting_{}', 'test_field', 'coal colt cot cheese beer co beer colter cat bolt c', 'co', 1) order by position;",title);
        Spi::connect(|client| {
            let table = client.select(&select, None, None);

            // field_name  | term |    type    | position | start_offset | end_offset
            // ------------+------+------------+----------+--------------+------------
            // test_field | co   | <ALPHANUM> |        5 |           26 |         28
            let expect = vec![("<ALPHANUM>", "co", 5, 26, 28)];

            test_table(table, expect);

            Ok(Some(()))
        });
    }

    #[pg_test]
    #[initialize(es = true)]
    fn test_highlighter_fuzzy_6_char_string() {
        let title = "fuzzy_six";
        start_table_and_index(title);
        let select = format!("select * from zdb.highlight_fuzzy('idxtest_highlighting_{}', 'test_field', 'coal colt cot cheese beer co beer colter cat bolt c cott cooler', 'colter', 2) order by position;",title);
        Spi::connect(|client| {
            let table = client.select(&select, None, None);

            // field_name | term   |    type    | position | start_offset | end_offset
            // -----------+--------+------------+----------+--------------+------------
            // test_field | colt   | <ALPHANUM> |        1 |            5 |          9
            // test_field | colter | <ALPHANUM> |        7 |           34 |         40
            // test_field | cooler | <ALPHANUM> |       12 |           57 |         63

            let expect = vec![
                ("<ALPHANUM>", "colt", 1, 5, 9),
                ("<ALPHANUM>", "colter", 7, 34, 40),
                ("<ALPHANUM>", "cooler", 12, 57, 63),
            ];

            test_table(table, expect);

            Ok(Some(()))
        });
    }

    #[pg_test]
    #[initialize(es = true)]
    fn test_highlighter_fuzzy_with_prefix_number_longer_then_given_string() {
        let title = "fuzzy_long_prefix";
        start_table_and_index(title);
        let select = format!("select * from zdb.highlight_fuzzy('idxtest_highlighting_{}', 'test_field', 'coal colt cot cheese beer co beer colter cat bolt', 'cot', 4) order by position;",title);
        Spi::connect(|client| {
            let table = client.select(&select, None, None);

            // field_name | term |    type    | position | start_offset | end_offset
            // -----------+------+------------+----------+--------------+------------
            // test_field | cot  | <ALPHANUM> |        2 |           10 |         13

            let expect = vec![("<ALPHANUM>", "cot", 2, 10, 13)];

            test_table(table, expect);

            Ok(Some(()))
        });
    }

    #[pg_test]
    #[initialize(es = true)]
    fn test_highlighter_fuzzy_with_prefix_number_longer_then_given_string_with_non_return() {
        let title = "fuzzy_long_prefix_no_return";
        start_table_and_index(title);
        let select = format!("select * from zdb.highlight_fuzzy('idxtest_highlighting_{}', 'test_field', 'coal colt cot cheese beer co beer colter cat bolt', 'cet', 4) order by position;",title);
        Spi::connect(|client| {
            let table = client.select(&select, None, None);

            // field_name | term |    type    | position | start_offset | end_offset
            // -----------+------+------------+----------+--------------+------------

            let expect = vec![];

            test_table(table, expect);

            Ok(Some(()))
        });
    }

    #[pg_test(error = "negative prefixes not allowed")]
    #[initialize(es = true)]
    fn test_highlighter_fuzzy_with_negative_prefix() {
        let title = "fuzzy_neg_prefix";
        start_table_and_index(title);
        let select = format!("select * from zdb.highlight_fuzzy('idxtest_highlighting_{}', 'test_field', 'coal colt cot cheese beer co beer colter cat bolt', 'cet', -4) order by position;",title);
        Spi::connect(|client| {
            let table = client.select(&select, None, None);

            // field_name | term |    type    | position | start_offset | end_offset
            // -----------+------+------------+----------+--------------+------------

            let expect = vec![];

            test_table(table, expect);

            Ok(Some(()))
        });
    }

    #[pg_test]
    #[initialize(es = true)]
    fn test_highlighter_proximity_two_term() {
        let title = "highlight_proximity_two_term";
        start_table_and_index(title);
        let array_one = "{\"words\": [\"this\"], \"distance\": 2, \"in_order\": false}";
        let array_two = "{\"words\": [\"test\"], \"distance\": 0, \"in_order\": false}";
        let select = format!("select * from zdb.highlight_proximity('idxtest_highlighting_{}', 'test_field','this is a test that is longer and has a second this near test a second time and a third this that is not' ,  ARRAY['{}'::proximitypart, '{}'::proximitypart]) order by position;", title, array_one, array_two);
        Spi::connect(|client| {
            let table = client.select(&select, None, None);

            // field_name | term |    type    | position | start_offset | end_offset
            // -----------+------+------------+----------+--------------+------------
            // test_field | this | <ALPHANUM> |        0 |            0 |          4
            // test_field | test | <ALPHANUM> |        3 |           10 |         14
            // test_field | this | <ALPHANUM> |       11 |           47 |         51
            // test_field | test | <ALPHANUM> |       13 |           57 |         61
            let expect = vec![
                ("<ALPHANUM>", "this", 0, 0, 4),
                ("<ALPHANUM>", "test", 3, 10, 14),
                ("<ALPHANUM>", "this", 11, 47, 51),
                ("<ALPHANUM>", "test", 13, 57, 61),
            ];

            test_table(table, expect);

            Ok(Some(()))
        });
    }

    #[pg_test]
    #[initialize(es = true)]
    fn test_highlighter_proximity_three_term() {
        let title = "highlight_proximity_three_term";
        start_table_and_index(title);
        let search_string = "this is a test that is longer and has a second this near test a second time and a third this that is not";
        let array_one = "{\"words\": [\"this\"], \"distance\": 2, \"in_order\": false}";
        let array_two = "{\"words\": [\"test\"], \"distance\": 0, \"in_order\": false}";
        let array_three = "{\"words\": [\"that\"], \"distance\": 2, \"in_order\": false}";
        let select = format!("select * from zdb.highlight_proximity('idxtest_highlighting_{}', 'test_field','{}' ,  ARRAY['{}'::proximitypart, '{}'::proximitypart, '{}'::proximitypart]) order by position;", title,search_string, array_one, array_two,array_three);
        Spi::connect(|client| {
            let table = client.select(&select, None, None);

            // field_name | term |    type    | position | start_offset | end_offset
            // ------------+------+------------+----------+--------------+------------
            // test_field | this | <ALPHANUM> |        0 |            0 |          4
            // test_field | test | <ALPHANUM> |        3 |           10 |         14
            // test_field | that | <ALPHANUM> |        4 |           15 |         19

            let expect = vec![
                ("<ALPHANUM>", "this", 0, 0, 4),
                ("<ALPHANUM>", "test", 3, 10, 14),
                ("<ALPHANUM>", "that", 4, 15, 19),
            ];

            test_table(table, expect);

            Ok(Some(()))
        });
    }

    #[pg_test]
    #[initialize(es = true)]
    fn test_highlighter_proximity_one_term() {
        let title = "highlight_proximity_one_term";
        start_table_and_index(title);
        let search_string = "this is a test that is longer and has a second this near test a second time and a third this that is not";
        let array_one = "{\"words\": [\"this\"], \"distance\": 2, \"in_order\": false}";
        let select = format!("select * from zdb.highlight_proximity('idxtest_highlighting_{}', 'test_field','{}' ,  ARRAY['{}'::proximitypart]) order by position;", title,search_string, array_one);
        Spi::connect(|client| {
            let table = client.select(&select, None, None);

            // field_name | term |    type    | position | start_offset | end_offset
            // -----------+------+------------+----------+--------------+------------
            // test_field | this | <ALPHANUM> |        0 |            0 |          4
            // test_field | this | <ALPHANUM> |       11 |           47 |         51
            // test_field | this | <ALPHANUM> |       21 |           88 |         92

            let expect = vec![
                ("<ALPHANUM>", "this", 0, 0, 4),
                ("<ALPHANUM>", "this", 11, 47, 51),
                ("<ALPHANUM>", "this", 20, 88, 92),
            ];

            test_table(table, expect);

            Ok(Some(()))
        });
    }

    #[pg_test]
    #[initialize(es = true)]
    fn test_highlighter_proximity_three_term_found_twice() {
        let title = "highlight_proximity_three_term_twice";
        start_table_and_index(title);
        let search_string = "this is a test that is longer and has a second this near test a second time and a third that is not this test that whatever ";
        let array_one = "{\"words\": [\"this\"], \"distance\": 2, \"in_order\": false}";
        let array_two = "{\"words\": [\"test\"], \"distance\": 0, \"in_order\": false}";
        let array_three = "{\"words\": [\"that\"], \"distance\": 2, \"in_order\": false}";
        let select = format!("select * from zdb.highlight_proximity('idxtest_highlighting_{}', 'test_field','{}' ,  ARRAY['{}'::proximitypart, '{}'::proximitypart, '{}'::proximitypart]) order by position;", title,search_string, array_one, array_two,array_three);
        Spi::connect(|client| {
            let table = client.select(&select, None, None);

            // field_name | term |    type    | position | start_offset | end_offset
            // -----------+------+------------+----------+--------------+------------
            // test_field | this | <ALPHANUM> |        0 |            0 |          4
            // test_field | test | <ALPHANUM> |        3 |           10 |         14
            // test_field | that | <ALPHANUM> |        4 |           15 |         19
            // test_field | this | <ALPHANUM> |       23 |          100 |        104
            // test_field | test | <ALPHANUM> |       24 |          105 |        109
            // test_field | that | <ALPHANUM> |       25 |          110 |        114

            let expect = vec![
                ("<ALPHANUM>", "this", 0, 0, 4),
                ("<ALPHANUM>", "test", 3, 10, 14),
                ("<ALPHANUM>", "that", 4, 15, 19),
                ("<ALPHANUM>", "this", 23, 100, 104),
                ("<ALPHANUM>", "test", 24, 105, 109),
                ("<ALPHANUM>", "that", 25, 110, 114),
            ];

            test_table(table, expect);

            Ok(Some(()))
        });
    }

    #[pg_test]
    #[initialize(es = true)]
    fn test_highlighter_proximity_simple_in_order_test() {
        let title = "highlight_proximity_simple_in_order_test";
        start_table_and_index(title);
        let search_string = "this is this";
        let array_one = "{\"words\": [\"this\"], \"distance\": 2, \"in_order\": true}";
        let array_two = "{\"words\": [\"is\"], \"distance\": 0, \"in_order\": true}";
        let select = format!("select * from zdb.highlight_proximity('idxtest_highlighting_{}', 'test_field','{}' ,  ARRAY['{}'::proximitypart, '{}'::proximitypart]) order by position;", title,search_string, array_one, array_two);
        Spi::connect(|client| {
            let table = client.select(&select, None, None);

            // field_name | term |    type    | position | start_offset | end_offset
            // -----------+------+------------+----------+--------------+------------
            // test_field | this | <ALPHANUM> |        0 |            0 |          4
            // test_field | is   | <ALPHANUM> |        1 |            5 |          7

            let expect = vec![
                ("<ALPHANUM>", "this", 0, 0, 4),
                ("<ALPHANUM>", "is", 1, 5, 7),
            ];

            test_table(table, expect);

            Ok(Some(()))
        });
    }

    #[pg_test]
    #[initialize(es = true)]
    fn test_highlighter_proximity_simple_without_order_test() {
        let title = "highlight_proximity_simple_without_order_test";
        start_table_and_index(title);
        let search_string = "this is this";
        let array_one = "{\"words\": [\"this\"], \"distance\": 2, \"in_order\": false}";
        let array_two = "{\"words\": [\"is\"], \"distance\": 0, \"in_order\": true}";
        let select = format!("select * from zdb.highlight_proximity('idxtest_highlighting_{}', 'test_field','{}' ,  ARRAY['{}'::proximitypart, '{}'::proximitypart]) order by position;", title,search_string, array_one, array_two);
        Spi::connect(|client| {
            let table = client.select(&select, None, None);

            // field_name | term |    type    | position | start_offset | end_offset
            // -----------+------+------------+----------+--------------+------------
            // test_field | this | <ALPHANUM> |        0 |            0 |          4
            // test_field | is   | <ALPHANUM> |        1 |            5 |          7
            // test_field | this | <ALPHANUM> |        2 |            8 |         12

            let expect = vec![
                ("<ALPHANUM>", "this", 0, 0, 4),
                ("<ALPHANUM>", "is", 1, 5, 7),
                ("<ALPHANUM>", "this", 2, 8, 12),
            ];

            test_table(table, expect);

            Ok(Some(()))
        });
    }

    #[pg_test]
    #[initialize(es = true)]
    fn test_highlighter_proximity_four_with_inorder_and_not_inorder() {
        let title = "highlight_proximity_four_with_inorder_and_not_inorder";
        start_table_and_index(title);
        let search_string = "now is the time for all good men to come to the aid of their country.";
        let array_one = "{\"words\": [\"for\"], \"distance\": 2, \"in_order\": true}";
        let array_two = "{\"words\": [\"men\"], \"distance\": 2, \"in_order\": true}";
        let array_three = "{\"words\": [\"to\"], \"distance\": 12, \"in_order\": false}";
        let array_four = "{\"words\": [\"now\"], \"distance\": 2, \"in_order\": false}";
        let select = format!("select * from zdb.highlight_proximity('idxtest_highlighting_{}', 'test_field','{}' ,  ARRAY['{}'::proximitypart, '{}'::proximitypart, '{}'::proximitypart, '{}'::proximitypart]) order by position;", title,search_string, array_one, array_two,array_three, array_four);
        Spi::connect(|client| {
            let table = client.select(&select, None, None);

            // field_name | term |    type    | position | start_offset | end_offset
            // -----------+------+------------+----------+--------------+------------
            // test_field | now  | <ALPHANUM> |        0 |            0 |          3
            // test_field | for  | <ALPHANUM> |        4 |           16 |         19
            // test_field | men  | <ALPHANUM> |        7 |           29 |         32
            // test_field | to   | <ALPHANUM> |        8 |           33 |         35
            // test_field | to   | <ALPHANUM> |       10 |           41 |         43
            let expect = vec![
                ("<ALPHANUM>", "now", 0, 0, 3),
                ("<ALPHANUM>", "for", 4, 16, 19),
                ("<ALPHANUM>", "men", 7, 29, 32),
                ("<ALPHANUM>", "to", 8, 33, 35),
                ("<ALPHANUM>", "to", 10, 41, 43),
            ];

            test_table(table, expect);

            Ok(Some(()))
        });
    }

    #[pg_test]
    #[initialize(es = true)]
    fn test_highlighter_proximity_four_with_inorder_and_not_inorder_doubles_far_apart() {
        let title = "highlight_proximity_four_with_inorder_and_not_inorder_double";
        start_table_and_index(title);
        let search_string = "now is the time of the year for all good men to rise up and come to the aid of their country.";
        let array_one = "{\"words\": [\"for\"], \"distance\": 2, \"in_order\": true}";
        let array_two = "{\"words\": [\"men\"], \"distance\": 5, \"in_order\": true}";
        let array_three = "{\"words\": [\"to\"], \"distance\": 11, \"in_order\": false}";
        let array_four = "{\"words\": [\"now\"], \"distance\": 2, \"in_order\": false}";
        let select = format!("select * from zdb.highlight_proximity('idxtest_highlighting_{}', 'test_field','{}' ,  ARRAY['{}'::proximitypart, '{}'::proximitypart, '{}'::proximitypart, '{}'::proximitypart]) order by position;", title,search_string, array_one, array_two,array_three, array_four);
        Spi::connect(|client| {
            let table = client.select(&select, None, None);

            // field_name | term |    type    | position | start_offset | end_offset
            // -----------+------+------------+----------+--------------+------------
            // test_field | now  | <ALPHANUM> |        0 |            0 |          3
            // test_field | for  | <ALPHANUM> |        7 |           28 |         31
            // test_field | men  | <ALPHANUM> |       10 |           41 |         44
            // test_field | to   | <ALPHANUM> |       11 |           45 |         47
            // test_field | to   | <ALPHANUM> |       16 |           65 |         67
            let expect = vec![
                ("<ALPHANUM>", "now", 0, 0, 3),
                ("<ALPHANUM>", "for", 7, 28, 31),
                ("<ALPHANUM>", "men", 10, 41, 44),
                ("<ALPHANUM>", "to", 11, 45, 47),
                ("<ALPHANUM>", "to", 16, 65, 67),
            ];

            test_table(table, expect);

            Ok(Some(()))
        });
    }

    #[pg_test]
    #[initialize(es = true)]
    fn test_highlighter_proximity_long_test() {
        let title = "highlight_proximity_long_test";
        start_table_and_index(title);
        let search_string = test_blurb();
        let array_one = "{\"words\": [\"energy\"], \"distance\": 3, \"in_order\": false}";
        let array_two = "{\"words\": [\"enron\"], \"distance\": 3, \"in_order\": false}";
        let array_three = "{\"words\": [\"lay\"], \"distance\": 3, \"in_order\": false}";
        let select = format!("select * from zdb.highlight_proximity('idxtest_highlighting_{}', 'test_field','{}' ,  ARRAY['{}'::proximitypart, '{}'::proximitypart, '{}'::proximitypart]) order by position;", title, search_string, array_one, array_two,array_three);
        Spi::connect(|client| {
            let table = client.select(&select, None, None);

            // field_name | term |    type    | position | start_offset | end_offset
            // -----------+------+------------+----------+--------------+-----------

            let expect = vec![
                ("<ALPHANUM>", "energy", 223, 1597, 1603),
                ("<ALPHANUM>", "lay", 226, 1631, 1634),
                ("<ALPHANUM>", "enron", 227, 1648, 1653),
            ];

            test_table(table, expect);

            Ok(Some(()))
        });
    }

    #[pg_test]
    #[initialize(es = true)]
    fn test_highlighter_proximity_array_two_then_one_in_order() {
        let title = "highlight_proximity_array_two_then_one_in_order";
        start_table_and_index(title);
        let search_string = "This is a test";
        let array_one = "{\"words\": [\"this\", \"is\" ], \"distance\": 2, \"in_order\": true}";
        let array_two = "{\"words\": [\"test\"], \"distance\": 5, \"in_order\": true}";
        let select = format!("select * from zdb.highlight_proximity('idxtest_highlighting_{}', 'test_field','{}' ,  ARRAY['{}'::proximitypart, '{}'::proximitypart]) order by position;", title,search_string, array_one, array_two);
        Spi::connect(|client| {
            let table = client.select(&select, None, None);

            // field_name | term |    type    | position | start_offset | end_offset
            // -----------+------+------------+----------+--------------+------------
            // test_field | this | <ALPHANUM> |        0 |            0 |          4
            // test_field | is   | <ALPHANUM> |        1 |            5 |          7
            // test_field | test | <ALPHANUM> |        3 |           10 |         14
            let expect = vec![
                ("<ALPHANUM>", "this", 0, 0, 4),
                ("<ALPHANUM>", "is", 1, 5, 7),
                ("<ALPHANUM>", "test", 3, 10, 14),
            ];

            test_table(table, expect);

            Ok(Some(()))
        });
    }

    #[pg_test]
    #[initialize(es = true)]
    fn test_highlighter_proximity_array_one_then_two_in_order() {
        let title = "highlight_proximity_array_two_then_one_in_order";
        start_table_and_index(title);
        let search_string = "This is a test";
        let array_one = "{\"words\": [\"this\" ], \"distance\": 2, \"in_order\": true}";
        let array_two = "{\"words\": [\"test\", \"is\"], \"distance\": 5, \"in_order\": true}";
        let select = format!("select * from zdb.highlight_proximity('idxtest_highlighting_{}', 'test_field','{}' ,  ARRAY['{}'::proximitypart, '{}'::proximitypart]) order by position;", title,search_string, array_one, array_two);
        Spi::connect(|client| {
            let table = client.select(&select, None, None);

            // field_name | term |    type    | position | start_offset | end_offset
            // -----------+------+------------+----------+--------------+------------
            // test_field | this | <ALPHANUM> |        0 |            0 |          4
            // test_field | is   | <ALPHANUM> |        1 |            5 |          7
            // test_field | test | <ALPHANUM> |        3 |           10 |         14
            let expect = vec![
                ("<ALPHANUM>", "this", 0, 0, 4),
                ("<ALPHANUM>", "is", 1, 5, 7),
                ("<ALPHANUM>", "test", 3, 10, 14),
            ];

            test_table(table, expect);

            Ok(Some(()))
        });
    }

    #[pg_test]
    #[initialize(es = true)]
    fn test_highlighter_proximity_array_two_then_one_without_order() {
        let title = "highlight_proximity_array_two_then_one_without_order";
        start_table_and_index(title);
        let search_string = "This is a test";
        let array_one = "{\"words\": [\"test\", \"is\" ], \"distance\": 2, \"in_order\": false}";
        let array_two = "{\"words\": [\"this\"], \"distance\": 5, \"in_order\": true}";
        let select = format!("select * from zdb.highlight_proximity('idxtest_highlighting_{}', 'test_field','{}' ,  ARRAY['{}'::proximitypart, '{}'::proximitypart]) order by position;", title,search_string, array_one, array_two);
        Spi::connect(|client| {
            let table = client.select(&select, None, None);

            // field_name | term |    type    | position | start_offset | end_offset
            // -----------+------+------------+----------+--------------+------------
            // test_field | this | <ALPHANUM> |        0 |            0 |          4
            // test_field | is   | <ALPHANUM> |        1 |            5 |          7
            // test_field | test | <ALPHANUM> |        3 |           10 |         14
            let expect = vec![
                ("<ALPHANUM>", "this", 0, 0, 4),
                ("<ALPHANUM>", "is", 1, 5, 7),
                ("<ALPHANUM>", "test", 3, 10, 14),
            ];

            test_table(table, expect);

            Ok(Some(()))
        });
    }

    #[pg_test]
    #[initialize(es = true)]
    fn test_highlighter_proximity_array_one_then_two_without_order() {
        let title = "highlight_proximity_array_two_then_one_without_order";
        start_table_and_index(title);
        let search_string = "This is a test";
        let array_one = "{\"words\": [\"test\" ], \"distance\": 2, \"in_order\": false}";
        let array_two = "{\"words\": [\"this\", \"is\"], \"distance\": 5, \"in_order\": true}";
        let select = format!("select * from zdb.highlight_proximity('idxtest_highlighting_{}', 'test_field','{}' ,  ARRAY['{}'::proximitypart, '{}'::proximitypart]) order by position;", title,search_string, array_one, array_two);
        Spi::connect(|client| {
            let table = client.select(&select, None, None);

            // field_name | term |    type    | position | start_offset | end_offset
            // -----------+------+------------+----------+--------------+------------
            // test_field | this | <ALPHANUM> |        0 |            0 |          4
            // test_field | is   | <ALPHANUM> |        1 |            5 |          7
            // test_field | test | <ALPHANUM> |        3 |           10 |         14
            let expect = vec![
                ("<ALPHANUM>", "this", 0, 0, 4),
                ("<ALPHANUM>", "is", 1, 5, 7),
                ("<ALPHANUM>", "test", 3, 10, 14),
            ];

            test_table(table, expect);

            Ok(Some(()))
        });
    }

    #[pg_test]
    #[initialize(es = true)]
    fn test_highlighter_proximity_array_two_then_two_in_order() {
        let title = "highlight_proximity_array_two_then_two_in_order";
        start_table_and_index(title);
        let search_string = "This is a test that is a bit longer";
        let array_one = "{\"words\": [\"this\", \"is\" ], \"distance\": 2, \"in_order\": true}";
        let array_two = "{\"words\": [\"test\", \"longer\"], \"distance\": 5, \"in_order\": true}";
        let select = format!("select * from zdb.highlight_proximity('idxtest_highlighting_{}', 'test_field','{}' ,  ARRAY['{}'::proximitypart, '{}'::proximitypart]) order by position;", title,search_string, array_one, array_two);
        Spi::connect(|client| {
            let table = client.select(&select, None, None);

            // field_name | term |    type    | position | start_offset | end_offset
            // -----------+------+------------+----------+--------------+------------
            // test_field | this   | <ALPHANUM> |        0 |            0 |          4
            // test_field | is     | <ALPHANUM> |        1 |            5 |          7
            // test_field | test   | <ALPHANUM> |        3 |           10 |         14
            // test_field | is     | <ALPHANUM> |        5 |           20 |         22
            // test_field | longer | <ALPHANUM> |        8 |           29 |         35
            let expect = vec![
                ("<ALPHANUM>", "this", 0, 0, 4),
                ("<ALPHANUM>", "is", 1, 5, 7),
                ("<ALPHANUM>", "test", 3, 10, 14),
                ("<ALPHANUM>", "is", 5, 20, 22),
                ("<ALPHANUM>", "longer", 8, 29, 35),
            ];

            test_table(table, expect);

            Ok(Some(()))
        });
    }

    #[pg_test]
    #[initialize(es = true)]
    fn test_highlighter_proximity_array_two_then_two_without_order() {
        let title = "highlight_proximity_array_two_then_two_without_order";
        start_table_and_index(title);
        let search_string = "This is a test that is a bit longer";
        let array_one =
            "{\"words\": [\"that\", \"longer\" ], \"distance\": 2, \"in_order\": false}";
        let array_two = "{\"words\": [\"test\", \"is\"], \"distance\": 5, \"in_order\": true}";
        let select = format!("select * from zdb.highlight_proximity('idxtest_highlighting_{}', 'test_field','{}' ,  ARRAY['{}'::proximitypart, '{}'::proximitypart]) order by position;", title,search_string, array_one, array_two);
        Spi::connect(|client| {
            let table = client.select(&select, None, None);

            // field_name | term |    type    | position | start_offset | end_offset
            // -----------+------+------------+----------+--------------+------------
            //  test_field | is     | <ALPHANUM> |        1 |            5 |          7
            //  test_field | test   | <ALPHANUM> |        3 |           10 |         14
            //  test_field | that   | <ALPHANUM> |        4 |           15 |         19
            //  test_field | is     | <ALPHANUM> |        5 |           20 |         22
            //  test_field | longer | <ALPHANUM> |        8 |           29 |         35
            let expect = vec![
                ("<ALPHANUM>", "is", 1, 5, 7),
                ("<ALPHANUM>", "test", 3, 10, 14),
                ("<ALPHANUM>", "that", 4, 15, 19),
                ("<ALPHANUM>", "is", 5, 20, 22),
                ("<ALPHANUM>", "longer", 8, 29, 35),
            ];

            test_table(table, expect);

            Ok(Some(()))
        });
    }

    #[pg_test]
    #[initialize(es = true)]
    fn test_highlighter_proximity_array_three_then_three_in_order() {
        let title = "highlight_proximity_array_three_then_three_in_order";
        start_table_and_index(title);
        let search_string =
            "This is a test that is a bit longer. I have also added another sentence to test.";
        let array_one =
            "{\"words\": [\"this\", \"longer\", \"sentence\" ], \"distance\": 2, \"in_order\": true}";
        let array_two =
            "{\"words\": [\"test\", \"is\", \"to\"], \"distance\": 5, \"in_order\": true}";
        let select = format!("select * from zdb.highlight_proximity('idxtest_highlighting_{}', 'test_field','{}' ,  ARRAY['{}'::proximitypart, '{}'::proximitypart]) order by position;", title,search_string, array_one, array_two);
        Spi::connect(|client| {
            let table = client.select(&select, None, None);

            // field_name | term |    type    | position | start_offset | end_offset
            // -----------+------+------------+----------+--------------+------------
            // test_field | this     | <ALPHANUM> |        0 |            0 |          4
            // test_field | is       | <ALPHANUM> |        1 |            5 |          7
            // test_field | test     | <ALPHANUM> |        3 |           10 |         14
            // test_field | sentence | <ALPHANUM> |       13 |           58 |         66
            // test_field | to       | <ALPHANUM> |       14 |           67 |         69
            // test_field | test     | <ALPHANUM> |       15 |           70 |         74
            let expect = vec![
                ("<ALPHANUM>", "this", 0, 0, 4),
                ("<ALPHANUM>", "is", 1, 5, 7),
                ("<ALPHANUM>", "test", 3, 10, 14),
                ("<ALPHANUM>", "sentence", 14, 63, 71),
                ("<ALPHANUM>", "to", 15, 72, 74),
                ("<ALPHANUM>", "test", 16, 75, 79),
            ];

            test_table(table, expect);

            Ok(Some(()))
        });
    }

    #[pg_test]
    #[initialize(es = true)]
    fn test_highlighter_proximity_array_three_then_three_without_order() {
        let title = "highlight_proximity_array_three_then_three_without_order";
        start_table_and_index(title);
        let search_string =
            "This is a test that is a bit longer. I have also added another sentence to test.";
        let array_one =
            "{\"words\": [\"this\", \"longer\", \"sentence\" ], \"distance\": 2, \"in_order\": false}";
        let array_two =
            "{\"words\": [\"test\", \"is\", \"to\"], \"distance\": 5, \"in_order\": true}";
        let select = format!("select * from zdb.highlight_proximity('idxtest_highlighting_{}', 'test_field','{}' ,  ARRAY['{}'::proximitypart, '{}'::proximitypart]) order by position;", title,search_string, array_one, array_two);
        Spi::connect(|client| {
            let table = client.select(&select, None, None);

            // field_name | term |    type    | position | start_offset | end_offset
            // -----------+------+------------+----------+--------------+------------
            // test_field | this     | <ALPHANUM> |        0 |            0 |          4
            // test_field | is       | <ALPHANUM> |        1 |            5 |          7
            // test_field | test     | <ALPHANUM> |        3 |           10 |         14
            // test_field | is       | <ALPHANUM> |        5 |           20 |         22
            // test_field | longer   | <ALPHANUM> |        8 |           29 |         35
            // test_field | sentence | <ALPHANUM> |       14 |           63 |         71
            // test_field | to       | <ALPHANUM> |       15 |           72 |         74
            // test_field | test     | <ALPHANUM> |       16 |           75 |         79
            let expect = vec![
                ("<ALPHANUM>", "this", 0, 0, 4),
                ("<ALPHANUM>", "is", 1, 5, 7),
                ("<ALPHANUM>", "test", 3, 10, 14),
                ("<ALPHANUM>", "is", 5, 20, 22),
                ("<ALPHANUM>", "longer", 8, 29, 35),
                ("<ALPHANUM>", "sentence", 14, 63, 71),
                ("<ALPHANUM>", "to", 15, 72, 74),
                ("<ALPHANUM>", "test", 16, 75, 79),
            ];

            test_table(table, expect);

            Ok(Some(()))
        });
    }

    #[pg_test]
    #[initialize(es = true)]
    fn test_highlighter_proximity_array_three_then_three() {
        let title = "highlight_proximity_array_three_then_three_then_three";
        start_table_and_index(title);
        let search_string =
            "This is a test that is a bit longer. I have also added another sentence to test.";
        let array_one =
            "{\"words\": [\"this\", \"longer\", \"sentence\" ], \"distance\": 2, \"in_order\": true}";
        let array_two =
            "{\"words\": [\"is\", \"have\", \"to\"], \"distance\": 0, \"in_order\": true}";
        let array_three =
            "{\"words\": [\"a\", \"test\", \"also\"], \"distance\": 5, \"in_order\": true}";
        let select = format!("select * from zdb.highlight_proximity('idxtest_highlighting_{}', 'test_field','{}' ,  ARRAY['{}'::proximitypart, '{}'::proximitypart, '{}'::proximitypart]) order by position;", title,search_string, array_one, array_two,array_three);
        Spi::connect(|client| {
            let table = client.select(&select, None, None);

            //  field_name | term |    type    | position | start_offset | end_offset
            //  -----------+------+------------+----------+--------------+------------
            //  test_field | this     | <ALPHANUM> |        0 |            0 |          4
            //  test_field | is       | <ALPHANUM> |        1 |            5 |          7
            //  test_field | a        | <ALPHANUM> |        2 |            8 |          9
            //  test_field | longer   | <ALPHANUM> |        8 |           29 |         35
            //  test_field | have     | <ALPHANUM> |       10 |           39 |         43
            //  test_field | also     | <ALPHANUM> |       11 |           44 |         48
            //  test_field | sentence | <ALPHANUM> |       14 |           63 |         71
            //  test_field | to       | <ALPHANUM> |       15 |           72 |         74
            //  test_field | test     | <ALPHANUM> |       16 |           75 |         79
            let expect = vec![
                ("<ALPHANUM>", "this", 0, 0, 4),
                ("<ALPHANUM>", "is", 1, 5, 7),
                ("<ALPHANUM>", "a", 2, 8, 9),
                ("<ALPHANUM>", "longer", 8, 29, 35),
                ("<ALPHANUM>", "have", 10, 39, 43),
                ("<ALPHANUM>", "also", 11, 44, 48),
                ("<ALPHANUM>", "sentence", 14, 63, 71),
                ("<ALPHANUM>", "to", 15, 72, 74),
                ("<ALPHANUM>", "test", 16, 75, 79),
            ];

            test_table(table, expect);

            Ok(Some(()))
        });
    }

    #[pg_test]
    #[initialize(es = true)]
    fn test_highlighter_proximity_array_three_then_three_no_order() {
        let title = "highlight_proximity_array_three_then_three_then_three_no_order";
        start_table_and_index(title);
        let search_string =
            "This is a test that is a bit longer. I have also added another sentence to test.";
        let array_one =
            "{\"words\": [\"this\", \"longer\", \"sentence\" ], \"distance\": 0, \"in_order\": false}";
        let array_two =
            "{\"words\": [\"is\", \"have\", \"another\"], \"distance\": 0, \"in_order\": false}";
        let array_three =
            "{\"words\": [\"a\", \"test\", \"added\"], \"distance\": 5, \"in_order\": true}";
        let select = format!("select * from zdb.highlight_proximity('idxtest_highlighting_{}', 'test_field','{}' ,  ARRAY['{}'::proximitypart, '{}'::proximitypart, '{}'::proximitypart]) order by position;", title,search_string, array_one, array_two,array_three);
        Spi::connect(|client| {
            let table = client.select(&select, None, None);

            //  field_name | term |    type    | position | start_offset | end_offset
            //  -----------+------+------------+----------+--------------+------------
            //  test_field | this     | <ALPHANUM> |        0 |            0 |          4
            //  test_field | is       | <ALPHANUM> |        1 |            5 |          7
            //  test_field | a        | <ALPHANUM> |        2 |            8 |          9
            //  test_field | added    | <ALPHANUM> |       12 |           49 |         54
            //  test_field | another  | <ALPHANUM> |       13 |           55 |         62
            //  test_field | sentence | <ALPHANUM> |       14 |           63 |         71
            let expect = vec![
                ("<ALPHANUM>", "this", 0, 0, 4),
                ("<ALPHANUM>", "is", 1, 5, 7),
                ("<ALPHANUM>", "a", 2, 8, 9),
                ("<ALPHANUM>", "added", 12, 49, 54),
                ("<ALPHANUM>", "another", 13, 55, 62),
                ("<ALPHANUM>", "sentence", 14, 63, 71),
            ];

            test_table(table, expect);

            Ok(Some(()))
        });
    }

    #[pg_test]
    #[initialize(es = true)]
    fn test_highlighter_proximity_array_two_then_three_no_order() {
        let title = "highlight_proximity_array_three_then_three_then_three_no_order";
        start_table_and_index(title);
        let search_string =
            "Okay sure, I think this sentence is about fifteen words long Maybe less I dunno";
        let array_one = "{\"words\": [\"okay\", \"sure\" ], \"distance\": 15, \"in_order\": false}";
        let array_two =
            "{\"words\": [\"is\", \"fifteen\", \"words\"], \"distance\": 15, \"in_order\": false}";
        let array_three = "{\"words\": [\"dunno\"], \"distance\": 5, \"in_order\": true}";
        let select = format!("select * from zdb.highlight_proximity('idxtest_highlighting_{}', 'test_field','{}' ,  ARRAY['{}'::proximitypart, '{}'::proximitypart, '{}'::proximitypart]) order by position;", title,search_string, array_one, array_two,array_three);
        Spi::connect(|client| {
            let table = client.select(&select, None, None);

            // field_name | term |    type    | position | start_offset | end_offset
            // -----------+------+------------+----------+--------------+------------
            // test_field | okay    | <ALPHANUM> |        0 |            0 |          4
            // test_field | sure    | <ALPHANUM> |        1 |            5 |          9
            // test_field | is      | <ALPHANUM> |        6 |           33 |         35
            // test_field | fifteen | <ALPHANUM> |        8 |           42 |         49
            // test_field | words   | <ALPHANUM> |        9 |           50 |         55
            // test_field | dunno   | <ALPHANUM> |       14 |           74 |         79
            let expect = vec![
                ("<ALPHANUM>", "okay", 0, 0, 4),
                ("<ALPHANUM>", "sure", 1, 5, 9),
                ("<ALPHANUM>", "is", 6, 33, 35),
                ("<ALPHANUM>", "fifteen", 8, 42, 49),
                ("<ALPHANUM>", "words", 9, 50, 55),
                ("<ALPHANUM>", "dunno", 14, 74, 79),
            ];

            test_table(table, expect);

            Ok(Some(()))
        });
    }

    fn start_table_and_index(title: &str) {
        let create_table = &format!(
            "CREATE TABLE test_highlighting_{} AS SELECT * FROM generate_series(1, 10);",
            title,
        );
        Spi::run(create_table);
        let create_index = &format!("CREATE INDEX idxtest_highlighting_{} ON test_highlighting_{} USING zombodb ((test_highlighting_{}.*))",title,title,title,);
        Spi::run(create_index);
    }

    fn test_table(mut table: SpiTupleTable, expect: Vec<(&str, &str, i32, i64, i64)>) {
        let mut i = 0;
        while let Some(_) = table.next() {
            let token = table.get_datum::<&str>(2).unwrap();
            let ttype = table.get_datum::<&str>(3).unwrap();
            let pos = table.get_datum::<i32>(4).unwrap();
            let start_offset = table.get_datum::<i64>(5).unwrap();
            let end_offset = table.get_datum::<i64>(6).unwrap();

            let row_tuple = (ttype, token, pos, start_offset, end_offset);

            assert_eq!(expect[i], row_tuple);

            i += 1;
        }
        assert_eq!(expect.len(), i);
    }

    fn test_blurb() -> String {
        let blurb = "Enron

        P.O.Box 1188
        Houston, TX 77251-1188

        Mark Palmer
        713-853-4738

        ENRON REPORTS RECURRING THIRD QUARTER EARNINGS OF $0.43 PER
        DILUTED SHARE; REPORTS NON-RECURRING CHARGES OF $1.01 BILLION
        AFTER-TAX; REAFFIRMS RECURRING EARNINGS ESTIMATES OF $1.80 FOR
        2001 AND $2.15 FOR 2002; AND EXPANDS FINANCIAL REPORTING

        FOR IMMEDIATE RELEASE:  Tuesday, Oct. 16, 2001

        HOUSTON - Enron Corp. (NYSE - ENE) announced today recurring earnings per
        diluted share of $0.43 for the third quarter of 2001, compared to $0.34 a year ago.  Total
        recurring net income increased to $393 million, versus $292 million a year ago.
            Our 26 percent increase in recurring earnings per diluted share shows the very strong
        results of our core wholesale and retail energy businesses and our natural gas pipelines,   said
        Kenneth L. Lay, Enron chairman and CEO.   The continued excellent prospects in these
        businesses and Enron ''s leading market position make us very confident in our strong earnings
        outlook.
            Non-recurring charges totaling $1.01 billion after-tax, or $(1.11) loss per diluted share,
        were recognized for the third quarter of 2001.  The total net loss for the quarter, including non-
            recurring items, was $(618) million, or $(0.84) per diluted share.
            After a thorough review of our businesses, we have decided to take these charges to
        clear away issues that have clouded the performance and earnings potential of our core energy
        businesses,   said Lay.
            Enron also reaffirmed today it is on track to continue strong earnings growth and achieve
        its previously stated targets of recurring earnings per diluted share of  $0.45 for the fourth
        quarter 2001, $1.80 for 2001 and $2.15 for 2002.
        PERFORMANCE SUMMARY
        Enron has recently expanded the reporting of its financial results by both providing
        additional segments and expanding financial and operating information in the attached tables.
            Enron ''s business segments are as follows:
        ?  Wholesale Services
        o Americas
        o Europe and Other Commodity Markets
            ?  Retail Services
            ?  Transportation and Distribution
        o Natural Gas Pipelines
        o Portland General
        o Global Assets
            ?  Broadband Services
            ?  Corporate and Other

        Wholesale Services:  Total income before interest, minority interests and taxes (IBIT)
        increased 28 percent to $754 million in the third quarter of 2001, compared to $589 million in
        the third quarter of last year.  Total wholesale physical volumes increased 65 percent to 88.2
        trillion British thermal units equivalent per day (Tbtue/d) in the recent quarter.
            Americas  - This segment consists of Enron ''s gas and power market-making operations
        and merchant energy activities in North and South America.  IBIT from this segment grew 31
        percent to $701 million in the recent quarter from $536 million a year ago, driven by strong
        results from the North America natural gas and power businesses.  Natural gas volumes
        increased 6 percent to 26.7 Tbtu/d, and power volumes increased 77 percent to 290 million
        megawatt-hours (MWh).
            Europe and Other Commodity Markets - This segment includes Enron ''s European gas
        and power operations and Enron ''s other commodity businesses, such as metals, coal, crude and
        liquids, weather, forest products and steel.  For the third quarter of 2001, IBIT for the segment
        remained unchanged at $53 million as compared to last year.  Although physical volumes
        increased for each commodity in the segment, the low level of volatility in the gas and power
        markets caused profitability to remain flat.

            Retail Services:  Enron ''s Retail Services product offerings include pricing and delivery
        of natural gas and power, as well as demand side management services to minimize energy costs
        for business consumers in North America and Europe.  In the third quarter of 2001, Retail
        Services generated IBIT of $71 million, compared to $27 million a year ago.  Retail Services
        continues to successfully penetrate markets with standard, scalable products to reduce
        consumers '' total energy costs.  Enron recently added new business with large consumers,
        including Wal-Mart, Northrop Grumman, the City of Chicago, Equity Office Properties and
        Wendy ''s in the U.S. and Sainsbury and Guinness Brewery in the U.K.  To date in 2001, Enron
        has completed over 50 transactions with large consumers.  Enron is also successfully extending
        its retail energy products to small business customers, completing over 95,000 transactions in the
        first nine months of this year.
            Transportation and Distribution:  The Transportation and Distribution group includes
        Natural Gas Pipelines, Portland General and Global Assets.
            Natural Gas Pipelines - This segment provided $85 million of IBIT in the current
        quarter, up slightly from the same quarter last year.  Pipeline expansions are underway in high
        growth areas and include a 428 million cubic feet per day (MMcf/d) expansion by Florida Gas
        Transmission and a 150 MMcf/d expansion by Transwestern.
            Portland General - Portland General Electric, an electric utility in the northwestern U.S.,
        reported an IBIT loss of $(17) million compared to IBIT of $74 million in the same quarter a
        year ago.  Portland General entered into power contracts in prior periods to ensure adequate
        supply for the recent quarter at prices that were significantly higher than actual settled prices
        during the third quarter of 2001.  Although the rate mechanism in place anticipated and
        substantially mitigated the effect of the higher purchased power costs, only the amount in excess
        of a defined baseline was recoverable from ratepayers.  Increased power cost recovery was
        incorporated into Portland General ''s new fifteen-month rate structure, which became effective
        October 1, 2001 and included an average 40 percent rate increase.
            Last week, Enron announced a definitive agreement to sell Portland General to Northwest
        Natural Gas for approximately $1.9 billion and the assumption of approximately $1.1 billion in
        Portland General debt.  The proposed transaction, which is subject to customary regulatory
        approvals, is expected to close by late 2002.

        Global Assets - The Global Assets segment includes assets not part of Enron ''s wholesale
        or retail energy operations.  Major assets included in this segment are Elektro, an electric utility
        in Brazil; Dabhol, a power plant in India; TGS, a natural gas pipeline in Argentina; Azurix; and
        the Enron Wind operations.  For the third quarter of 2001, IBIT for the segment remained
        unchanged at $19 million as compared to last year.
            Broadband Services:  Enron makes markets for bandwidth, IP and storage products and
        bundles such products for comprehensive network management services.  IBIT losses were $(80)
        million in the current quarter compared to a $(20) million loss in the third quarter of last year.
            This quarter ''s results include significantly lower investment-related income and lower operating
        costs.
            Corporate and Other:  Corporate and Other reported an IBIT loss of $(59) million for
        the quarter compared to $(106) million loss a year ago.  Corporate and Other represents the
        unallocated portion of expenses related to general corporate functions.

            NON-RECURRING ITEMS
        Enron ''s results in the third quarter of 2001 include after-tax non-recurring charges of
        $1.01 billion, or $(1.11) per diluted share, consisting of:
        ?  $287 million related to asset impairments recorded by Azurix Corp.  These
        impairments primarily reflect Azurix ''s planned disposition of its North American
        and certain South American service-related businesses;
        ?  $180 million associated with the restructuring of Broadband Services, including
        severance costs, loss on the sale of inventory and an impairment to reflect the
        reduced value of Enron ''s content services business; and
            ?  $544 million related to losses associated with certain investments, principally
        Enron ''s interest in The New Power Company, broadband and technology
        investments, and early termination during the third quarter of certain structured
        finance arrangements with a previously disclosed entity.


            OTHER INFORMATION
        A conference call with Enron management regarding third quarter results will be
        conducted live today at 10:00 a.m. EDT and may be accessed through the Investor Relations
        page at www.enron.com
            .
                Enron is one of the world ''s leading energy, commodities and service companies.  The
        company makes markets in electricity and natural gas, delivers energy and other physical
        commodities, and provides financial and risk management services to customers around the
        world.  The stock is traded under the ticker symbol   ENE.
            ______________________________________________________________________________
        Please see attached tables for additional financial information.

            This press release includes forward-looking statements within the meaning of Section 27A of the Securities Act of 1933 and Section
        21E of the Securities Exchange Act of 1934.  The Private Securities Litigation Reform Act of 1995 provides a safe harbor for forward-looking
        statements made by Enron or on its behalf.  These forward-looking statements are not historical facts, but reflect Enron ''s curr ent expectations,
        estimates and projections.  All statements contained in the press release which address future operating performance, events or developments that
        are expected to occur in the future (including statements relating to earnings expectations, sales of assets, or statements expressing general
        optimism about future operating results) are forward-looking statements.  Although Enron believes that its expectations are bas ed on reasonable
        assumptions, it can give no assurance that its goals will be achieved.  Important factors that could cause actual results to di ffer materially from
        those in the forward-looking statements herein include success in marketing natural gas and power to wholesale customers; the ability to
        penetrate new retail natural gas and electricity markets, including the energy outsource market, in the United States and Europe; the timing, extent
        and market effects of deregulation of energy markets in the United States and in foreign jurisdictions; development of Enron ''s broadband
        network and customer demand for intermediation and content services; political developments in foreign countries; receipt of re gulatory
        approvals and satisfaction of customary closing conditions to the sale of Portland General; and conditions of the capital markets and equity
        markets during the periods covered by the forward-looking statements.";
        return String::from(blurb);
    }
}
