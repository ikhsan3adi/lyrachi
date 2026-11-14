use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use ratatui::DefaultTerminal;

use crate::{cli::Args, ui};

pub mod event;
pub mod resolve;

/// Run the TUI until the user quits.
pub async fn run(terminal: &mut DefaultTerminal, _args: Args) -> color_eyre::Result<()> {
    let mut events = EventStream::new();

    loop {
        terminal.draw(ui::render::scaffold)?;

        let Some(event) = events.next().await else {
            break;
        };
        let event = event?;

        if is_quit(&event) {
            break;
        }
    }

    Ok(())
}

/// M0 placeholder for the MPRIS event dump.
pub async fn run_dump_events(_args: Args) -> color_eyre::Result<()> {
    println!("lyrachi: MPRIS event dump is not wired yet (M3).");
    tokio::signal::ctrl_c().await?;
    Ok(())
}

fn is_quit(event: &Event) -> bool {
    let Event::Key(key) = event else {
        return false;
    };

    if key.kind != KeyEventKind::Press {
        return false;
    }

    key.code == KeyCode::Char('q')
        || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
}
