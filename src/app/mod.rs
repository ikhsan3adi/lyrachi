use std::io::Write as _;

use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use ratatui::DefaultTerminal;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{cli::Args, media::mpris::arbiter, ui};

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

pub async fn run_dump_events(args: Args) -> color_eyre::Result<()> {
    let (tx, mut rx) = mpsc::channel(256);
    let cancel = CancellationToken::new();
    let monitor_cancel = cancel.clone();
    let player_filter = args.player.clone();
    let conn = zbus::Connection::session().await?;
    let mut monitor =
        tokio::spawn(async move { arbiter::run(player_filter, conn, tx, monitor_cancel).await });
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);

    loop {
        tokio::select! {
            _ = &mut ctrl_c => {
                cancel.cancel();
                break;
            }
            event = rx.recv() => {
                match event {
                    Some(event) => {
                        println!("{event:?}");
                        let _ = std::io::stdout().flush();
                    }
                    None => break,
                }
            }
            result = &mut monitor => {
                match result {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => eprintln!("lyrachi: MPRIS monitor failed: {error:?}"),
                    Err(error) => eprintln!("lyrachi: MPRIS monitor task failed: {error}"),
                }
                break;
            }
        }
    }

    cancel.cancel();
    monitor.abort();
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
