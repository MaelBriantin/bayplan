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

/// Where the update goes, against where the binary is.
#[derive(Debug, PartialEq, Eq)]
enum Target {
    /// Where it is, as the updater finds it.
    Same,
    /// Its root, told in so many words: the installer put the binary straight in a `bin` folder,
    /// in which the updater would otherwise create another `bin` folder.
    Root(PathBuf),
}

fn target(prefix: &Path, flat: bool) -> Target {
    if flat && prefix.ends_with("bin") {
        prefix
            .parent()
            .map_or(Target::Same, |root| Target::Root(root.to_path_buf()))
    } else {
        Target::Same
    }
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
    let prefix = PathBuf::from(updater.install_prefix_root()?.as_str());
    let flat = receipt_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .is_some_and(|receipt| receipt.contains(r#""install_layout":"flat""#));
    if let Target::Root(root) = target(&prefix, flat) {
        updater.set_install_dir(root.to_string_lossy().as_ref());
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
    fn the_flat_layout_is_given_its_root_the_others_stay_where_they_are() {
        let home = Path::new("/home/dev");
        assert_eq!(
            target(&home.join(".local/bin"), true),
            Target::Root(home.join(".local"))
        );
        assert_eq!(target(&home.join(".cargo"), false), Target::Same);
        assert_eq!(target(&home.join(".local"), false), Target::Same);
    }
}
