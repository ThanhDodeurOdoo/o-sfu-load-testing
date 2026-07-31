#![allow(
    clippy::print_stdout,
    reason = "Cargo build scripts communicate directives through stdout"
)]

use std::{env, error::Error, fs, io, path::PathBuf};

fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo:rerun-if-changed=Cargo.lock");
    let manifest_directory = PathBuf::from(env::var("CARGO_MANIFEST_DIR")?);
    let lock = fs::read_to_string(manifest_directory.join("Cargo.lock"))?;
    let revision = locked_o_sfu_revision(&lock)
        .ok_or_else(|| io::Error::other("Cargo.lock has no o-sfu Git revision"))?;
    println!("cargo:rustc-env=O_SFU_LOCKED_REVISION={revision}");
    Ok(())
}

fn locked_o_sfu_revision(lock: &str) -> Option<&str> {
    lock.split("[[package]]").find_map(|package| {
        let is_o_sfu = package
            .lines()
            .any(|line| line.trim() == "name = \"o-sfu\"");
        if !is_o_sfu {
            return None;
        }
        package.lines().find_map(|line| {
            line.trim()
                .strip_prefix("source = \"git+https://github.com/ThanhDodeurOdoo/o-sfu")
                .and_then(|source| source.strip_suffix('"'))
                .and_then(|source| source.rsplit_once('#'))
                .map(|(_url, revision)| revision)
        })
    })
}
