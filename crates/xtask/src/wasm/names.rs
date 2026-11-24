//! Short names for what a WebAssembly module and its JavaScript bindings have of
//! each other.
//!
//! wasm-bindgen puts a hash into the name of every function the module imports,
//! and writes out what every method it exports is called. Only the bindings use
//! these names, hundreds of them, and compression cannot do much about hashes.

use std::collections::BTreeMap;

use crate::util::Result;

/// The one export that is used by others than the bindings.
const MEMORY: &str = "memory";

const HEADER: &[u8] = b"\0asm\x01\0\0\0";
const IMPORTS: u8 = 2;
const EXPORTS: u8 = 7;
const FUNCTION: u8 = 0;

/// The new names by the old ones.
type Names<'a> = BTreeMap<&'a str, String>;

/// A module, its bindings and the types of both, all with the same names.
#[derive(Debug, PartialEq, Eq)]
pub struct Package {
    pub module: Vec<u8>,
    pub bindings: String,
    /// The types of the bindings.
    pub types: String,
    /// The types of the module.
    pub module_types: String,
}

impl Package {
    pub fn shorten(&self) -> Result<Self> {
        let (module, imports, exports) = shorten_module(&self.module)?;
        let bindings = rename_imports(&self.bindings, &imports)?;
        Ok(Self {
            module,
            bindings: rename_exports(&bindings, &exports),
            types: rename_types(&self.types, "export interface InitOutput {", &exports),
            module_types: rename_types(&self.module_types, "", &exports),
        })
    }
}

/// `a` to `z`, `A` to `Z`, `aa` and so on.
fn short_name(mut index: usize) -> String {
    const LETTERS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
    let mut name: Vec<u8> = Vec::new();
    loop {
        name.extend(LETTERS.get(index % LETTERS.len()));
        match (index / LETTERS.len()).checked_sub(1) {
            Some(next) => index = next,
            None => break,
        }
    }
    name.iter().rev().copied().map(char::from).collect()
}

/// The module with short names, and the names of its imports and exports.
fn shorten_module(module: &[u8]) -> Result<(Vec<u8>, Names<'_>, Names<'_>)> {
    let Some(sections) = module.strip_prefix(HEADER) else {
        return Err(String::from("not a WebAssembly module"));
    };
    let mut sections = Reader(sections);
    let (mut imports, mut exports) = (Names::new(), Names::new());
    let mut out = HEADER.to_vec();
    while !sections.0.is_empty() {
        let id = sections.byte()?;
        let length = sections.number()?;
        let body = sections.bytes(length)?;
        match id {
            IMPORTS => section(&mut out, id, &shorten_imports(body, &mut imports)?),
            EXPORTS => section(&mut out, id, &shorten_exports(body, &mut exports)?),
            _ => section(&mut out, id, body),
        }
    }
    Ok((out, imports, exports))
}

fn shorten_imports<'a>(body: &'a [u8], names: &mut Names<'a>) -> Result<Vec<u8>> {
    let mut body = Reader(body);
    let count = body.number()?;
    let mut out = number(count);
    for index in 0..count {
        let module = body.name()?;
        let name = body.name()?;
        let kind = body.byte()?;
        if kind != FUNCTION {
            return Err(format!("`{name}` is imported, but not a function"));
        }
        let short = short_name(index);
        for text in [module, &short] {
            out.extend(number(text.len()));
            out.extend(text.as_bytes());
        }
        out.push(kind);
        out.extend(number(body.number()?));
        if names.insert(name, short).is_some() {
            return Err(format!("`{name}` is imported twice"));
        }
    }
    Ok(out)
}

fn shorten_exports<'a>(body: &'a [u8], names: &mut Names<'a>) -> Result<Vec<u8>> {
    let mut body = Reader(body);
    let count = body.number()?;
    let mut out = number(count);
    for index in 0..count {
        let name = body.name()?;
        let short = if name == MEMORY {
            name.to_owned()
        } else {
            short_name(index)
        };
        out.extend(number(short.len()));
        out.extend(short.as_bytes());
        out.push(body.byte()?);
        out.extend(number(body.number()?));
        names.insert(name, short);
    }
    Ok(out)
}

fn section(out: &mut Vec<u8>, id: u8, body: &[u8]) {
    out.push(id);
    out.extend(number(body.len()));
    out.extend(body);
}

/// The bindings hand the module an object with a function for each name.
fn rename_imports(bindings: &str, names: &Names<'_>) -> Result<String> {
    let mut out = bindings.to_owned();
    for (name, short) in names {
        let written = format!(" {name}: function(");
        if out.matches(&written).count() != 1 {
            return Err(format!(
                "`{name}` is not in the bindings once: they are not what wasm-bindgen used to write"
            ));
        }
        out = out.replacen(&written, &format!(" {short}: function("), 1);
    }
    Ok(out)
}

/// The bindings keep the module's exports in a variable.
fn rename_exports(bindings: &str, names: &Names<'_>) -> String {
    const VARIABLE: &str = "wasm.";
    let mut out = String::with_capacity(bindings.len());
    let mut rest = bindings;
    while let Some((before, after)) = rest.split_once(VARIABLE) {
        out.push_str(before);
        out.push_str(VARIABLE);
        let is_variable = !before.ends_with(is_identifier);
        let (name, following) = split_identifier(after);
        match names.get(name) {
            Some(short) if is_variable => out.push_str(short),
            _ => out.push_str(name),
        }
        rest = following;
    }
    out.push_str(rest);
    out
}

/// Renames what is declared after a line, one to a line.
fn rename_types(types: &str, first: &str, names: &Names<'_>) -> String {
    let mut inside = first.is_empty();
    let mut out = String::with_capacity(types.len());
    for line in types.split_inclusive('\n') {
        let declared = ["    readonly ", "export const "]
            .into_iter()
            .find_map(|keyword| Some((keyword, line.strip_prefix(keyword)?)))
            .filter(|_| inside);
        let renamed = declared.and_then(|(keyword, rest)| {
            let (name, following) = split_identifier(rest);
            Some(format!("{keyword}{}{following}", names.get(name)?))
        });
        out.push_str(renamed.as_deref().unwrap_or(line));
        inside |= line.trim_end() == first;
    }
    out
}

fn is_identifier(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '$')
}

fn split_identifier(text: &str) -> (&str, &str) {
    let end = text.find(|c| !is_identifier(c)).unwrap_or(text.len());
    text.split_at(end)
}

/// An unsigned number as modules have it: seven bits to a byte, the lowest first.
fn number(mut value: usize) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let low = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(low);
            return out;
        }
        out.push(low | 0x80);
    }
}

/// What is left of a module to read.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn bytes(&mut self, count: usize) -> Result<&'a [u8]> {
        let Some((read, rest)) = self.0.split_at_checked(count) else {
            return Err(String::from("the WebAssembly module ends too soon"));
        };
        self.0 = rest;
        Ok(read)
    }

    fn byte(&mut self) -> Result<u8> {
        Ok(self.bytes(1)?.first().copied().unwrap_or_default())
    }

    fn number(&mut self) -> Result<usize> {
        let mut value: usize = 0;
        for shift in [0, 7, 14, 21, 28] {
            let byte = self.byte()?;
            value |= usize::from(byte & 0x7f) << shift;
            if byte < 0x80 {
                return Ok(value);
            }
        }
        Err(String::from(
            "a number in the WebAssembly module is too long",
        ))
    }

    fn name(&mut self) -> Result<&'a str> {
        let length = self.number()?;
        std::str::from_utf8(self.bytes(length)?)
            .map_err(|e| format!("a name in the WebAssembly module is not text: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(text: &str) -> Vec<u8> {
        [number(text.len()), text.as_bytes().to_vec()].concat()
    }

    /// Two functions imported, two exported and the memory, and a section of another kind.
    fn module(imports: [&str; 2], exports: [&str; 2]) -> Vec<u8> {
        let mut out = HEADER.to_vec();
        section(&mut out, 1, &[1, 0x60, 0, 0]);
        let import = |text: &str| [name("./app_bg.js"), name(text), vec![FUNCTION, 0]].concat();
        let body = [vec![2], import(imports[0]), import(imports[1])].concat();
        section(&mut out, IMPORTS, &body);
        let export =
            |text: &str, kind, index: usize| [name(text), vec![kind], number(index)].concat();
        let body = [
            vec![3],
            export(MEMORY, 2, 0),
            export(exports[0], FUNCTION, 2),
            export(exports[1], FUNCTION, 300),
        ]
        .concat();
        section(&mut out, EXPORTS, &body);
        out
    }

    const BINDINGS: &str = r#"/* @ts-self-types="./app_wasm.d.ts" */
        take() { return wasm.engine_take(this.ptr); }
        all() { wasm.engine_takeAll(wasm.memory.buffer); other_wasm.engine_take(); }
        __wbg_get_635eea311fa1a291: function(arg0) {
        __wbg_get_d7176cfb76d71114: function(arg0) {
"#;

    fn package() -> Package {
        Package {
            module: module(
                ["__wbg_get_635eea311fa1a291", "__wbg_get_d7176cfb76d71114"],
                ["engine_take", "engine_takeAll"],
            ),
            bindings: BINDINGS.to_owned(),
            types: String::from(
                "export interface Other {\n    readonly engine_take: string;\n}\n\
                 export interface InitOutput {\n    readonly memory: WebAssembly.Memory;\n    \
                 readonly engine_takeAll: () => void;\n}\n",
            ),
            module_types: String::from(
                "export const memory: WebAssembly.Memory;\nexport const engine_take: () => void;\n",
            ),
        }
    }

    #[test]
    fn the_module_and_its_bindings_agree_on_short_names() {
        let short = package().shorten().unwrap();
        assert_eq!(short.module, module(["a", "b"], ["b", "c"]));
        assert_eq!(
            short.bindings,
            BINDINGS
                .replace("__wbg_get_635eea311fa1a291:", "a:")
                .replace("__wbg_get_d7176cfb76d71114:", "b:")
                .replace("return wasm.engine_take(", "return wasm.b(")
                .replace(" wasm.engine_takeAll(", " wasm.c(")
        );
        assert_eq!(
            short.types,
            package().types.replace("engine_takeAll", "c"),
            "only what the module exports has new names"
        );
        assert_eq!(
            short.module_types,
            package().module_types.replace("engine_take", "b")
        );
    }

    #[test]
    fn short_names_are_all_different() {
        let names: std::collections::BTreeSet<String> = (0..3_000).map(short_name).collect();
        assert_eq!(names.len(), 3_000);
        assert_eq!(
            [0, 25, 26, 51, 52, 103, 104].map(short_name),
            ["a", "z", "A", "Z", "aa", "aZ", "ba"]
        );
    }

    #[test]
    fn numbers_are_read_as_they_are_written() {
        for value in [0, 1, 127, 128, 300, 16_383, 16_384, 1 << 31] {
            let bytes = number(value);
            let mut reader = Reader(&bytes);
            assert_eq!(reader.number(), Ok(value));
            assert!(reader.0.is_empty());
        }
        assert!(Reader(&[0x80; 6]).number().is_err());
        assert!(Reader(&[0x80]).number().is_err());
    }

    #[test]
    fn what_is_not_understood_is_an_error() {
        let wrong = |change: &dyn Fn(&mut Package)| {
            let mut package = package();
            change(&mut package);
            package.shorten().unwrap_err()
        };
        assert!(wrong(&|p| p.module.truncate(60)).contains("too soon"));
        assert!(wrong(&|p| p.module = b"\0asm\x02\0\0\0".to_vec()).contains("not a"));
        assert!(wrong(&|p| p.bindings.push_str(BINDINGS)).contains("wasm-bindgen"));
        assert!(wrong(&|p| p.bindings.clear()).contains("wasm-bindgen"));
        let twice = |p: &mut Package| p.module = module(["f", "f"], ["g", "h"]);
        assert!(wrong(&twice).contains("twice"));
        let memory = |p: &mut Package| {
            p.module = HEADER.to_vec();
            let body = [vec![1], name("env"), name("memory"), vec![2, 0, 1]].concat();
            section(&mut p.module, IMPORTS, &body);
        };
        assert!(wrong(&memory).contains("not a function"));
    }
}
