//! `bp update` and `bp uninstall`, for a copy installed by the release installer, which leaves a
//! receipt saying where it came from. A copy built from source is managed the way it was built.

use std::path::{Path, PathBuf};

use anyhow::Result;
use axoupdater::AxoUpdater;

use crate::commands::confirm;

/// Name of the app in the releases and the installer receipt, not of the binary.
const APP: &str = "bayplan";

/// A GitHub token to ask for releases with, if set, against the rate limit of anonymous requests.
const TOKEN_VARIABLE: &str = "BAYPLAN_GITHUB_TOKEN";

/// Folder a flat install is updated in: the one of the running binary. The installer puts a flat install
/// exactly in the folder it is given, and the receipt may point elsewhere: `bp update` up to 0.2.0 wrote
/// the folder above `bin` in it. `None` keeps the folder of the receipt.
fn target(binary: &Path, flat: bool) -> Option<PathBuf> {
    flat.then(|| binary.parent().map(Path::to_path_buf))
        .flatten()
}

fn receipt_path() -> Option<PathBuf> {
    let file = format!("{APP}-receipt.json");
    let xdg = std::env::var_os("XDG_CONFIG_HOME")
        .map(|home| PathBuf::from(home).join(APP).join(&file))
        .filter(|path| path.exists());
    let home = std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config").join(APP));
    xdg.or_else(|| home.map(|folder| folder.join(&file)))
}

pub fn update() -> Result<()> {
    let mut updater = AxoUpdater::new_for(APP);
    if let Ok(token) = std::env::var(TOKEN_VARIABLE) {
        updater.set_github_token(&token);
    }
    if updater.load_receipt().is_err() {
        println!(
            "This copy of bp was not installed by its installer: update it the way you got it."
        );
        return Ok(());
    }
    let flat = receipt_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .is_some_and(|receipt| receipt.contains(r#""install_layout":"flat""#));
    let binary = std::env::current_exe()?.canonicalize()?;
    if let Some(folder) = target(&binary, flat) {
        updater.set_install_dir(folder.to_string_lossy().as_ref());
    }
    match updater.run_sync()? {
        Some(update) => println!("bp is now version {}.", update.new_version),
        None => println!("bp is up to date (version {}).", env!("CARGO_PKG_VERSION")),
    }
    Ok(())
}

pub fn uninstall() -> Result<()> {
    let mut updater = AxoUpdater::new_for(APP);
    let installed = updater.load_receipt().is_ok()
        && updater
            .check_receipt_is_for_this_executable()
            .unwrap_or(false);
    let Some(receipt) = receipt_path().filter(|path| installed && path.exists()) else {
        println!(
            "This copy of bp was not installed by its installer: remove it the way you got it."
        );
        return Ok(());
    };
    println!("Bays are left as they are: run `bp rm <bay>` first to remove them.");
    if !confirm("Remove bp from this computer?")? {
        println!("Nothing removed.");
        return Ok(());
    }
    self_replace::self_delete()?;
    std::fs::remove_file(&receipt)?;
    if let Some(folder) = receipt.parent() {
        // Removed only when nothing else is left in it.
        let _ = std::fs::remove_dir(folder);
    }
    println!("bp is removed.");
    println!(
        "The line the installer may have added to your shell profile, for your PATH, stays: it does no harm."
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_flat_install_is_updated_in_the_folder_of_the_binary() {
        let bin = Path::new("/home/dev/.local/bin");
        assert_eq!(target(&bin.join("bp"), true), Some(bin.to_path_buf()));
    }

    #[test]
    fn another_layout_keeps_the_folder_of_the_receipt() {
        assert_eq!(target(Path::new("/home/dev/.cargo/bin/bp"), false), None);
    }
}
