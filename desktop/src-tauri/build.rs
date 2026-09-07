fn main() {
    println!("cargo:rerun-if-env-changed=ARC_BUILD_SOURCE_COMMIT");
    emit_git_rerun_path("HEAD");
    let sealed_commit = match std::env::var("ARC_BUILD_SOURCE_COMMIT") {
        Ok(value) if is_commit(&value) => Some(value),
        Ok(_) => panic!("ARC_BUILD_SOURCE_COMMIT must be exact lowercase 40-byte Git hex"),
        Err(std::env::VarError::NotPresent) => None,
        Err(std::env::VarError::NotUnicode(_)) => {
            panic!("ARC_BUILD_SOURCE_COMMIT must be valid UTF-8")
        }
    };
    let git_commit = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir("../..")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|value| value.trim().to_string())
        .filter(|value| is_commit(value));
    if let Ok(symbolic_ref) = std::process::Command::new("git")
        .args(["symbolic-ref", "-q", "HEAD"])
        .current_dir("../..")
        .output()
    {
        if symbolic_ref.status.success() {
            let reference = String::from_utf8_lossy(&symbolic_ref.stdout);
            let reference = reference.trim();
            if !reference.is_empty() {
                emit_git_rerun_path(reference);
            }
        }
    }
    if let (Some(sealed), Some(checked_out)) = (&sealed_commit, &git_commit) {
        assert_eq!(
            sealed, checked_out,
            "ARC_BUILD_SOURCE_COMMIT must equal the exact checked-out Git HEAD"
        );
    }
    let source_commit = sealed_commit
        .or(git_commit)
        .expect("ARC desktop build must be bound to one exact Git source commit");
    println!("cargo:rustc-env=ARC_BUILD_SOURCE_COMMIT={source_commit}");
    tauri_build::build()
}

fn emit_git_rerun_path(name: &str) {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--git-path", name])
        .current_dir("../..")
        .output();
    if let Ok(output) = output {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout);
            let path = path.trim();
            if !path.is_empty() {
                if std::path::Path::new(path).is_absolute() {
                    println!("cargo:rerun-if-changed={path}");
                } else {
                    println!("cargo:rerun-if-changed=../../{path}");
                }
            }
        }
    }
}

fn is_commit(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
