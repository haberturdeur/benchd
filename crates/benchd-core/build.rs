use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=BENCHD_BUILD_ID");
    // Track all component sources, including uncommitted changes. HEAD/index
    // alone miss edits made without committing or staging them.
    println!("cargo:rerun-if-changed=..");
    println!("cargo:rerun-if-changed=../../Cargo.toml");
    println!("cargo:rerun-if-changed=../../Cargo.lock");
    for name in ["HEAD", "index", "refs"] {
        if let Some(path) = git(&["rev-parse", "--git-path", name]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    let build = std::env::var("BENCHD_BUILD_ID")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| git(&["describe", "--always", "--dirty", "--abbrev=12"]))
        .unwrap_or_else(|| "unknown".into());
    let build: String = build
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(128)
        .collect();
    println!("cargo:rustc-env=BENCHD_BUILD_ID={build}");
}

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}
