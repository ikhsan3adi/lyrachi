use clap::Parser;
use color_eyre::Result;
use lyrachi::{
    app,
    cli::{Args, Command},
    config::RuntimeConfig,
    logging,
    paths::{AppPaths, PathOverrides},
};

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    let args = Args::parse();
    let paths = AppPaths::resolve(PathOverrides {
        config_file: args.config.clone(),
        log_file: args.log_file.clone(),
    });

    if matches!(args.command, Some(Command::Paths)) {
        for (name, value) in paths.lines() {
            println!("{name}: {value}");
        }
        return Ok(());
    }

    let outcome =
        RuntimeConfig::resolve(&paths.config_file, paths.default_music_dirs.clone(), &args);
    let _guard = logging::init(&paths.log_file)?;
    for warning in &outcome.warnings {
        tracing::warn!("{warning}");
    }

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
