use std::path::PathBuf;

use clap::Parser;
use color_eyre::Result;
use lyrachi::{
    app,
    cache::CacheHandle,
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

    if let Some(Command::Config { action }) = args.command {
        return lyrachi::config::run_config_action(
            action,
            &paths.config_file,
            Some(&paths.styles_dir),
        )
        .map_err(|err| color_eyre::eyre::eyre!(err));
    }

    let outcome =
        RuntimeConfig::resolve(&paths.config_file, paths.default_music_dirs.clone(), &args);
    let _guard = logging::init(&paths.log_file)?;
    for warning in &outcome.warnings {
        tracing::warn!("{warning}");
    }

    if let Some(Command::Status { json }) = args.command {
        let cache = open_cache(paths.cache_db_file.clone(), args.no_cache);
        return app::status::run(outcome.config, paths.offsets_file, json, cache).await;
    }

    if let Some(Command::Pipe { format, max_width }) = args.command {
        let cache = open_cache(paths.cache_db_file.clone(), args.no_cache);
        return app::pipe::run(
            outcome.config,
            paths.offsets_file,
            app::pipe::PipeOptions { format, max_width },
            cache,
        )
        .await;
    }

    if args.dump_events {
        return app::run_dump_events(&outcome.config.media, outcome.config.player_filter).await;
    }

    let mut terminal = ratatui::init();
    if std::env::var_os("LYRACHI_TEST_PANIC").is_some() {
        panic!("forced panic for terminal restore test");
    }
    let cache = open_cache(paths.cache_db_file.clone(), args.no_cache);
    let result = app::run(
        &mut terminal,
        outcome.config,
        outcome.warnings,
        paths.offsets_file,
        paths.config_file,
        cache,
    )
    .await;
    ratatui::restore();
    result
}

fn open_cache(path: PathBuf, no_cache: bool) -> Option<CacheHandle> {
    if no_cache {
        return None;
    }
    match CacheHandle::open(path) {
        Ok(cache) => Some(cache),
        Err(error) => {
            tracing::warn!(%error, "lyric cache unavailable");
            None
        }
    }
}
