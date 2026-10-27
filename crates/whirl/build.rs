//! Embeds the built browser shim into the binary for `whirl install`.
//!
//! When `shim/dist` exists (mise builds it before packaging), this script
//! copies every `.js` file in it into `OUT_DIR/shim-dist/` and generates
//! `shim_embed.rs` with an `include_bytes!` entry per file. When the dist
//! tree is absent (a plain cargo build without a built shim), it generates
//! an empty table so the crate still compiles; `whirl install` then falls
//! back to the `WHIRL_SHIM_JS` tree as its shim source.
//!
//! This script only copies prebuilt output. It never invokes Bun or tsc:
//! the repository-owned-tasks ADR gives shim building to mise.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::{env, fs};

// Build scripts speak to cargo on stdout; clippy's print_stdout lint
// exempts them.
fn emit(directive: &str) {
    println!("{directive}");
}

fn main() {
    let manifest_dir =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR"));
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    let dist_dir = manifest_dir.join("../../shim/dist");
    emit(&format!("cargo::rerun-if-changed={}", dist_dir.display()));

    let copies_dir = out_dir.join("shim-dist");
    if copies_dir.exists() {
        fs::remove_dir_all(&copies_dir).expect("stale shim-dist copies should be removable");
    }
    fs::create_dir_all(&copies_dir).expect("OUT_DIR/shim-dist should be creatable");

    let names = copy_dist_js(&dist_dir, &copies_dir);
    let table = embed_table(&copies_dir, &names);
    fs::write(out_dir.join("shim_embed.rs"), table).expect("shim_embed.rs should be writable");
}

/// Copies every `.js` file under `dist_dir` into `copies_dir` and returns
/// the sorted relative paths. A missing dist tree yields an empty list.
fn copy_dist_js(dist_dir: &Path, copies_dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dist_dir) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    copy_js_entries(dist_dir, copies_dir, entries, &mut names);
    names.sort();
    names
}

fn copy_js_entries(root: &Path, copies_dir: &Path, entries: fs::ReadDir, names: &mut Vec<String>) {
    for entry in entries {
        let entry = entry.expect("shim/dist should be readable");
        let path = entry.path();
        let kind = entry
            .file_type()
            .expect("shim entry type should be readable");
        if kind.is_dir() {
            copy_js_entries(
                root,
                copies_dir,
                fs::read_dir(&path).expect("shim subdirectory should be readable"),
                names,
            );
        } else if kind.is_file() && path.extension().is_some_and(|ext| ext == "js") {
            let relative = path.strip_prefix(root).expect("shim file is under dist");
            let destination = copies_dir.join(relative);
            fs::create_dir_all(destination.parent().expect("copy has a parent"))
                .expect("shim copy directory should be creatable");
            fs::copy(&path, destination).expect("shim dist file should copy");
            names.push(
                relative
                    .to_str()
                    .expect("shim dist paths are UTF-8")
                    .to_owned(),
            );
        }
    }
}

/// Renders the generated static table of embedded shim files.
fn embed_table(copies_dir: &Path, names: &[String]) -> String {
    let mut table = String::from(
        "/// The embedded shim dist tree: `(file name, contents)` pairs.\n\
         /// Empty when the shim was not built before this binary.\n\
         pub(crate) static SHIM_DIST_FILES: &[(&str, &[u8])] = &[\n",
    );
    for name in names {
        let path = copies_dir.join(name);
        writeln!(
            table,
            "    ({name:?}, include_bytes!({path:?})),",
            path = path.display().to_string()
        )
        .expect("writing to a String cannot fail");
    }
    table.push_str("];\n");
    table
}
