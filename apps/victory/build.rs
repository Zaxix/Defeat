//! Names the source the program was built from (`VICTORY_SOURCE`, the
//! commit and whether the tree had changes), for the run log's first
//! lines: research R61 asked every run to say what it ran.

fn main() {
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
    };
    let commit = git(&["rev-parse", "--short=12", "HEAD"]).unwrap_or_else(|| "unknown".to_owned());
    let changed = git(&["status", "--porcelain", "--untracked-files=no"]).is_some_and(|s| !s.is_empty());
    println!(
        "cargo:rustc-env=VICTORY_SOURCE={commit}{}",
        if changed { " with local changes" } else { "" }
    );
    // Built again when the commit changes (the head, or the branch it names).
    for path in ["../../.git/HEAD", "../../.git/refs/heads", "../../.git/index"] {
        if std::path::Path::new(path).exists() {
            println!("cargo:rerun-if-changed={path}");
        }
    }
}
