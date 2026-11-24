//! Takes out of wasm-bindgen's bindings what they have for browsers without
//! features that the app itself cannot do without, and for callers it has not.

use crate::util::Result;

/// How the app has a module loaded: from an address.
const LOAD: &str = "\
async function __wbg_init({ module_or_path }) {
    if (wasm !== undefined) return wasm;

    const response = await fetch(module_or_path);
    if (!response.ok) {
        throw new Error(`cannot load ${response.url}: HTTP ${response.status}`);
    }
    const imports = __wbg_get_imports();
    const { instance, module } = response.headers.get('Content-Type') === 'application/wasm'
        ? await WebAssembly.instantiateStreaming(response, imports)
        : await WebAssembly.instantiate(await response.arrayBuffer(), imports);
    return __wbg_finalize_init(instance, module);
}

export { __wbg_init as default };
";

/// Where each part of the bindings begins, what comes after it, and what takes its place.
const BINDINGS: [(&str, &str, &str); 4] = [
    // `TextEncoder.encodeInto` is older than the private fields the app uses.
    (
        "\nif (!('encodeInto' in cachedTextEncoder)) {\n",
        "\nlet WASM_VECTOR_LEN",
        "",
    ),
    // So is `FinalizationRegistry`.
    (
        "(typeof FinalizationRegistry === 'undefined')\n",
        "new FinalizationRegistry(",
        "",
    ),
    // For `using`, which the app does not use.
    ("if (Symbol.dispose) ", "\n", ""),
    // From everything else a module can be loaded from.
    ("async function __wbg_load(module, imports) {\n", "", LOAD),
];

/// The same for the types.
const TYPES: [(&str, &str, &str); 2] = [
    ("export type SyncInitInput = ", "/**\n * If ", ""),
    (
        "export default function __wbg_init (",
        ": Promise<InitOutput>;",
        "export default function __wbg_init (from: { module_or_path: RequestInfo | URL })",
    ),
];

/// The bindings and their types without any of them.
pub fn remove(bindings: &str, types: &str) -> Result<(String, String)> {
    Ok((replace(bindings, &BINDINGS)?, replace(types, &TYPES)?))
}

/// Replaces every part that begins and ends like this, up to the end of the text
/// if there is no other end.
fn replace(text: &str, parts: &[(&str, &str, &str)]) -> Result<String> {
    let unexpected = |part: &str| {
        Err(format!(
            "{part:?} is not in the bindings: they are not what wasm-bindgen used to write"
        ))
    };
    let mut out = text.to_owned();
    for &(start, end, replacement) in parts {
        let mut rest = out.as_str();
        let mut replaced = String::with_capacity(out.len());
        while let Some((before, after)) = rest.split_once(start) {
            let Some(length) = (if end.is_empty() {
                Some(after.len())
            } else {
                after.find(end)
            }) else {
                return unexpected(end);
            };
            replaced.push_str(before);
            replaced.push_str(replacement);
            rest = after.split_at(length).1;
        }
        if replaced.is_empty() {
            return unexpected(start);
        }
        replaced.push_str(rest);
        out = replaced;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GENERATED: &str = "\
export class Engine {}
if (Symbol.dispose) Engine.prototype[Symbol.dispose] = Engine.prototype.free;

export class Player {}
if (Symbol.dispose) Player.prototype[Symbol.dispose] = Player.prototype.free;

const EngineFinalization = (typeof FinalizationRegistry === 'undefined')
    ? { register: () => {}, unregister: () => {} }
    : new FinalizationRegistry(ptr => wasm.__wbg_engine_free(ptr, 1));
const cachedTextEncoder = new TextEncoder();

if (!('encodeInto' in cachedTextEncoder)) {
    cachedTextEncoder.encodeInto = function (arg, view) {
        return {
            read: arg.length
        };
    };
}

let WASM_VECTOR_LEN = 0;

async function __wbg_load(module, imports) {
    return await WebAssembly.instantiate(module, imports);
}

export { initSync, __wbg_init as default };
";

    const DECLARED: &str = "\
export interface InitOutput {
    readonly memory: WebAssembly.Memory;
}

export type SyncInitInput = BufferSource | WebAssembly.Module;

export function initSync(module: { module: SyncInitInput } | SyncInitInput): InitOutput;

/**
 * If `module_or_path` is {RequestInfo} or {URL}, makes a request.
 */
export default function __wbg_init (module_or_path?: { module_or_path: InitInput }): Promise<InitOutput>;
";

    #[test]
    fn what_is_left_is_what_the_app_uses() {
        let (bindings, types) = remove(GENERATED, DECLARED).unwrap();
        let kept = "\
export class Engine {}


export class Player {}


const EngineFinalization = new FinalizationRegistry(ptr => wasm.__wbg_engine_free(ptr, 1));
const cachedTextEncoder = new TextEncoder();

let WASM_VECTOR_LEN = 0;

";
        assert_eq!(bindings, format!("{kept}{LOAD}"));
        assert_eq!(
            types,
            "\
export interface InitOutput {
    readonly memory: WebAssembly.Memory;
}

/**
 * If `module_or_path` is {RequestInfo} or {URL}, makes a request.
 */
export default function __wbg_init (from: { module_or_path: RequestInfo | URL }): Promise<InitOutput>;
"
        );
    }

    #[test]
    fn bindings_of_another_kind_are_an_error() {
        for (start, end, _) in BINDINGS {
            let without = GENERATED.replace(start, "");
            assert!(
                remove(&without, DECLARED).is_err_and(|e| e.contains("wasm-bindgen")),
                "{start:?}"
            );
            let endless = GENERATED.replace(end, "");
            assert!(
                end.is_empty() || remove(&endless, DECLARED).is_err(),
                "{end:?}"
            );
        }
        for (start, _, _) in TYPES {
            assert!(remove(GENERATED, &DECLARED.replace(start, "")).is_err());
        }
    }
}
