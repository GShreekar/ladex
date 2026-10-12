use std::path::Path;

/// Print the ticket and, when asked, write it atomically so a script can wait for the file to appear.
pub fn publish(ticket: &str, ticket_file: Option<&Path>) -> anyhow::Result<()> {
    eprintln!("ticket: {ticket}");
    let Some(path) = ticket_file else {
        return Ok(());
    };
    let partial = path.with_extension("partial");
    std::fs::write(&partial, ticket)?;
    std::fs::rename(&partial, path)?;
    Ok(())
}
