//! `ladex pair`: pairs this node with another as the app's Network page does, by both people comparing six words.

use std::collections::HashSet;
use std::io::{BufRead, IsTerminal};
use std::time::Duration;

use anyhow::bail;
use ladex_core::control::{Reply, Request};
use ladex_core::pairing::{Outcome, View, WaitingCode};

use crate::commands::{Session, EXIT_FAILED, EXIT_OK};

const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Pairs one device: dials `address` if given, otherwise waits for the other device to dial this one.
pub async fn run(session: &Session, address: Option<String>) -> anyhow::Result<i32> {
    let start = pairing_view(session, &Request::OpenPairing).await?;
    if let Some(own) = &start.address {
        eprintln!("This device's address is {own}; on the other device run: ladex pair {own}");
    }
    if let Some(address) = &address {
        session.call(&Request::DialPairing { address: address.clone() }).await?;
        eprintln!("Dialing {address}…");
    } else {
        eprintln!("Waiting for the other device to dial this one (Ctrl-C stops).");
    }
    let outcome = tokio::select! {
        outcome = follow(session, &start, address.is_some()) => outcome,
        _ = tokio::signal::ctrl_c() => Ok(EXIT_FAILED),
    };
    // Nobody is left to answer codes once this command ends, so the window closes with it.
    let _ = session.call(&Request::ClosePairing).await;
    outcome
}

async fn pairing_view(session: &Session, request: &Request) -> anyhow::Result<View> {
    match session.call(request).await? {
        Reply::Pairing(view) => Ok(*view),
        _ => bail!("the node answered something else"),
    }
}

// Answers each code as it shows up, and ends with the first pairing that finishes, one way or the other.
async fn follow(session: &Session, start: &View, is_dialing: bool) -> anyhow::Result<i32> {
    let mut answered: HashSet<u64> = HashSet::new();
    let mut newest = start.status.recent.first().cloned();
    loop {
        tokio::time::sleep(POLL_INTERVAL).await;
        let view = pairing_view(session, &Request::Pairing).await?;
        for code in &view.status.waiting {
            if answered.insert(code.id) {
                let accepted = confirm(code).await;
                if let Err(e) = session.call(&Request::AnswerPairing { id: code.id, accepted }).await {
                    eprintln!("Could not answer: {e:#}");
                }
            }
        }
        let finished: Vec<&Outcome> = view.status.recent.iter().take_while(|outcome| Some(*outcome) != newest.as_ref()).collect();
        if let Some(outcome) = finished.last() {
            return Ok(report(outcome));
        }
        newest = view.status.recent.first().cloned();
        if !is_dialing && !view.status.open {
            eprintln!("Stopped waiting: nobody dialed this device in time.");
            return Ok(EXIT_FAILED);
        }
    }
}

fn report(outcome: &Outcome) -> i32 {
    if outcome.paired {
        println!("Paired with {}", outcome.name);
        return EXIT_OK;
    }
    eprintln!("Not paired with {}: {}", outcome.name, outcome.message);
    EXIT_FAILED
}

// Without a terminal to ask at, a code is refused: pairing is only as safe as the person comparing the words.
async fn confirm(code: &WaitingCode) -> bool {
    let request = &code.request;
    let who = if request.dialed { format!("Pairing with {}", request.name) } else { format!("{} wants to pair", request.name) };
    eprintln!("{who}. Check that the other device shows these same words:\n\n    {}\n", request.words.join(" "));
    if !std::io::stdin().is_terminal() {
        eprintln!("Refusing: nobody is at a terminal to compare the words.");
        return false;
    }
    let answer = tokio::task::spawn_blocking(|| {
        eprint!("Do they match? [y/N] ");
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line).map(|_| line)
    })
    .await;
    matches!(answer, Ok(Ok(line)) if matches!(line.trim().to_lowercase().as_str(), "y" | "yes"))
}
