use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use ridgewarden_client::CoreClient;
use ridgewarden_planner::config::{Config, EXAMPLE};
use ridgewarden_planner::terrain::{Terrain, tile_name};
use ridgewarden_planner::{App, share, spawn_loops, web};

#[derive(Parser)]
#[command(
    version,
    about = "Network planner for AREDN meshes (a ridgewarden app)"
)]
struct Cli {
    #[arg(
        long,
        short,
        global = true,
        default_value = "/etc/ridgewarden/planner.toml"
    )]
    config: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Write a starter config
    Init,
    /// Follow the core and serve the planner pages
    Run,
    /// Elevation tiles for predictions
    #[command(subcommand)]
    Terrain(TerrainCmd),
}

#[derive(Subcommand)]
enum TerrainCmd {
    /// Check .hgt tiles (gzipped or not), store them, and share them with other sites
    Import {
        /// files named for their south-west corner, like N35W121.hgt or N35W121.hgt.gz
        #[arg(required = true)]
        files: Vec<PathBuf>,
    },
    /// List the tiles on this site's disk
    List,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ridgewarden_planner=info,ridgewarden_client=info".into()),
        )
        .init();
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Init => {
            if cli.config.exists() {
                bail!(
                    "{} already exists; not overwriting it",
                    cli.config.display()
                );
            }
            if let Some(d) = cli.config.parent() {
                std::fs::create_dir_all(d)?;
            }
            std::fs::write(&cli.config, EXAMPLE)?;
            println!("Wrote {}", cli.config.display());
            Ok(())
        }
        Cmd::Run => {
            let cfg = Config::load(&cli.config)?;
            tokio::runtime::Runtime::new()?.block_on(run(cfg))
        }
        Cmd::Terrain(TerrainCmd::List) => {
            let cfg = Config::load(&cli.config)?;
            let terrain = Terrain::new(&cfg.terrain_dir());
            let tiles = terrain.list()?;
            if tiles.is_empty() {
                println!("No terrain tiles in {}", cfg.terrain_dir().display());
            }
            for (lat, lon) in tiles {
                println!("{}", terrain.path_of(lat, lon).display());
            }
            Ok(())
        }
        Cmd::Terrain(TerrainCmd::Import { files }) => {
            let cfg = Config::load(&cli.config)?;
            tokio::runtime::Runtime::new()?.block_on(import(&cfg, &files))
        }
    }
}

/// Import every file it can, sharing each one; say what failed at the end.
async fn import(cfg: &Config, files: &[PathBuf]) -> Result<()> {
    let terrain = Terrain::new(&cfg.terrain_dir());
    let core = if cfg.terrain.share {
        match CoreClient::new(&cfg.core_url, Some(&cfg.token_file)) {
            Ok(c) => Some(c),
            Err(e) => {
                println!("Tiles won't be shared with other sites: {e:#}");
                None
            }
        }
    } else {
        println!("Tiles won't be shared with other sites: [terrain] share is off.");
        None
    };
    let mut failed = 0;
    for f in files {
        let tile = match terrain.import(f) {
            Ok(t) => t,
            Err(e) => {
                println!("Not imported: {e:#}");
                failed += 1;
                continue;
            }
        };
        let name = tile_name(tile.0, tile.1);
        println!("Imported {name}");
        let Some(core) = &core else { continue };
        match share::offer(core, &cfg.core_data_dir, &terrain, tile).await {
            Ok(_) => println!("Shared {name} with the other sites through the core"),
            Err(e) => {
                println!("{name} was imported here but not shared: {e:#}");
                failed += 1;
            }
        }
    }
    if failed > 0 {
        bail!("{failed} of {} files had a problem", files.len());
    }
    Ok(())
}

async fn run(cfg: Config) -> Result<()> {
    let listen = cfg.listen;
    let app = Arc::new(App::new(cfg)?);
    tracing::info!(
        "ridgewarden-planner {} following {}, pages on http://{listen}",
        env!("CARGO_PKG_VERSION"),
        app.core.base()
    );
    spawn_loops(app.clone());
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("binding {listen}"))?;
    axum::serve(listener, web::router(app))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
