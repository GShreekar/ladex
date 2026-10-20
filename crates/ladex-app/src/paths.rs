//! The folders the app's node uses on each platform.

use std::path::PathBuf;

use tauri::{AppHandle, Manager};

/// Where the node keeps its data and certificate, and where received files go until the person picks a folder.
pub struct AppPaths {
    pub data_dir: PathBuf,
    pub certificate_dir: PathBuf,
    pub default_downloads: Option<PathBuf>,
}

impl AppPaths {
    // The same folders the `ladex` command uses, so the app and the daemon share one node.
    #[cfg(desktop)]
    pub fn resolve(app: &AppHandle) -> tauri::Result<Self> {
        Ok(Self {
            data_dir: ladex_core::node::default_data_dir(),
            certificate_dir: ladex_core::tls::config_dir(),
            default_downloads: app.path().download_dir().ok(),
        })
    }

    // A phone app may only write inside its own storage until the person grants it a folder.
    #[cfg(mobile)]
    pub fn resolve(app: &AppHandle) -> tauri::Result<Self> {
        let app_data = app.path().app_data_dir()?;
        Ok(Self {
            data_dir: app_data.join("files"),
            certificate_dir: app_data.clone(),
            default_downloads: Some(app_data.join("Downloads")),
        })
    }
}
