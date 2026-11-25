//! Table files that were written by version 1 are read for ever, with the same cells.

#[cfg(target_arch = "wasm32")]
use wasm_bindgen_test::wasm_bindgen_test as test;

use reticula_table::determinism::{content_digest, to_hex};

const FILES: [(&str, &[u8]); 4] = [
    (
        "empty.rtbl",
        include_bytes!("../../../tests/compat/table-v1/empty.rtbl"),
    ),
    (
        "kinds.rtbl",
        include_bytes!("../../../tests/compat/table-v1/kinds.rtbl"),
    ),
    (
        "mixed.rtbl",
        include_bytes!("../../../tests/compat/table-v1/mixed.rtbl"),
    ),
    (
        "orders.rtbl",
        include_bytes!("../../../tests/compat/table-v1/orders.rtbl"),
    ),
];

#[test]
fn files_of_version_1_are_read() {
    let recorded = include_str!("../../../tests/compat/table-v1/expected_digests.txt");
    let recorded: Vec<(&str, &str)> = recorded
        .lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            Some((words.next()?, words.next()?))
        })
        .collect();
    assert_eq!(recorded.len(), FILES.len());
    for (name, file) in FILES {
        let digest = content_digest(file).map(to_hex);
        let digest = digest
            .as_ref()
            .and_then(|digest| core::str::from_utf8(digest).ok());
        assert!(
            recorded
                .iter()
                .any(|(hex, of)| *of == name && Some(*hex) == digest),
            "{name}: {digest:?}"
        );
    }
}
