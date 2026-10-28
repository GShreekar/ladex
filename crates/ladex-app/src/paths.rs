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

    // Until the person chooses a folder, received files stay in the app's own storage, in the folder its FileProvider
    // lends to other apps (`received`, under Android's files directory, which is the node's data folder).
    #[cfg(mobile)]
    pub fn resolve(app: &AppHandle) -> tauri::Result<Self> {
        let app_data = app.path().app_data_dir()?;
        let data_dir = app_data.join("files");
        Ok(Self { default_downloads: Some(data_dir.join("received")), data_dir, certificate_dir: app_data })
    }
}
