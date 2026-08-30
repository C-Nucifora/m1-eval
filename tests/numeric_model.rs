// SPDX-License-Identifier: GPL-3.0-or-later
//! Regression guard for the final M1-width runtime numeric model.

use std::fs;
use std::path::{Path, PathBuf};

fn rust_sources(dir: &Path, files: &mut Vec<PathBuf>) {
    let mut entries = fs::read_dir(dir)
        .expect("source directory is readable")
        .map(|entry| entry.expect("source entry is readable").path())
        .collect::<Vec<_>>();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            rust_sources(&path, files);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
}

fn has_path_token(source: &str, token: &str) -> bool {
    source.match_indices(token).any(|(offset, _)| {
        source[..offset]
            .chars()
            .next_back()
            .is_none_or(|before| !before.is_ascii_alphanumeric() && before != '_')
    })
}

#[test]
fn runtime_source_has_no_host_width_value_variants_or_adapters() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_sources(&root, &mut files);

    let value_paths =
        ["Int", "Uint", "Float"].map(|variant| ["Value", "::", variant, "("].concat());
    let removed_names = [
        ["Legacy", "Numeric", "Kind"].concat(),
        ["into", "_legacy", "_value"].concat(),
        ["into", "_legacy", "_builtin", "_argument"].concat(),
        ["legacy", "_builtin", "_arguments"].concat(),
        ["restore", "_legacy", "_builtin", "_result"].concat(),
        ["try", "_as", "_m1", "_scalar"].concat(),
        ["legacy", "_numeric", "_kind"].concat(),
    ];

    let value_source = fs::read_to_string(root.join("value.rs")).expect("value.rs is UTF-8");
    for variant in ["Int", "Uint", "Float"] {
        let declaration = ["\n    ", variant, "("].concat();
        assert!(
            !value_source.contains(&declaration),
            "value.rs still declares removed runtime variant {variant}"
        );
    }

    for path in files {
        let source = fs::read_to_string(&path).expect("Rust source is UTF-8");
        for token in &value_paths {
            assert!(
                !has_path_token(&source, token),
                "{} still contains removed runtime path {token}",
                path.display()
            );
        }
        for name in &removed_names {
            assert!(
                !source.contains(name),
                "{} still contains removed numeric adapter {name}",
                path.display()
            );
        }
    }
}
