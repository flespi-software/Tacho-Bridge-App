fn main() {
  // Display version for logs and the settings report. Pre-release versions
  // (containing `-`: alpha/beta/rc) get the git commit appended as semver
  // build metadata (`0.8.0-alpha.6+1a2b3c4`) because they are republished
  // across many commits under one version; stable versions map to exactly one
  // release, so they stay clean (`0.8.0`).
  let version = std::env::var("CARGO_PKG_VERSION").expect("cargo always sets CARGO_PKG_VERSION");
  let display = if version.contains('-') {
    let hash = git_short_hash().unwrap_or_else(|| "unknown".to_string());
    format!("{version}+{hash}")
  } else {
    version
  };
  println!("cargo:rustc-env=TBA_BUILD_VERSION={display}");
  // HEAD changes on every commit/branch switch; without this cargo would keep
  // the stale hash until the next full rebuild.
  println!("cargo:rerun-if-changed=../.git/HEAD");
  println!("cargo:rerun-if-changed=../.git/refs");

  tauri_build::build()
}

/// Short commit hash of the working tree, with `-dirty` appended when there
/// are uncommitted changes. None when git or the repo is unavailable
/// (e.g. building from a source tarball).
fn git_short_hash() -> Option<String> {
  let out = std::process::Command::new("git")
    .args(["rev-parse", "--short=7", "HEAD"])
    .output()
    .ok()
    .filter(|o| o.status.success())?;
  let mut hash = String::from_utf8_lossy(&out.stdout).trim().to_string();
  if hash.is_empty() {
    return None;
  }
  let dirty = std::process::Command::new("git")
    .args(["status", "--porcelain"])
    .output()
    .ok()
    .filter(|o| o.status.success())
    .map(|o| !o.stdout.is_empty())
    .unwrap_or(false);
  if dirty {
    hash.push_str("-dirty");
  }
  Some(hash)
}
