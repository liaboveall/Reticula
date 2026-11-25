//! What keeps the server from starting.

use std::process::Command;

fn start(trusted_proxies: &str) -> (bool, String) {
    let data = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_reticula-server"))
        .env("RUST_LOG", "info")
        .env("BIND_ADDR", "127.0.0.1:0")
        .env("RETICULA_STATIC_DIR", data.path())
        .env("RETICULA_DATA_DIR", data.path())
        // No address of a database, so it goes no further than this.
        .env("RETICULA_DATABASE_URL", "none")
        .env("RETICULA_PUBLIC_ORIGIN", "http://127.0.0.1:3200")
        .env("RETICULA_GITHUB_CLIENT_ID", "client")
        .env("RETICULA_GITHUB_CLIENT_SECRET", "secret")
        .env("RETICULA_AUTH_MODE", "github")
        .env("RETICULA_TRUSTED_PROXIES", trusted_proxies)
        .output()
        .unwrap();
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn proxies_that_cannot_be_read() {
    for raw in ["nginx", "172.18.0.0/33", "172.18.0.1/16", "127.0.0.1,"] {
        let (started, log) = start(raw);
        assert!(!started, "{raw}");
        assert!(
            log.contains("invalid RETICULA_TRUSTED_PROXIES"),
            "{raw}: {log}"
        );
    }
}

#[test]
fn proxies_that_can_be_read_do_not() {
    for raw in ["", "127.0.0.0/8,::1/128,172.16.0.0/12"] {
        let (started, log) = start(raw);
        assert!(!started, "{raw}");
        assert!(
            log.contains("could not initialize collaboration storage"),
            "{raw}: {log}"
        );
    }
}
