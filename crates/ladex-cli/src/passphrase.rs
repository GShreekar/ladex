//! `ladex passphrase`: the mesh passphrase, shown or set as the app's wizard and Settings page do.
//! With a node running it asks that node; without one it reads and writes the passphrase saved in the data folder.

use std::io::{BufRead, IsTerminal};
use std::path::Path;

use anyhow::{bail, Context};
use clap::Subcommand;
use ladex_core::control::{Reply, Request};
use ladex_core::preferences::{PreferencesStore, Update};
use ladex_core::{auth, saved_passphrase};

use crate::commands::{is_connection_refused, report, Session};

#[derive(Subcommand)]
pub enum Action {
    /// Save a new passphrase; a running node starts using it once it restarts
    Set {
        /// The passphrase. Default: read from standard input
        passphrase: Option<String>,
        /// Make up a passphrase and print it.
        #[arg(long, conflicts_with = "passphrase")]
        generate: bool,
    },
}

pub async fn run(action: Option<Action>, data_dir: &Path, use_keychain: bool, is_json: bool) -> anyhow::Result<i32> {
    let session = Session::open(data_dir, is_json).ok();
    match action {
        None => show(session.as_ref(), data_dir, use_keychain, is_json).await,
        Some(Action::Set { passphrase, generate }) => {
            let passphrase = if generate { auth::generate_passphrase() } else { given_or_read(passphrase)? };
            set(session.as_ref(), data_dir, use_keychain, &passphrase, generate, is_json).await
        }
    }
}

pub fn print(passphrase: Option<&str>) {
    match passphrase {
        Some(passphrase) => println!("Mesh passphrase: {passphrase}"),
        None => println!("No passphrase is set, so any device on the network can join. Set one with: ladex passphrase set --generate"),
    }
}

async fn show(session: Option<&Session>, data_dir: &Path, use_keychain: bool, is_json: bool) -> anyhow::Result<i32> {
    if let Some(reply) = ask_running_node(session, &Request::Passphrase).await? {
        return Ok(report(&reply, is_json));
    }
    let saved = saved_passphrase::load(data_dir, use_keychain)?;
    Ok(report(&Reply::Passphrase { passphrase: saved }, is_json))
}

async fn set(
    session: Option<&Session>,
    data_dir: &Path,
    use_keychain: bool,
    passphrase: &str,
    is_generated: bool,
    is_json: bool,
) -> anyhow::Result<i32> {
    if passphrase.is_empty() {
        bail!("the passphrase must not be empty");
    }
    if let Some(reason) = auth::weakness(passphrase) {
        eprintln!("Warning: this passphrase is weak ({reason}). Prefer `ladex passphrase set --generate`.");
    }
    if is_generated && !is_json {
        println!("New passphrase: {passphrase}");
    }
    let request = Request::ChangePassphrase { passphrase: passphrase.to_string() };
    if let Some(reply) = ask_running_node(session, &request).await? {
        return Ok(report(&reply, is_json));
    }
    saved_passphrase::save(data_dir, use_keychain, passphrase)?;
    // The app would otherwise open its first-run wizard here and offer a different passphrase.
    let setup_done = Update { is_setup_done: Some(true), ..Default::default() };
    PreferencesStore::open(data_dir).update(&setup_done, 0).map_err(|e| anyhow::anyhow!("{e}"))?;
    let message = format!("Saved for {}; `ladex` and the app use it from their next start", data_dir.display());
    Ok(report(&Reply::Acknowledged { message }, is_json))
}

// None when no node is running, so the caller works on the data folder instead.
async fn ask_running_node(session: Option<&Session>, request: &Request) -> anyhow::Result<Option<Reply>> {
    let Some(session) = session else { return Ok(None) };
    match session.call(request).await {
        Ok(reply) => Ok(Some(reply)),
        Err(e) if is_connection_refused(&e) => Ok(None),
        Err(e) => Err(e),
    }
}

// Read from standard input, so the passphrase need not appear in the shell's history or the process list.
fn given_or_read(given: Option<String>) -> anyhow::Result<String> {
    if let Some(passphrase) = given {
        return Ok(passphrase);
    }
    if std::io::stdin().is_terminal() {
        eprint!("New passphrase: ");
    }
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).context("could not read the passphrase")?;
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}
