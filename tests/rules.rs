//! Rules that keep ccorbit's promises and its docs true, checked on the source itself. Each
//! failure says where, why the rule exists, and how to fix it: these messages are written for
//! whoever (or whatever agent) made the change. See docs/INVARIANTS.md.

use std::fs;
use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Every Rust source file, with its path relative to the repository.
fn sources() -> Vec<(String, String)> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(dir).expect("read src").flatten() {
            let p = entry.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|e| e == "rs") {
                out.push(p);
            }
        }
    }
    let mut files = Vec::new();
    walk(&root().join("src"), &mut files);
    files.sort();
    files
        .into_iter()
        .map(|p| {
            let rel = p.strip_prefix(root()).expect("inside the repo").to_string_lossy().replace('\\', "/");
            (rel, fs::read_to_string(&p).expect("read source"))
        })
        .collect()
}

/// The lines of a file outside its `#[cfg(test)]` module, with their numbers: tests may write
/// temporary files; the program may not.
fn program_lines(text: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim_start().starts_with("#[cfg(test)]") && text.lines().nth(i + 1).is_some_and(|l| l.trim_start().starts_with("mod tests")) {
            break;
        }
        out.push((i + 1, line));
    }
    out
}

/// Lines that use any of `patterns`, unless the line or the one above carries `allow`.
fn offenders(patterns: &[&str], allow: &str, only: impl Fn(&str) -> bool) -> Vec<String> {
    let mut found = Vec::new();
    for (file, text) in sources() {
        if !only(&file) {
            continue;
        }
        let lines = program_lines(&text);
        for (k, &(n, line)) in lines.iter().enumerate() {
            let code = line.split("//").next().unwrap_or("");
            let Some(p) = patterns.iter().find(|p| code.contains(*p)) else { continue };
            let marked = line.contains(allow) || (k > 0 && lines[k - 1].1.contains(allow));
            if !marked {
                found.push(format!("  {file}:{n}: `{p}` in `{}`", line.trim()));
            }
        }
    }
    found
}

#[test]
fn the_program_never_writes_files() {
    let found = offenders(
        &["fs::write", "File::create", "OpenOptions", "create_dir", "remove_file", "remove_dir", "fs::rename", "fs::copy", "set_permissions"],
        "harness: allow write",
        |_| true,
    );
    assert!(
        found.is_empty(),
        "ccorbit promises it never writes anything anywhere (README, docs/INVARIANTS.md), but these lines write files:\n{}\n\n\
         Fix: read instead, or keep the data in memory. A developer-only output (like --snapshot-html) may write: put \
         `// harness: allow write (why)` on the line above it.",
        found.join("\n")
    );
}

#[test]
fn the_program_never_runs_a_command() {
    let found = offenders(&["Command::new", "process::Command"], "harness: allow command", |_| true);
    assert!(
        found.is_empty(),
        "ccorbit never runs a command (docs/INVARIANTS.md). These lines do:\n{}\n\n\
         Fix: read the information from a file instead.",
        found.join("\n")
    );
}

#[test]
fn the_program_never_opens_a_network_connection() {
    let found = offenders(&["std::net", "TcpStream", "TcpListener", "UdpSocket", "reqwest", "ureq", "hyper::"], "harness: allow network", |_| true);
    assert!(
        found.is_empty(),
        "ccorbit never opens a network connection (docs/INVARIANTS.md), except the read-only engine pulse, which only the `harness: allow network` lines in src/engines.rs may do. These lines do:\n{}\n\n\
         Fix: everything ccorbit shows comes from reading files (opencode's storage database); take the information from there.",
        found.join("\n")
    );
}

#[test]
fn the_data_layer_knows_nothing_of_the_screen() {
    let data =
        ["src/model.rs", "src/parse.rs", "src/time.rs", "src/price.rs", "src/insight.rs", "src/source.rs", "src/demo.rs", "src/opencode.rs", "src/engines.rs"];
    let found = offenders(&["ratatui", "crate::ui", "crate::hud", "crossterm"], "harness: allow ui", |f| data.contains(&f));
    assert!(
        found.is_empty(),
        "The data layer ({}) reads and computes; only src/ui* and src/hud* draw (docs/ARCHITECTURE.md). These lines \
         reach for the screen:\n{}\n\nFix: return plain data (numbers, strings, enums) and let src/ui or src/hud turn it into \
         spans and colors.",
        data.join(", "),
        found.join("\n")
    );
}

#[test]
fn dependencies_stay_few() {
    let manifest = fs::read_to_string(root().join("Cargo.toml")).expect("Cargo.toml");
    let deps: Vec<&str> = manifest
        .split("[dependencies]")
        .nth(1)
        .unwrap_or("")
        .lines()
        .take_while(|l| !l.starts_with('['))
        .filter_map(|l| l.split('=').next().map(str::trim).filter(|k| !k.is_empty() && !k.starts_with('#')))
        .collect();
    let allowed = ["ratatui", "serde_json", "rusqlite"];
    let extra: Vec<&&str> = deps.iter().filter(|d| !allowed.contains(d)).collect();
    assert!(
        extra.is_empty(),
        "ccorbit runs on few dependencies, to stay small and quick to build; new ones: {extra:?}.\n\
         Fix: do without it, or record why it is worth it in docs/DECISIONS.md and add it to the list in this test."
    );
}

/// `--options` in the text, in order of appearance.
fn options_in(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for w in text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-')) {
        if w.len() > 3 && w.starts_with("--") && w.chars().nth(2).is_some_and(|c| c.is_ascii_lowercase()) && !out.iter().any(|o| o == w) {
            out.push(w.to_string());
        }
    }
    out
}

#[test]
fn every_option_is_documented_and_every_documented_option_exists() {
    let main = fs::read_to_string(root().join("src/main.rs")).expect("main.rs");
    let help = main.split("const HELP: &str = \"").nth(1).and_then(|h| h.split("\";").next()).expect("HELP text in main.rs");
    let readme = fs::read_to_string(root().join("README.md")).expect("README.md");
    let missing: Vec<String> = options_in(help).into_iter().filter(|o| !readme.contains(&format!("`{o}"))).collect();
    assert!(missing.is_empty(), "Options in `ccorbit --help` that the README does not explain: {missing:?}.\nFix: add each to the Options table of README.md.");
    let table: Vec<String> = readme.lines().filter(|l| l.starts_with("| `--")).flat_map(options_in).collect();
    let gone: Vec<&String> = table.iter().filter(|o| !help.contains(o.as_str())).collect();
    assert!(
        gone.is_empty(),
        "The README's Options table lists options that `ccorbit --help` does not have: {gone:?}.\nFix: remove them from README.md, or add them to HELP in src/main.rs."
    );
}

#[test]
fn every_source_file_is_on_the_maps() {
    let readme = fs::read_to_string(root().join("README.md")).expect("README.md");
    let arch = fs::read_to_string(root().join("docs/ARCHITECTURE.md")).unwrap_or_default();
    let mut missing = Vec::new();
    for (file, _) in sources() {
        for (doc, text) in [("README.md", &readme), ("docs/ARCHITECTURE.md", &arch)] {
            if !text.contains(&format!("`{file}`")) {
                missing.push(format!("  {file} is not in {doc}"));
            }
        }
    }
    assert!(
        missing.is_empty(),
        "Agents find their way through the module maps; these files are missing from them:\n{}\n\nFix: add a line for each \
         (what the file does, in a few words) to the Development table of README.md and to docs/ARCHITECTURE.md.",
        missing.join("\n")
    );
}

#[test]
fn the_changelog_covers_the_version() {
    let manifest = fs::read_to_string(root().join("Cargo.toml")).expect("Cargo.toml");
    let version = manifest.lines().find_map(|l| l.strip_prefix("version = \"")).and_then(|v| v.strip_suffix('"')).expect("version");
    let changelog = fs::read_to_string(root().join("CHANGELOG.md")).expect("CHANGELOG.md");
    assert!(
        changelog.contains(&format!("## {version}")),
        "Cargo.toml says {version} but CHANGELOG.md has no `## {version}` section.\nFix: move the Unreleased notes under `## {version} - <date>`."
    );
}
