// SPDX-License-Identifier: Apache-2.0
//! Stamps the commit into the binary, so `--version` can name the build.
//!
//! **Not a dependency and not a network call.** `git rev-parse` if there is a
//! git and it answers, `unknown` otherwise — a tarball build has no history and
//! must not fail for the want of one.

#![forbid(unsafe_code)]

use std::process::Command;

fn main() {
    let commit = Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()
        .filter(|it| it.status.success())
        .and_then(|it| String::from_utf8(it.stdout).ok())
        .map(|it| it.trim().to_owned())
        .filter(|it| !it.is_empty())
        .unwrap_or_else(|| "unknown".to_owned());
    println!("cargo:rustc-env=SKERRY_COMMIT={commit}");
    // Rerun when the commit could have changed, and never otherwise: without
    // this, cargo reruns the script on every build of every file in the crate.
    for path in [".git/HEAD", ".git/refs"] {
        println!("cargo:rerun-if-changed=../../{path}");
    }
}
