//! `ladex receive`: this command as a device in the mesh, which saves the files sent to it as the app's window does.

use std::io::{BufRead, IsTerminal};
use std::path::{Path, PathBuf};

use anyhow::bail;
use ladex_core::control::{Event, Offer, Reply, Request};
use ladex_core::types::SessionId;
use tokio::sync::mpsc;

use crate::commands::{report, Session, EXIT_OK};
use crate::output::format_bytes;

enum News {
    Joined(SessionId),
    Offered(Offer),
}

/// Receives as a device called `name` until the node stops or the person presses Ctrl-C.
pub async fn run(session: &Session, name: Option<String>, into: PathBuf, accepts_everything: bool) -> anyhow::Result<i32> {
    let name = match name {
        Some(name) => name,
        None => node_name(session).await?,
    };
    let (tell, mut news) = mpsc::unbounded_channel();
    let request = Request::Receive { name: name.clone() };
    let receiving = session.stream(&request, move |event| {
        let _ = match event {
            Event::Joined { device } => tell.send(News::Joined(device.clone())),
            Event::Incoming { offer } => tell.send(News::Offered(offer.clone())),
            _ => Ok(()),
        };
    });
    tokio::pin!(receiving);
    let mut device = None;
    loop {
        tokio::select! {
            ended = &mut receiving => {
                ended?;
                return Ok(EXIT_OK);
            }
            Some(news) = news.recv() => match news {
                News::Joined(id) => {
                    announce(&name, &into, session.is_json());
                    device = Some(id);
                }
                News::Offered(offer) => {
                    if let Some(device) = &device {
                        answer(session, device, &offer, &into, accepts_everything).await;
                    }
                }
            },
        }
    }
}

async fn node_name(session: &Session) -> anyhow::Result<String> {
    match session.call(&Request::Status).await? {
        Reply::Status(status) => Ok(status.node_name),
        _ => bail!("the node answered something else"),
    }
}

fn announce(name: &str, into: &Path, is_json: bool) {
    if is_json {
        return;
    }
    eprintln!("Receiving as {name:?}: other devices can now send files here, saved into {}.", into.display());
    eprintln!("Files your incoming files rule allows are saved at once; you are asked about the rest. Ctrl-C stops.");
}

async fn answer(session: &Session, device: &SessionId, offer: &Offer, into: &Path, accepts_everything: bool) {
    let is_json = session.is_json();
    let from = if offer.from_name.is_empty() { "A device" } else { offer.from_name.as_str() };
    if !is_json {
        eprintln!("{from} sends {} ({})", offer.name, format_bytes(offer.size));
    }
    let is_accepted = offer.is_accepted_without_asking || accepts_everything || is_accepted_when_asked(is_json).await;
    let request = match is_accepted {
        true => Request::AcceptOffer { device: device.clone(), file: offer.file_id.clone(), into: into.to_path_buf() },
        false => Request::DeclineOffer { device: device.clone(), file: offer.file_id.clone(), from: offer.from.clone() },
    };
    match session.call(&request).await {
        Ok(Reply::Acknowledged { .. }) if !is_json => eprintln!("Declined {}", offer.name),
        Ok(reply) => {
            report(&reply, is_json);
        }
        Err(e) => eprintln!("Could not receive {}: {e:#}", offer.name),
    }
}

// Without a terminal to answer at, an offer the rule does not cover is declined.
async fn is_accepted_when_asked(is_json: bool) -> bool {
    if is_json || !std::io::stdin().is_terminal() {
        eprintln!("Declining: nobody is at a terminal to ask. Use --yes, or `ladex settings --auto-accept mesh`, to accept files unasked.");
        return false;
    }
    let answer = tokio::task::spawn_blocking(|| {
        eprint!("Save it? [y/N] ");
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line).map(|_| line)
    })
    .await;
    matches!(answer, Ok(Ok(line)) if matches!(line.trim().to_lowercase().as_str(), "y" | "yes"))
}
