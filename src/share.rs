//! Terrain tiles move between sites through the core's blob store: the site that imported
//! a tile adds it as a blob and logs a `planner.terrain` event naming it; every other
//! planner that needs the tile asks its core for the blob, and copies it out of the
//! core's export folder once it has arrived. Tiles are large, so they go at low priority
//! and only to sites whose mesh they cover.

use std::collections::{BTreeMap, BTreeSet};

use std::path::Path;

use anyhow::{Context, Result, bail};
use ridgewarden_client::core::Projection;
use ridgewarden_client::mesh::s;
use ridgewarden_client::{CoreClient, Event};
use rusqlite::{Connection, Transaction, params};
use serde_json::{Value, json};

use crate::App;
use crate::net::{Net, Pos};
use crate::terrain::{Terrain, parse_name, tile_name, tiles_around};

pub const TILE: &str = "planner.terrain";
/// Blobs fetch lowest number first; most files are 5, so tiles wait behind them.
const PRIORITY: u8 = 8;
/// A 1 arc-second tile, the biggest there is.
const MAX_TILE_BYTES: u64 = 25_934_402;

pub struct Shared;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS shared_tiles (
    name TEXT PRIMARY KEY,
    root TEXT NOT NULL,
    size INTEGER NOT NULL,
    call TEXT NOT NULL,
    ts   INTEGER NOT NULL
);";

fn valid_root(r: &str) -> bool {
    r.len() == 64 && r.bytes().all(|b| b.is_ascii_hexdigit())
}

impl Projection for Shared {
    fn schema(&self) -> &'static str {
        SCHEMA
    }

    fn reset(&self, tx: &Transaction) -> Result<()> {
        tx.execute("DELETE FROM shared_tiles", [])?;
        Ok(())
    }

    fn apply(&self, tx: &Transaction, e: &Event, ts: i64) -> Result<()> {
        if e.kind != TILE {
            return Ok(());
        }
        let p = &e.payload;
        let (Some((lat, lon)), Some(root)) = (parse_name(s(p, "name").unwrap_or("")), s(p, "root"))
        else {
            return Ok(());
        };
        if !valid_root(root) {
            return Ok(());
        }
        let size = p.get("size").and_then(Value::as_i64).unwrap_or(0);
        // the newest announcement of a tile wins, e.g. a void-filled replacement
        tx.execute(
            "INSERT INTO shared_tiles (name, root, size, call, ts) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(name) DO UPDATE SET root = excluded.root, size = excluded.size,
                 call = excluded.call, ts = excluded.ts WHERE excluded.ts >= shared_tiles.ts",
            params![tile_name(lat, lon), root, size, e.author.call, ts],
        )?;
        Ok(())
    }
}

/// A tile some site shared.
pub struct SharedTile {
    pub root: String,
    pub size: i64,
    /// the station that shared it
    pub call: String,
}

pub fn shared(conn: &Connection) -> Result<BTreeMap<(i32, i32), SharedTile>> {
    let mut st = conn.prepare("SELECT name, root, size, call FROM shared_tiles")?;
    let mut out = BTreeMap::new();
    for r in st.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, String>(3)?,
        ))
    })? {
        let (name, root, size, call) = r?;
        if let Some(c) = parse_name(&name) {
            out.insert(c, SharedTile { root, size, call });
        }
    }
    Ok(out)
}

/// Give a tile on this site's disk to every other site.
pub async fn offer(
    core: &CoreClient,
    core_data_dir: &Path,
    terrain: &Terrain,
    (lat, lon): (i32, i32),
) -> Result<String> {
    let name = tile_name(lat, lon);
    let src = terrain.path_of(lat, lon);
    let file = format!("planner-{name}.hgt");
    let imports = core_data_dir.join("imports");
    let dst = imports.join(&file);
    std::fs::create_dir_all(&imports)
        .with_context(|| format!("making the core's import folder {}", imports.display()))?;
    std::fs::copy(&src, &dst).with_context(|| {
        format!(
            "copying {} into the core's import folder {}",
            src.display(),
            imports.display()
        )
    })?;
    let added = core
        .blob_add(
            &file,
            &format!("ridgewarden-planner/terrain/{name}.hgt"),
            PRIORITY,
        )
        .await;
    // the core keeps its own copy in its chunk store, or it failed
    let _ = std::fs::remove_file(&dst);
    let root = added?;
    let size = std::fs::metadata(&src)?.len();
    core.append(TILE, json!({"name": name, "root": root, "size": size}))
        .await?;
    Ok(root)
}

/// Tiles this mesh needs: around every located site and candidate.
pub fn needed(app: &App, net: &Net) -> BTreeSet<(i32, i32)> {
    let mut pts: Vec<Pos> = net.pos.values().copied().collect();
    pts.extend(app.cfg.candidates.iter().map(|c| Pos {
        lat: c.lat,
        lon: c.lon,
    }));
    tiles_around(&pts, app.cfg.terrain.margin_km)
}

/// Ask the core for shared tiles this mesh needs and copy in those that have arrived.
/// Returns how many tiles were added. A tile that turns out bad is not tried again until
/// the planner restarts.
pub async fn fetch_round(app: &App, net: &Net) -> Result<usize> {
    let need = needed(app, net);
    let want: Vec<((i32, i32), SharedTile)> = shared(&app.db())?
        .into_iter()
        .filter(|(c, t)| !app.terrain.has(c.0, c.1) && need.contains(c) && !app.is_bad(&t.root))
        .collect();
    let mut added = 0;
    for ((lat, lon), t) in want {
        let name = tile_name(lat, lon);
        if t.size > MAX_TILE_BYTES as i64 {
            tracing::warn!(
                "{} shared {name} as {} bytes, too big for a tile; skipped",
                t.call,
                t.size
            );
            app.mark_bad(&t.root);
            continue;
        }
        let path = match app
            .core
            .blob_export(&t.root, &format!("planner-{name}.hgt"))
            .await
        {
            Ok(p) => p,
            Err(e) => {
                // usually still on its way; asking again is harmless
                tracing::debug!("tile {name} not here yet: {e:#}");
                if let Err(e) = app.core.blob_want(&t.root).await {
                    // the blob's manifest may not have synced to this core yet
                    tracing::debug!("tile {name} not wanted yet: {e:#}");
                }
                continue;
            }
        };
        let stored = take_export(app, &path, (lat, lon));
        let _ = std::fs::remove_file(&path);
        match stored {
            Ok(()) => {
                tracing::info!("terrain tile {name} arrived from another site");
                added += 1;
            }
            Err(e) => {
                tracing::warn!("the copy of {name} {} shared is unusable: {e:#}", t.call);
                app.mark_bad(&t.root);
            }
        }
    }
    Ok(added)
}

fn take_export(app: &App, path: &str, (lat, lon): (i32, i32)) -> Result<()> {
    let len = std::fs::metadata(path)
        .with_context(|| format!("reading {path}"))?
        .len();
    if len > MAX_TILE_BYTES {
        bail!("it is {len} bytes, more than any tile");
    }
    let bytes = std::fs::read(path).with_context(|| format!("reading {path}"))?;
    app.terrain.store(lat, lon, &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::tests::NOW;
    use ridgewarden_client::mesh::testing::Fake;

    #[test]
    fn announcements_are_checked_and_the_newest_wins() {
        let mut l = Fake::new(vec![Box::new(Shared)]);
        let root = |c: char| c.to_string().repeat(64);
        l.add(
            TILE,
            json!({"name": "N35W121", "root": root('a'), "size": 10}),
            NOW - 10,
        );
        l.add(
            TILE,
            json!({"name": "N35W121", "root": root('b'), "size": 20}),
            NOW,
        );
        l.add(
            TILE,
            json!({"name": "N35W121", "root": root('c'), "size": 30}),
            NOW - 20,
        );
        l.add(TILE, json!({"name": "somewhere", "root": root('d')}), NOW);
        l.add(TILE, json!({"name": "N36W121", "root": "not hex"}), NOW);
        let s = shared(&l.conn).unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!(s[&(35, -121)].root, root('b'));
        assert_eq!(s[&(35, -121)].size, 20);
    }
}
