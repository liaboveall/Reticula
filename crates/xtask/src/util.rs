//! Process and path helpers shared by the tasks.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Tasks report failures as a message for the person at the terminal.
pub type Result<T = ()> = std::result::Result<T, String>;

/// The repository root.
pub fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

/// Announces the next step of a task.
pub fn step(title: &str) {
    println!("\n==> {title}");
}

/// Command-line flags of one task: `--switch` and `--option value`.
pub struct Flags {
    switches: Vec<String>,
    options: Vec<(String, String)>,
}

impl Flags {
    /// Parses `args`, accepting only the listed switches and options.
    pub fn parse(args: &[String], switches: &[&str], options: &[&str]) -> Result<Self> {
        let mut flags = Self {
            switches: Vec::new(),
            options: Vec::new(),
        };
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            let Some(name) = arg.strip_prefix("--") else {
                return Err(format!("unexpected argument `{arg}`"));
            };
            if switches.contains(&name) {
                flags.switches.push(name.to_owned());
            } else if options.contains(&name) {
                let value = args
                    .next()
                    .ok_or_else(|| format!("`--{name}` needs a value"))?;
                flags.options.push((name.to_owned(), value.clone()));
            } else {
                return Err(format!("unknown option `--{name}`"));
            }
        }
        Ok(flags)
    }

    /// Whether `--name` was given.
    pub fn has(&self, name: &str) -> bool {
        self.switches.iter().any(|switch| switch == name)
    }

    /// The first flag given that is not one of these.
    pub fn other_than(&self, known: &[&str]) -> Option<&str> {
        let options = self.options.iter().map(|(option, _)| option);
        self.switches
            .iter()
            .chain(options)
            .map(String::as_str)
            .find(|name| !known.contains(name))
    }

    /// The value of `--name value`, if given.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.options
            .iter()
            .find(|(option, _)| option == name)
            .map(|(_, value)| value.as_str())
    }
}

/// A child process, run from the repository root with inherited output.
pub struct Cmd {
    command: Command,
    display: String,
}

impl Cmd {
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        let program = program.as_ref();
        let mut command = Command::new(program);
        command.current_dir(root());
        let display = Path::new(program)
            .file_stem()
            .unwrap_or(program)
            .to_string_lossy()
            .into_owned();
        Self { command, display }
    }

    pub fn arg(mut self, arg: impl AsRef<OsStr>) -> Self {
        self.display.push(' ');
        self.display.push_str(&arg.as_ref().to_string_lossy());
        self.command.arg(arg);
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        for arg in args {
            self = self.arg(arg);
        }
        self
    }

    pub fn env(mut self, key: &str, value: impl AsRef<OsStr>) -> Self {
        self.command.env(key, value);
        self
    }

    /// Runs the command and fails unless it exits successfully.
    pub fn run(mut self) -> Result {
        println!("$ {}", self.display);
        let status = self
            .command
            .status()
            .map_err(|error| format!("cannot start `{}`: {error}", self.display))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("`{}` failed ({status})", self.display))
        }
    }

    /// Runs the command quietly and returns what it printed.
    pub fn output(mut self) -> Result<String> {
        let output = self
            .command
            .output()
            .map_err(|error| format!("cannot start `{}`: {error}", self.display))?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        } else {
            Err(format!(
                "`{}` failed ({}): {}",
                self.display,
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ))
        }
    }
}

/// The cargo that is running this task.
pub fn cargo() -> Cmd {
    Cmd::new(std::env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo")))
}

/// pnpm. On Windows it is a script, which only the command interpreter can start.
pub fn pnpm() -> Cmd {
    if cfg!(windows) {
        Cmd::new("cmd").args(["/C", "pnpm"])
    } else {
        Cmd::new("pnpm")
    }
}

pub fn node() -> Cmd {
    Cmd::new("node")
}

/// A pinned command-line tool: the copy in `.tools/` if `cargo xtask setup`
/// installed one, otherwise whatever is on the path (as on CI).
pub fn tool(name: &str) -> PathBuf {
    let local = root()
        .join(".tools/bin")
        .join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    if local.is_file() {
        local
    } else {
        PathBuf::from(name)
    }
}
