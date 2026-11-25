//! Questions and answers as JSON, which JavaScript reads and writes. Objects would
//! have to be taken apart and put together call by call, which takes far more code.

use std::fmt::Write as _;
use std::iter::Peekable;
use std::str::Chars;

use serde::ser::{Impossible, SerializeSeq, SerializeStruct, Serializer};
use serde::{Serialize, ser};
use wasm_bindgen::prelude::*;

#[wasm_bindgen(typescript_custom_section)]
const JSON: &str = r"
/** A text that `JSON.parse` makes a `T` of. */
export type Json<T> = string & { readonly of?: T };
";

/// A value in JSON. Nothing that crosses the boundary has numbers with a
/// fraction, or anything else that is not written here.
pub fn json<T: Serialize>(value: &T) -> String {
    let mut out = String::new();
    match value.serialize(Json(&mut out)) {
        Ok(()) => out,
        Err(Unwritable) => String::from("null"),
    }
}

/// Why something is not written.
#[derive(Debug)]
pub struct Unwritable;

impl std::fmt::Display for Unwritable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("unwritable")
    }
}

impl std::error::Error for Unwritable {}

impl ser::Error for Unwritable {
    fn custom<T: std::fmt::Display>(_: T) -> Self {
        Self
    }
}

struct Json<'a>(&'a mut String);

/// Items of a list, or fields of an object, that are being written.
struct Items<'a> {
    out: &'a mut String,
    first: bool,
    end: char,
}

impl<'a> Items<'a> {
    #[inline(never)]
    fn new(out: &'a mut String, start: char, end: char) -> Self {
        out.push(start);
        Self {
            out,
            first: true,
            end,
        }
    }

    /// Begins the next one. In one place, as every field of every type does.
    #[inline(never)]
    fn next(&mut self, key: Option<&str>) {
        if !std::mem::replace(&mut self.first, false) {
            self.out.push(',');
        }
        if let Some(key) = key {
            text(self.out, key);
            self.out.push(':');
        }
    }
}

impl SerializeSeq for Items<'_> {
    type Ok = ();
    type Error = Unwritable;

    #[inline(never)]
    fn serialize_element<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Unwritable> {
        self.next(None);
        value.serialize(Json(self.out))
    }

    #[inline(never)]
    fn end(self) -> Result<(), Unwritable> {
        self.out.push(self.end);
        Ok(())
    }
}

impl SerializeStruct for Items<'_> {
    type Ok = ();
    type Error = Unwritable;

    #[inline(never)]
    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<(), Unwritable> {
        self.next(Some(key));
        value.serialize(Json(self.out))
    }

    fn end(self) -> Result<(), Unwritable> {
        SerializeSeq::end(self)
    }
}

#[inline(never)]
fn number(out: &mut String, value: u64) {
    let _ = write!(out, "{value}");
}

#[inline(never)]
fn text(out: &mut String, value: &str) {
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' | '\\' => {
                out.push('\\');
                out.push(ch);
            }
            '\0'..='\x1f' => {
                let _ = write!(out, "\\u{:04x}", u32::from(ch));
            }
            _ => out.push(ch),
        }
    }
    out.push('"');
}

impl<'a> Serializer for Json<'a> {
    type Ok = ();
    type Error = Unwritable;
    type SerializeSeq = Items<'a>;
    type SerializeTuple = Impossible<(), Unwritable>;
    type SerializeTupleStruct = Impossible<(), Unwritable>;
    type SerializeTupleVariant = Impossible<(), Unwritable>;
    type SerializeMap = Impossible<(), Unwritable>;
    type SerializeStruct = Items<'a>;
    type SerializeStructVariant = Impossible<(), Unwritable>;

    fn serialize_bool(self, value: bool) -> Result<(), Unwritable> {
        self.0.push_str(if value { "true" } else { "false" });
        Ok(())
    }

    fn serialize_u8(self, value: u8) -> Result<(), Unwritable> {
        self.serialize_u64(u64::from(value))
    }

    fn serialize_u16(self, value: u16) -> Result<(), Unwritable> {
        self.serialize_u64(u64::from(value))
    }

    fn serialize_u32(self, value: u32) -> Result<(), Unwritable> {
        self.serialize_u64(u64::from(value))
    }

    fn serialize_u64(self, value: u64) -> Result<(), Unwritable> {
        number(self.0, value);
        Ok(())
    }

    fn serialize_str(self, value: &str) -> Result<(), Unwritable> {
        text(self.0, value);
        Ok(())
    }

    fn serialize_none(self) -> Result<(), Unwritable> {
        self.0.push_str("null");
        Ok(())
    }

    fn serialize_some<T: Serialize + ?Sized>(self, value: &T) -> Result<(), Unwritable> {
        value.serialize(self)
    }

    fn serialize_unit_variant(
        self,
        _: &'static str,
        _: u32,
        variant: &'static str,
    ) -> Result<(), Unwritable> {
        self.serialize_str(variant)
    }

    fn serialize_seq(self, _: Option<usize>) -> Result<Items<'a>, Unwritable> {
        Ok(Items::new(self.0, '[', ']'))
    }

    fn serialize_struct(self, _: &'static str, _: usize) -> Result<Items<'a>, Unwritable> {
        Ok(Items::new(self.0, '{', '}'))
    }

    fn serialize_newtype_struct<T: Serialize + ?Sized>(
        self,
        _: &'static str,
        value: &T,
    ) -> Result<(), Unwritable> {
        value.serialize(self)
    }

    fn serialize_i8(self, _: i8) -> Result<(), Unwritable> {
        Err(Unwritable)
    }

    fn serialize_i16(self, _: i16) -> Result<(), Unwritable> {
        Err(Unwritable)
    }

    fn serialize_i32(self, _: i32) -> Result<(), Unwritable> {
        Err(Unwritable)
    }

    fn serialize_i64(self, _: i64) -> Result<(), Unwritable> {
        Err(Unwritable)
    }

    fn serialize_f32(self, _: f32) -> Result<(), Unwritable> {
        Err(Unwritable)
    }

    fn serialize_f64(self, _: f64) -> Result<(), Unwritable> {
        Err(Unwritable)
    }

    fn serialize_char(self, _: char) -> Result<(), Unwritable> {
        Err(Unwritable)
    }

    fn serialize_bytes(self, _: &[u8]) -> Result<(), Unwritable> {
        Err(Unwritable)
    }

    fn serialize_unit(self) -> Result<(), Unwritable> {
        Err(Unwritable)
    }

    fn serialize_unit_struct(self, _: &'static str) -> Result<(), Unwritable> {
        Err(Unwritable)
    }

    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: &T,
    ) -> Result<(), Unwritable> {
        Err(Unwritable)
    }

    fn serialize_tuple(self, _: usize) -> Result<Self::SerializeTuple, Unwritable> {
        Err(Unwritable)
    }

    fn serialize_tuple_struct(
        self,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeTupleStruct, Unwritable> {
        Err(Unwritable)
    }

    fn serialize_tuple_variant(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeTupleVariant, Unwritable> {
        Err(Unwritable)
    }

    fn serialize_map(self, _: Option<usize>) -> Result<Self::SerializeMap, Unwritable> {
        Err(Unwritable)
    }

    fn serialize_struct_variant(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeStructVariant, Unwritable> {
        Err(Unwritable)
    }
}

/// How far values may be in each other.
const MOST_DEPTH: u32 = 16;

/// What a text of `JSON.stringify` holds. Numbers and truth values are the texts
/// that they are written as.
#[derive(Debug, Default, PartialEq, Eq)]
pub enum Value {
    #[default]
    Nothing,
    Text(String),
    /// A list, or the fields of an object, which have names.
    Items(Vec<(String, Self)>),
}

impl Value {
    /// What cannot be read is nothing.
    pub fn read(text: &str) -> Self {
        value(&mut text.chars().peekable(), MOST_DEPTH).unwrap_or_default()
    }

    pub fn items(&self) -> &[(String, Self)] {
        match self {
            Self::Items(items) => items,
            _ => &[],
        }
    }

    #[inline(never)]
    pub fn field(&self, name: &str) -> &Self {
        let found = self.items().iter().find(|field| field.0 == name);
        found.map_or(&Self::Nothing, |field| &field.1)
    }

    /// A text, which is empty when there is none.
    pub fn text(&self) -> &str {
        match self {
            Self::Text(text) => text,
            _ => "",
        }
    }

    /// A number from 0 on. Anything else is one that nothing has.
    pub fn number(&self) -> usize {
        self.text().parse().unwrap_or(usize::MAX)
    }

    /// The text of a field.
    pub fn of(&self, name: &str) -> &str {
        self.field(name).text()
    }
}

fn value(chars: &mut Peekable<Chars<'_>>, depth: u32) -> Option<Value> {
    let depth = depth.checked_sub(1)?;
    let end = match chars.peek()? {
        '"' => return Some(Value::Text(quoted(chars)?)),
        '[' => ']',
        '{' => '}',
        _ => {
            let mut word = String::new();
            while let Some(ch) = chars.next_if(|ch| !matches!(ch, ',' | ']' | '}')) {
                word.push(ch);
            }
            return Some(Value::Text(word));
        }
    };
    chars.next();
    let mut items = Vec::new();
    if chars.next_if_eq(&end).is_some() {
        return Some(Value::Items(items));
    }
    loop {
        let mut name = String::new();
        if end == '}' {
            name = quoted(chars)?;
            chars.next_if_eq(&':')?;
        }
        items.push((name, value(chars, depth)?));
        match chars.next()? {
            ',' => {}
            ch => return (ch == end).then_some(Value::Items(items)),
        }
    }
}

fn quoted(chars: &mut Peekable<Chars<'_>>) -> Option<String> {
    chars.next_if_eq(&'"')?;
    let mut out = String::new();
    loop {
        out.push(match chars.next()? {
            '"' => return Some(out),
            '\\' => match chars.next()? {
                'b' => '\x08',
                'f' => '\x0c',
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                'u' => {
                    let mut code = 0;
                    for _ in 0..4 {
                        code = code * 16 + chars.next()?.to_digit(16)?;
                    }
                    // Half of a character is none.
                    char::from_u32(code).unwrap_or(char::REPLACEMENT_CHARACTER)
                }
                ch => ch,
            },
            ch => ch,
        });
    }
}

#[cfg(test)]
mod tests {
    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use super::*;

    #[derive(Serialize)]
    #[serde(tag = "kind", rename_all = "camelCase")]
    enum Shape {
        Nothing,
        SomeBox { wide: u32, tags: Vec<&'static str> },
    }

    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    enum Name {
        TheOne,
    }

    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct All {
        yes: bool,
        largest_number: u64,
        none: Option<u8>,
        some: Option<u8>,
        name: Name,
        shapes: Vec<Shape>,
        #[serde(rename = "type")]
        kind: String,
    }

    fn list(items: Vec<Value>) -> Value {
        let unnamed = items.into_iter().map(|item| (String::new(), item));
        Value::Items(unnamed.collect())
    }

    fn word(text: &str) -> Value {
        Value::Text(String::from(text))
    }

    #[test]
    fn what_javascript_writes_is_read() {
        let read = Value::read(concat!(
            r#"{"a":[1,-2.5e+30,true],"b":{"c":"","d e":[]},"#,
            r#""名":"\"\\/\n\t\r\b\f\u001f\u0000|\ud800|名"}"#
        ));
        assert_eq!(
            *read.field("a"),
            list(["1", "-2.5e+30", "true"].map(word).into())
        );
        assert_eq!(read.field("b").items().len(), 2);
        assert_eq!(read.field("b").of("c"), "");
        assert_eq!(*read.field("b").field("d e"), list(Vec::new()));
        let half = char::REPLACEMENT_CHARACTER;
        assert_eq!(
            read.of("名"),
            format!("\"\\/\n\t\r\x08\x0c\x1f\0|{half}|名")
        );
        assert_eq!(*read.field("c"), Value::Nothing);
        assert_eq!((read.of("a"), read.of("c")), ("", ""));
        assert_eq!(Value::read("{}"), list(Vec::new()));
        assert_eq!(Value::read("\"x\""), word("x"));
    }

    #[test]
    fn what_is_no_json_is_nothing() {
        let whole = r#"{"a":["x",{"b":"\u00e4"}],"c":1}"#;
        for (cut, _) in whole.char_indices() {
            assert_eq!(Value::read(&whole[..cut]), Value::Nothing, "{cut}");
        }
        for wrong in [
            "[1}",
            "{1:2}",
            r#"{"a"}"#,
            r#"["\u00g0"]"#,
            r#"["\u12"#,
            "[,",
        ] {
            assert_eq!(Value::read(wrong), Value::Nothing, "{wrong}");
        }
        assert_eq!(Value::read(&"[".repeat(100_000)), Value::Nothing);
        let deep = format!("{}1{}", "[".repeat(15), "]".repeat(15));
        assert_ne!(Value::read(&deep), Value::Nothing);
    }

    #[test]
    fn everything_that_is_written_is_json() {
        let all = All {
            yes: true,
            largest_number: u64::MAX,
            none: None,
            some: Some(7),
            name: Name::TheOne,
            shapes: vec![
                Shape::Nothing,
                Shape::SomeBox {
                    wide: 3,
                    tags: vec!["a", "b"],
                },
            ],
            kind: String::from("say \"名\"\n\\ \x01\x7f"),
        };
        let expected = concat!(
            r#"{"yes":true,"largestNumber":18446744073709551615,"none":null,"some":7,"#,
            r#""name":"theOne","shapes":[{"kind":"nothing"},"#,
            r#"{"kind":"someBox","wide":3,"tags":["a","b"]}],"#,
            r#""type":"say |"名|"|u000a|| |u0001"#,
            "\x7f\"}"
        );
        assert_eq!(json(&all), expected.replace('|', "\\"));
        assert_eq!(json(&Vec::<u8>::new()), "[]");
        assert_eq!(json(&0.5), "null");
    }
}
