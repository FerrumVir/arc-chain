use sha2::{Digest, Sha256};
use std::{env, fs, path::PathBuf};
fn main() {
    let path = "reference/model.rs";
    println!("cargo:rerun-if-changed={path}");
    let bytes = fs::read(path).unwrap();
    assert_eq!(
        format!("{:x}", Sha256::digest(&bytes)),
        "15f2baef5e3db2a54ecba6831ee25d02570c84b3ce6026ba87cd3ca012af29eb"
    );
    let source = String::from_utf8(bytes).unwrap();
    let mut source = source
        .replace("//!", "//")
        .replace("super::", "arc_inference::modern::mla::")
        .replace("crate::modern::", "arc_inference::modern::")
        .replace(
            "ModernError::io(&context, e)",
            "ModernError::Io(format!(\"{context}: {e}\"))",
        );
    // Upstream in-crate tests reference private helpers. They are tested in the
    // pinned upstream workspace, not recompiled in this diagnostic module.
    if let Some(end) = source.find("\n#[cfg(test)]") {
        source.truncate(end);
    }
    let needle = "        combine(&weights, &outputs, &shared, &mut out)?;";
    assert_eq!(source.matches(needle).count(), 1);
    source = source.replace(needle, &format!("{needle}\n        ROUTES.with(|r| r.borrow_mut().push(serde_json::json!({{\"experts\":chosen,\"weights\":weights,\"shared\":shared,\"input\":x}})));"));
    source.push_str("\nthread_local! { static ROUTES: std::cell::RefCell<Vec<serde_json::Value>> = const { std::cell::RefCell::new(Vec::new()) }; }\npub fn take_routes() -> Vec<serde_json::Value> { ROUTES.with(|r| std::mem::take(&mut *r.borrow_mut())) }\n");
    fs::write(
        PathBuf::from(env::var("OUT_DIR").unwrap()).join("observed_model.rs"),
        source,
    )
    .unwrap();
}
