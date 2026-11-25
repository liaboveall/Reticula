//! The space left on the volume the data is on.

use std::path::Path;

/// The bytes left for this process where `dir` is, or nothing when the
/// platform cannot tell.
///
/// Runs a command and may take a while: not for the async runtime.
pub fn available_bytes(dir: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        // The standard library has no call for it.
        let output = std::process::Command::new("df")
            .arg("-Pk")
            .arg(dir)
            .env("LC_ALL", "C")
            .stdin(std::process::Stdio::null())
            .output()
            .ok()?;
        output
            .status
            .success()
            .then(|| available_in(&String::from_utf8_lossy(&output.stdout)))?
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        None
    }
}

/// Reads the last line of the portable output of `df -k`. The names at both of
/// its ends may have spaces, so the column is found from the percentage after it.
#[cfg(any(unix, test))]
fn available_in(output: &str) -> Option<u64> {
    let line = output.lines().rfind(|line| !line.trim().is_empty())?;
    let fields: Vec<&str> = line.split_whitespace().collect();
    let percentage = fields.iter().position(|field| {
        field
            .strip_suffix('%')
            .is_some_and(|n| n.parse::<u64>().is_ok())
    })?;
    let kibibytes: u64 = fields.get(percentage.checked_sub(1)?)?.parse().ok()?;
    kibibytes.checked_mul(1024)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER: &str = "Filesystem     1024-blocks      Used Available Capacity Mounted on\n";

    #[test]
    fn reads_the_available_column() {
        for (line, expected) in [
            (
                "/dev/sda1        152243760  41226348 111001028      28% /var/lib/reticula\n",
                Some(111_001_028 * 1024),
            ),
            ("overlay 100 100 0 100% /", Some(0)),
            (
                "//host/some share 100 40 60 40% /mnt/a b\n\n",
                Some(60 * 1024),
            ),
            ("tmpfs - - - - /", None),
            ("", None),
        ] {
            assert_eq!(available_in(&format!("{HEADER}{line}")), expected, "{line}");
        }
        assert_eq!(available_in(""), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_directory_has_some_space() {
        let dir = tempfile::tempdir().unwrap();
        assert!(available_bytes(dir.path()).is_some_and(|bytes| bytes > 0));
        assert_eq!(available_bytes(&dir.path().join("none")), None);
    }
}
