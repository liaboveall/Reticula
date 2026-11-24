//! Reads the string literals off Rust sources, as far as it takes to list the
//! messages among them. Not a parser: it knows tokens, brackets and attributes.

/// A string literal outside of tests.
#[derive(Debug, PartialEq, Eq)]
pub struct Literal {
    /// What it says. In a format string, each placeholder has the name of what
    /// is put there: `{count}`.
    pub text: String,
    /// The calls and macros it is in, the innermost last.
    pub calls: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
enum Token {
    /// An identifier, a keyword or a number.
    Word(String),
    /// A string literal.
    Text(String),
    Sign(char),
}

/// Every string literal of a source file in order, without those in tests and in
/// attributes. The message of an `#[error]` attribute is a literal like any other.
pub fn literals(source: &str) -> Vec<Literal> {
    let tokens = tokenize(source);
    let mut found = Vec::new();
    let mut calls: Vec<String> = Vec::new();
    let mut path = String::new();
    let mut at = 0;
    while let Some(token) = tokens.get(at) {
        match token {
            Token::Sign('#') => {
                if let Some((inside, after)) = attribute(&tokens, at) {
                    at = after;
                    if is_test(inside) {
                        at = skip_item(&tokens, at);
                    } else if inside.first().is_some_and(|token| is_word(token, "error")) {
                        let mut calls = calls.clone();
                        calls.push(String::from("#[error]"));
                        for (index, token) in inside.iter().enumerate() {
                            if let Token::Text(text) = token {
                                let text = template(text, arguments(inside, index));
                                found.push(Literal {
                                    text,
                                    calls: calls.clone(),
                                });
                            }
                        }
                    }
                    path.clear();
                    continue;
                }
            }
            Token::Word(word) => {
                if at > 1 && is_path_separator(&tokens, at - 2) && !path.is_empty() {
                    path.push_str("::");
                    path.push_str(word);
                } else {
                    path.clone_from(word);
                }
            }
            Token::Text(text) => {
                let is_format = calls.last().is_some_and(|call| is_format_macro(call));
                let text = if is_format {
                    template(text, arguments(&tokens, at))
                } else {
                    text.clone()
                };
                found.push(Literal {
                    text,
                    calls: calls.clone(),
                });
                path.clear();
            }
            Token::Sign('(' | '[' | '{') => calls.push(std::mem::take(&mut path)),
            Token::Sign(')' | ']' | '}') => {
                calls.pop();
                path.clear();
            }
            Token::Sign('!') if !path.is_empty() => path.push('!'),
            Token::Sign('<') if at > 1 && is_path_separator(&tokens, at - 2) => {
                // The types in `f::<T>()` are not where its name ends.
                let mut depth = 0_usize;
                while let Some(token) = tokens.get(at) {
                    match token {
                        Token::Sign('<') => depth += 1,
                        Token::Sign('>') => depth -= 1,
                        _ => {}
                    }
                    if depth == 0 {
                        break;
                    }
                    at += 1;
                }
            }
            Token::Sign(':') if is_path_separator(&tokens, at) => at += 1,
            Token::Sign(_) => path.clear(),
        }
        at += 1;
    }
    found
}

fn is_word(token: &Token, word: &str) -> bool {
    matches!(token, Token::Word(found) if found == word)
}

fn is_path_separator(tokens: &[Token], at: usize) -> bool {
    tokens.get(at) == Some(&Token::Sign(':')) && tokens.get(at + 1) == Some(&Token::Sign(':'))
}

fn is_format_macro(call: &str) -> bool {
    matches!(call, "format!" | "write!" | "writeln!")
}

/// What an attribute has between its brackets, and where it ends.
fn attribute(tokens: &[Token], at: usize) -> Option<(&[Token], usize)> {
    let mut open = at + 1;
    if tokens.get(open) == Some(&Token::Sign('!')) {
        open += 1;
    }
    if tokens.get(open) != Some(&Token::Sign('[')) {
        return None;
    }
    let end = closing(tokens, open);
    Some((&tokens[open + 1..end.min(tokens.len())], end + 1))
}

/// Where the bracket at `open` is closed.
fn closing(tokens: &[Token], open: usize) -> usize {
    let mut depth = 0_usize;
    for (at, token) in tokens.iter().enumerate().skip(open) {
        match token {
            Token::Sign('(' | '[' | '{') => depth += 1,
            Token::Sign(')' | ']' | '}') => depth -= 1,
            _ => {}
        }
        if depth == 0 {
            return at;
        }
    }
    tokens.len()
}

/// Whether an attribute makes what follows it a part of the tests only.
fn is_test(attribute: &[Token]) -> bool {
    let words: Vec<&str> = attribute
        .iter()
        .filter_map(|token| match token {
            Token::Word(word) => Some(word.as_str()),
            _ => None,
        })
        .collect();
    matches!(
        words.as_slice(),
        ["cfg", "test"] | ["cfg", "all", "test", ..] | ["test"]
    )
}

/// Where the item ends that begins at `at`.
fn skip_item(tokens: &[Token], mut at: usize) -> usize {
    while let Some(token) = tokens.get(at) {
        match token {
            Token::Sign('#') => {
                if let Some((_, after)) = attribute(tokens, at) {
                    at = after;
                    continue;
                }
            }
            Token::Sign('{') => return closing(tokens, at) + 1,
            Token::Sign('(' | '[') => at = closing(tokens, at),
            Token::Sign(';' | ',') => return at + 1,
            Token::Sign('}') => return at,
            _ => {}
        }
        at += 1;
    }
    at
}

/// The arguments after the format string at `at`, as far as they are positional.
fn arguments(tokens: &[Token], at: usize) -> Vec<String> {
    let mut found = Vec::new();
    let mut argument: Vec<&Token> = Vec::new();
    let mut depth = 0_usize;
    for token in tokens.iter().skip(at + 1) {
        let is_end = depth == 0 && matches!(token, Token::Sign(')' | ']' | '}'));
        if is_end || (depth == 0 && *token == Token::Sign(',')) {
            found.extend(name_of(&argument));
            argument.clear();
            if is_end {
                break;
            }
            continue;
        }
        match token {
            Token::Sign('(' | '[' | '{') => depth += 1,
            Token::Sign(')' | ']' | '}') => depth -= 1,
            _ => {}
        }
        argument.push(token);
    }
    found
}

/// The name that an argument goes by: the last one in it that is not of a method.
fn name_of(argument: &[&Token]) -> Option<String> {
    let first = argument.first()?;
    if let (Token::Word(name), Some(Token::Sign('='))) = (first, argument.get(1)) {
        return Some(name.clone());
    }
    let mut depth = 0_usize;
    let mut name = None;
    let mut inner = None;
    for (at, token) in argument.iter().enumerate() {
        match token {
            Token::Sign('(' | '[' | '{') => depth += 1,
            Token::Sign(')' | ']' | '}') => depth -= 1,
            Token::Word(word) if word.starts_with(|c: char| c.is_ascii_lowercase()) => {
                let is_call_or_path = matches!(argument.get(at + 1), Some(Token::Sign('(' | ':')));
                if !is_call_or_path && word != "len" && word != "self" {
                    let place = if depth == 0 { &mut name } else { &mut inner };
                    *place = Some(word.clone());
                }
            }
            _ => {}
        }
    }
    name.or(inner).or_else(|| Some(String::from("value")))
}

/// A format string with names in its placeholders, and without its formats.
fn template(format: &str, positional: Vec<String>) -> String {
    let mut names = positional.into_iter();
    let mut used: Vec<String> = Vec::new();
    let mut out = String::new();
    let mut chars = format.chars().peekable();
    while let Some(c) = chars.next() {
        if (c == '{' || c == '}') && chars.peek() == Some(&c) {
            chars.next();
            out.push(c);
        } else if c == '{' {
            let inside: String = chars.by_ref().take_while(|&c| c != '}').collect();
            let given = inside.split(':').next().unwrap_or_default();
            let mut name = if given.is_empty() {
                names.next().unwrap_or_else(|| String::from("value"))
            } else {
                given.to_owned()
            };
            if given.is_empty() && used.contains(&name) {
                name.push('2');
            }
            used.push(name.clone());
            out.push('{');
            out.push_str(&name);
            out.push('}');
        } else {
            out.push(c);
        }
    }
    out
}

fn tokenize(source: &str) -> Vec<Token> {
    let chars: Vec<char> = source.chars().collect();
    let mut tokens = Vec::new();
    let mut at = 0;
    while let Some(&c) = chars.get(at) {
        let next = chars.get(at + 1).copied();
        match c {
            '/' if next == Some('/') => {
                while chars.get(at).is_some_and(|&c| c != '\n') {
                    at += 1;
                }
            }
            '/' if next == Some('*') => at = skip_block_comment(&chars, at),
            '"' => {
                let (text, after) = quoted(&chars, at + 1);
                tokens.push(Token::Text(text));
                at = after;
            }
            '\'' => {
                // A character, unless it is a lifetime.
                if next == Some('\\') {
                    at += 3;
                    while chars.get(at).is_some_and(|&c| c != '\'') {
                        at += 1;
                    }
                    at += 1;
                } else if chars.get(at + 2) == Some(&'\'') {
                    at += 3;
                } else {
                    at += 1;
                }
            }
            c if c.is_alphanumeric() || c == '_' => {
                let start = at;
                while chars
                    .get(at)
                    .is_some_and(|&c| c.is_alphanumeric() || c == '_')
                {
                    at += 1;
                }
                let word: String = chars[start..at].iter().collect();
                match (word.as_str(), chars.get(at)) {
                    ("r" | "br", Some('"' | '#')) if is_raw(&chars, at) => {
                        let (text, after) = raw(&chars, at);
                        tokens.push(Token::Text(text));
                        at = after;
                    }
                    ("b", Some('"')) => at = quoted(&chars, at + 1).1,
                    ("b", Some('\'')) => {}
                    _ => tokens.push(Token::Word(word)),
                }
            }
            c if c.is_whitespace() => at += 1,
            c => {
                tokens.push(Token::Sign(c));
                at += 1;
            }
        }
    }
    tokens
}

fn skip_block_comment(chars: &[char], mut at: usize) -> usize {
    let mut depth = 0_usize;
    while at < chars.len() {
        match (chars[at], chars.get(at + 1)) {
            ('/', Some('*')) => {
                depth += 1;
                at += 2;
            }
            ('*', Some('/')) => {
                depth -= 1;
                at += 2;
                if depth == 0 {
                    break;
                }
            }
            _ => at += 1,
        }
    }
    at
}

/// The string literal whose first character is at `at`, and where it ends.
fn quoted(chars: &[char], mut at: usize) -> (String, usize) {
    let mut text = String::new();
    while let Some(&c) = chars.get(at) {
        at += 1;
        match c {
            '"' => break,
            '\\' => {
                let escape = chars.get(at).copied().unwrap_or_default();
                at += 1;
                match escape {
                    'n' => text.push('\n'),
                    't' => text.push('\t'),
                    'r' => text.push('\r'),
                    '0' => text.push('\0'),
                    'x' => {
                        let code: String = chars.iter().skip(at).take(2).collect();
                        text.extend(u32::from_str_radix(&code, 16).ok().and_then(char::from_u32));
                        at += 2;
                    }
                    'u' => {
                        let code: String = chars
                            .iter()
                            .skip(at + 1)
                            .take_while(|&&c| c != '}')
                            .collect();
                        at += code.chars().count() + 2;
                        text.extend(u32::from_str_radix(&code, 16).ok().and_then(char::from_u32));
                    }
                    '\n' | '\r' => {
                        while chars.get(at).is_some_and(|c| c.is_whitespace()) {
                            at += 1;
                        }
                    }
                    other => text.push(other),
                }
            }
            other => text.push(other),
        }
    }
    (text, at)
}

/// Whether what follows an `r` is a raw string and not a raw identifier.
fn is_raw(chars: &[char], at: usize) -> bool {
    chars.iter().skip(at).find(|&&c| c != '#') == Some(&'"')
}

/// The raw string literal whose quotes begin at `at`, and where it ends.
fn raw(chars: &[char], at: usize) -> (String, usize) {
    let hashes = chars.iter().skip(at).take_while(|&&c| c == '#').count();
    let start = at + hashes + 1;
    let mut end = start;
    while end < chars.len() {
        let is_end = chars[end] == '"'
            && chars
                .iter()
                .skip(end + 1)
                .take(hashes)
                .filter(|&&c| c == '#')
                .count()
                == hashes;
        if is_end {
            break;
        }
        end += 1;
    }
    let text = chars[start..end.min(chars.len())].iter().collect();
    (text, end + 1 + hashes)
}

#[cfg(test)]
mod tests {
    use super::literals;

    fn texts(source: &str) -> Vec<String> {
        literals(source)
            .into_iter()
            .map(|literal| literal.text)
            .collect()
    }

    #[test]
    fn literals_are_read_as_they_are_compiled() {
        let source = r##"
            // "not this"
            /* "nor /* "this" */ " */
            const A: &str = "a \"b\" \u{2013} \x41\
                             c";
            const B: &str = r#"a "b" \n"#;
            const C: &[u8] = b"bytes";
            fn f<'a>(x: &'a str) -> char { if x == "lifetime" { '"' } else { '\'' } }
            fn r#type() {}
        "##;
        assert_eq!(texts(source), ["a \"b\" – Ac", r#"a "b" \n"#, "lifetime"]);
    }

    #[test]
    fn placeholders_get_the_names_of_what_they_stand_for() {
        let source = r#"
            fn f() {
                format!("{} of {total:>3} in {:?}", self.rows.len(), &def.name);
                format!("{} to {}", a.row + 1, b.row + 2);
                format!("{{{0}}} {}", ids.len, sum = 3);
                write!(f, "{err:?}: {}", f64::from(rate));
                String::from("{not a placeholder}");
                format!("{}", text("{}"));
            }
        "#;
        assert_eq!(
            texts(source),
            [
                "{rows} of {total} in {name}",
                "{row} to {row2}",
                "{{0}} {ids}",
                "{err}: {rate}",
                "{not a placeholder}",
                "{value}",
                "{}",
            ]
        );
    }

    #[test]
    fn literals_know_the_calls_they_are_in() {
        let found = literals(
            r#"fn f() {
                tracing::warn!(reason = "a", "b");
                let x: u32 = integer::<_, i64>(value, "c").expect("d");
                ApiError::Invalid("e".into());
            }"#,
        );
        let innermost: Vec<(&str, &str)> = found
            .iter()
            .map(|l| (l.text.as_str(), l.calls.last().map_or("", String::as_str)))
            .collect();
        assert_eq!(
            innermost,
            [
                ("a", "tracing::warn!"),
                ("b", "tracing::warn!"),
                ("c", "integer"),
                ("d", "expect"),
                ("e", "ApiError::Invalid"),
            ]
        );
    }

    #[test]
    fn tests_and_attributes_are_left_out_and_error_messages_are_not() {
        let source = r#"
            #![doc = "a"]
            #[serde(rename = "b")]
            enum E {
                #[error("failed: {0:?} of {name}")]
                A(u32),
            }
            #[cfg(test)]
            use x::{y, z};
            #[cfg(not(test))]
            const C: &str = "c";
            #[test]
            #[ignore = "d"]
            fn t() { f("e"); }
            #[cfg(test)]
            mod tests { const F: &str = "f"; mod inner { const G: &str = "g"; } }
            const H: &str = "h";
        "#;
        assert_eq!(texts(source), ["failed: {0} of {name}", "c", "h"]);
    }
}
