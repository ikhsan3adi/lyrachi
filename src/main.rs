use clap::Parser;
use color_eyre::Result;
use lyrachi::{app, cli::Args, logging};

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    let args = Args::parse();
    let _guard = logging::init(&args)?;

    if args.dump_events {
        return app::run_dump_events(args).await;
    }

    let mut terminal = ratatui::init();
    if std::env::var_os("LYRACHI_TEST_PANIC").is_some() {
        panic!("forced panic for terminal restore test");
    }
    let result = app::run(&mut terminal, args).await;
    ratatui::restore();
    result
}
