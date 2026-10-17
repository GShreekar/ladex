//! How commands show their results to a person: sizes, tables, a progress line and QR codes.

use std::io::{IsTerminal, Write};

pub use ladex_core::doctor::format_bytes;

pub fn print_qr_code(url: &str) {
    use qrcode::{render::unicode, QrCode};
    match QrCode::new(url) {
        Ok(code) => {
            let qr = code.render::<unicode::Dense1x2>().quiet_zone(false).build();
            println!("\nScan to open on your phone:\n{qr}");
        }
        Err(e) => eprintln!("Could not draw a QR code for {url}: {e}"),
    }
}

/// Prints rows under their headers, each column as wide as its longest cell.
pub fn print_table(headers: &[&str], rows: &[Vec<String>]) {
    let width_of = |column: usize| {
        let cells = rows.iter().map(|row| row[column].chars().count());
        cells.chain([headers[column].chars().count()]).max().unwrap_or(0)
    };
    let widths: Vec<usize> = (0..headers.len()).map(width_of).collect();
    let print_row = |cells: Vec<&str>| {
        let padded: Vec<String> = cells.iter().zip(&widths).map(|(cell, width)| format!("{cell:<width$}")).collect();
        println!("{}", padded.join("  ").trim_end());
    };
    print_row(headers.to_vec());
    for row in rows {
        print_row(row.iter().map(String::as_str).collect());
    }
}

/// One line on the terminal that is redrawn as work goes on; silent when the output is not a terminal.
pub struct ProgressLine {
    is_shown: bool,
    is_drawn: bool,
}

impl ProgressLine {
    pub fn new(is_wanted: bool) -> Self {
        Self { is_shown: is_wanted && std::io::stderr().is_terminal(), is_drawn: false }
    }

    pub fn draw(&mut self, name: &str, done: u64, total: u64) {
        if !self.is_shown {
            return;
        }
        let percent = done.saturating_mul(100).checked_div(total).unwrap_or(100);
        eprint!("\r\x1b[2K{name}: {percent}% ({} of {})", format_bytes(done), format_bytes(total));
        let _ = std::io::stderr().flush();
        self.is_drawn = true;
    }

    /// Clears the line so ordinary output can follow it.
    pub fn clear(&mut self) {
        if self.is_drawn {
            eprint!("\r\x1b[2K");
            let _ = std::io::stderr().flush();
            self.is_drawn = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_are_shown_in_the_largest_unit_that_fits() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1536), "1.5 KiB");
        assert_eq!(format_bytes(5 * 1024 * 1024 * 1024), "5.0 GiB");
        assert_eq!(format_bytes(u64::MAX), "16777216.0 TiB");
    }
}
