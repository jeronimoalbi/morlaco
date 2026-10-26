//! Compiles the icons in `data/` into a GResource that the app embeds, so it
//! shows its icons even where no icon theme is installed (macOS, for one).

use std::path::PathBuf;
use std::process::Command;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").ok_or("OUT_DIR is not set")?);
    let status = Command::new("glib-compile-resources")
        .arg("--sourcedir=data")
        .arg(format!(
            "--target={}",
            out_dir.join("morlaco.gresource").display()
        ))
        .arg("data/morlaco.gresource.xml")
        .status()
        .map_err(|e| format!("could not run glib-compile-resources (part of glib): {e}"))?;
    if !status.success() {
        return Err(format!("glib-compile-resources failed: {status}").into());
    }
    println!("cargo:rerun-if-changed=data");
    Ok(())
}
