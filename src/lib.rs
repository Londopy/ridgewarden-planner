//! ridgewarden-planner: the live AREDN mesh as a graph, what breaks if any site or link
//! dies, terrain predictions calibrated against the links the mesh measures, which
//! candidate or hilltop would add the most redundancy, and GeoJSON and PDF exports.
//!
//! Runs next to a ridgewarden core and builds its view from the core's event feed: the
//! polls of every site with a core, and of the nodes the core crawls. It uses Link
//! Doctor's link analysis, so with the same `fresh_s` the two agree on what is up.

pub mod config;
pub mod geo;
pub mod hills;
pub mod impact;
pub mod inflate;
pub mod itm;
pub mod map;
pub mod net;
pub mod pdf;
pub mod predict;
pub mod publish;
pub mod rank;
pub mod record;
pub mod replicas;
pub mod report;
pub mod share;
pub mod terrain;
pub mod web;

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::Result;
use ridgewarden_client::core::Projection;
use ridgewarden_client::mesh::Mesh;
use ridgewarden_client::util::now_ms;
use ridgewarden_client::{CoreClient, open_db};
use ridgewarden_linkdoctor::history::{self, Notes};
use rusqlite::Connection;

use crate::config::Config;
use crate::net::Net;
use crate::predict::Calibration;
use crate::record::Measured;
use crate::terrain::Terrain;

/// Link Doctor's notes projection also creates the link history tables its analysis reads.
pub const PROJECTIONS: &[&dyn Projection] = &[&Mesh, &Notes, &replicas::Replicas, &share::Shared];

const VIEW_TTL: Duration = Duration::from_secs(30);

/// Everything the pages show, built together so they agree.
pub struct View {
    pub net: Net,
    pub measured: Vec<Measured>,
    pub cal: Calibration,
}

pub struct App {
    pub cfg: Config,
    pub core: CoreClient,
    pub db: Mutex<Connection>,
    pub terrain: Terrain,
    /// whether the core token was readable, so tiles and predictions can be published;
    /// without it the planner is read-only
    pub can_write: bool,
    /// shared tiles that turned out unusable, by blob root
    bad_tiles: Mutex<BTreeSet<String>>,
    /// the last analysis, reused for a short while so clicking around the map stays quick
    view: Mutex<Option<(Instant, Arc<View>)>>,
    here: Mutex<Option<String>>,
}

impl App {
    pub fn new(cfg: Config) -> Result<Self> {
        let (core, can_write) = match CoreClient::new(&cfg.core_url, Some(&cfg.token_file)) {
            Ok(c) => (c, true),
            Err(e) => {
                tracing::warn!(
                    "read-only: terrain tiles and predictions won't be shared with other sites: {e:#}"
                );
                (CoreClient::new(&cfg.core_url, None)?, false)
            }
        };
        let db = open_db(&cfg.db_path(), PROJECTIONS)?;
        db.execute_batch(publish::SCHEMA)?;
        Ok(Self {
            terrain: Terrain::new(&cfg.terrain_dir()),
            cfg,
            core,
            can_write,
            bad_tiles: Mutex::new(BTreeSet::new()),
            db: Mutex::new(db),
            view: Mutex::new(None),
            here: Mutex::new(None),
        })
    }

    pub fn db(&self) -> MutexGuard<'_, Connection> {
        self.db.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// This site's node name, or "" before the core has polled it.
    pub async fn here(&self) -> String {
        if let Some(h) = self.here.lock().unwrap().clone() {
            return h;
        }
        let node = self.core.whoami().await.map(|w| w.node).unwrap_or_default();
        if !node.is_empty() {
            *self.here.lock().unwrap() = Some(node.clone());
        }
        node
    }

    /// The analysis of the whole mesh, at most `VIEW_TTL` old.
    pub fn view(&self, here: &str) -> Result<Arc<View>> {
        let mut cache = self.view.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, v)) = cache.as_ref()
            && at.elapsed() < VIEW_TTL
            && v.net.here == here
        {
            return Ok(v.clone());
        }
        let now = now_ms();
        let (net, measured) = {
            let db = self.db();
            let net = Net::build(&db, now, self.cfg.fresh_ms(), here, &self.cfg.host_sites)?;
            let measured = record::measured(&db, now, &net.pos, self.cfg.radio.band_mhz)?;
            (net, measured)
        };
        let cal = predict::calibrate(&net, &measured, &self.terrain, &self.cfg);
        let v = Arc::new(View { net, measured, cal });
        *cache = Some((Instant::now(), v.clone()));
        Ok(v)
    }

    pub fn is_bad(&self, root: &str) -> bool {
        self.bad_tiles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(root)
    }

    pub fn mark_bad(&self, root: &str) {
        self.bad_tiles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(root.to_string());
    }

    /// Drop the cached analysis, after new terrain arrived.
    pub fn forget_view(&self) {
        *self.view.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// `App::view` off the async threads: building it scans the database and predicts every
/// measured link.
pub async fn view(app: &Arc<App>, here: &str) -> Result<Arc<View>> {
    let (a, h) = (app.clone(), here.to_string());
    tokio::task::spawn_blocking(move || a.view(&h)).await?
}

/// Follow the core's feed, keep the link history rolled up and pruned, and pick up
/// terrain other sites shared, forever.
pub fn spawn_loops(app: Arc<App>) {
    let a = app.clone();
    tokio::spawn(async move {
        loop {
            if let Err(e) = ridgewarden_client::follow(&a.core, &a.db, PROJECTIONS).await {
                tracing::warn!("following the core: {e:#}");
            }
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
    });
    let a = app.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(120)).await;
        loop {
            if let Err(e) = history::rollup(&a.db(), now_ms()) {
                tracing::warn!("rolling up link history: {e:#}");
            }
            tokio::time::sleep(Duration::from_secs(3600)).await;
        }
    });
    if !app.can_write {
        return;
    }
    let a = app.clone();
    tokio::spawn(async move {
        // let the feed catch up first, so predictions start from the full history
        tokio::time::sleep(Duration::from_secs(180)).await;
        loop {
            match publish_round(&a).await {
                Ok(0) => {}
                Ok(n) => tracing::info!("{n} link predictions published"),
                Err(e) => tracing::warn!("publishing predictions: {e:#}"),
            }
            tokio::time::sleep(Duration::from_secs(900)).await;
        }
    });
    if !app.cfg.terrain.fetch_shared {
        return;
    }
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(60)).await;
        loop {
            if let Err(e) = terrain_round(&app).await {
                tracing::warn!("fetching shared terrain: {e:#}");
            }
            tokio::time::sleep(Duration::from_secs(600)).await;
        }
    });
}

async fn terrain_round(app: &Arc<App>) -> Result<()> {
    let here = app.here().await;
    let v = view(app, &here).await?;
    if share::fetch_round(app, &v.net).await? > 0 {
        app.forget_view();
    }
    Ok(())
}

async fn publish_round(app: &Arc<App>) -> Result<usize> {
    let here = app.here().await;
    let v = view(app, &here).await?;
    let due = publish::due(&app.db(), &v.cal, &app.cfg, now_ms())?;
    for d in &due {
        app.core.append(publish::KIND, publish::payload(d)).await?;
        publish::record(&app.db(), d, now_ms())?;
    }
    Ok(due.len())
}
