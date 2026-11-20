//! Sites that carry a replicated service, from app 02 (ridgewarden-failover): each of its
//! sites sends a `failover.site` heartbeat, and every one of them holds the whole chat.
//! A what-if then says how many copies stay reachable.

use std::collections::BTreeSet;

use anyhow::Result;
use ridgewarden_client::Event;
use ridgewarden_client::core::Projection;
use ridgewarden_client::mesh::s;
use ridgewarden_client::util::{norm_node, valid_node};
use rusqlite::{Connection, Transaction, params};

pub const SITE: &str = "failover.site";
/// A site still counts as carrying the chat this long after its last heartbeat, so a
/// site that is down now is still listed as a copy that went missing.
const MEMORY_MS: i64 = 24 * 3_600_000;

pub struct Replicas;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS failover_sites (
    node TEXT PRIMARY KEY,
    ts   INTEGER NOT NULL
);";

impl Projection for Replicas {
    fn schema(&self) -> &'static str {
        SCHEMA
    }

    fn reset(&self, tx: &Transaction) -> Result<()> {
        tx.execute("DELETE FROM failover_sites", [])?;
        Ok(())
    }

    fn apply(&self, tx: &Transaction, e: &Event, ts: i64) -> Result<()> {
        if e.kind != SITE {
            return Ok(());
        }
        let node = norm_node(s(&e.payload, "node").unwrap_or(""));
        if !valid_node(&node) {
            return Ok(());
        }
        // heartbeats held up by a split arrive late; only the newest counts
        tx.execute(
            "INSERT INTO failover_sites (node, ts) VALUES (?1, ?2)
             ON CONFLICT(node) DO UPDATE SET ts = max(ts, excluded.ts)",
            params![node, ts],
        )?;
        Ok(())
    }
}

/// Nodes of the sites that ran the replicated chat in the last day.
pub fn sites(conn: &Connection, now: i64) -> Result<BTreeSet<String>> {
    let mut st = conn.prepare("SELECT node FROM failover_sites WHERE ts > ?1")?;
    let rows = st.query_map(params![now - MEMORY_MS], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::tests::{MIN, NOW};
    use ridgewarden_client::mesh::testing::Fake;
    use serde_json::json;

    #[test]
    fn heartbeats_name_the_sites_and_old_ones_drop_off() {
        let mut l = Fake::new(vec![Box::new(Replicas)]);
        l.add(
            SITE,
            json!({"node": "TST-Shack", "url": "http://x"}),
            NOW - 5 * MIN,
        );
        l.add(SITE, json!({"node": "not a node"}), NOW - 5 * MIN);
        assert_eq!(
            sites(&l.conn, NOW).unwrap(),
            ["tst-shack".to_string()].into()
        );
        assert!(sites(&l.conn, NOW + 2 * MEMORY_MS).unwrap().is_empty());
    }
}
