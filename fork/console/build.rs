//! Embeds the built console (`web/dist`) in the crate as a table of `(path, bytes)`.
//!
//! A checkout without `web/dist` still builds: the table is empty and the console
//! route answers 503 with the command that builds it. Set
//! `AWS_EMULATOR_REQUIRE_CONSOLE=1` to make a missing bundle a build error instead,
//! which is what CI does so a published binary never ships without it.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

const REQUIRE_ENV: &str = "AWS_EMULATOR_REQUIRE_CONSOLE";

fn main() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR"));
    let dist = manifest_dir.join("web").join("dist");
    println!("cargo:rerun-if-changed={}", dist.display());
    println!("cargo:rerun-if-env-changed={REQUIRE_ENV}");

    let mut files = Vec::new();
    if dist.is_dir() {
        collect(&dist, &dist, &mut files);
    }
    files.sort();
    if files.is_empty() {
        if std::env::var(REQUIRE_ENV).is_ok_and(|value| value == "1") {
            panic!(
                "{REQUIRE_ENV}=1 but {} holds no built console; run `npm ci && npm run build` in fork/console/web",
                dist.display()
            );
        }
        println!(
            "cargo:warning=the console is not built ({} is empty); /_console/ will answer 503",
            dist.display()
        );
    }

    let mut table = String::from("pub static BUNDLE: &[(&str, &[u8])] = &[\n");
    for (relative, absolute) in &files {
        writeln!(table, "    ({relative:?}, include_bytes!({absolute:?})),").expect("write to a String");
    }
    table.push_str("];\n");
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("cargo sets OUT_DIR")).join("bundle.rs");
    std::fs::write(&out, table).unwrap_or_else(|error| panic!("write {}: {error}", out.display()));
}

/// Every file under `dir`, as (path relative to `root` with `/` separators, absolute path).
fn collect(root: &Path, dir: &Path, files: &mut Vec<(String, String)>) {
    let entries = std::fs::read_dir(dir).unwrap_or_else(|error| panic!("read {}: {error}", dir.display()));
    for entry in entries {
        let path = entry.unwrap_or_else(|error| panic!("read an entry of {}: {error}", dir.display())).path();
        if path.is_dir() {
            collect(root, &path, files);
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .expect("a file under the root")
            .components()
            .map(|part| part.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        files.push((relative, path.display().to_string()));
    }
}
