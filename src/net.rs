//! The live network as the planner sees it: Link Doctor's link analysis and single points
//! of failure, plus where each site is and which site each advertised service lives on.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use ridgewarden_client::aredn::Service;
use ridgewarden_client::util::{median, norm_node, valid_node};
use ridgewarden_linkdoctor::config::LinksCfg;
use ridgewarden_linkdoctor::history::DAY;
use ridgewarden_linkdoctor::links::{self, PairView, Topology};
use rusqlite::{Connection, params};
use serde_json::Value;

use crate::config::valid_lat_lon;

/// Services are remembered this long after a site last advertised them, so the services
/// of a site that is already cut off still show up in reports.
const SERVICE_MEMORY: i64 = 7 * DAY;
/// Caps on what one list from another station can put on a page.
const MAX_SERVICES: usize = 2000;
const MAX_TEXT: usize = 200;
/// A site further than this from the middle of the mesh has a wrong position.
const MAX_SPREAD_KM: f64 = 500.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pos {
    pub lat: f64,
    pub lon: f64,
}

/// An advertised service and the site it runs at, when that can be told.
#[derive(Clone, Debug, PartialEq)]
pub struct Placed {
    pub svc: Service,
    pub site: Option<String>,
}

pub struct Net {
    pub now: i64,
    /// this site's node, or "" before the core has polled it
    pub here: String,
    pub pairs: Vec<PairView>,
    pub sites: BTreeSet<String>,
    /// sites whose own links we have fresh data for
    pub observed: BTreeSet<String>,
    pub topo: Topology,
    pub pos: BTreeMap<String, Pos>,
    pub services: Vec<Placed>,
    /// sites carrying the replicated chat (app 02)
    pub replicas: BTreeSet<String>,
}

impl Net {
    pub fn build(
        conn: &Connection,
        now: i64,
        fresh_ms: i64,
        here: &str,
        host_sites: &BTreeMap<String, String>,
    ) -> Result<Self> {
        let pairs = links::analyze(conn, now, fresh_ms, &LinksCfg::default())?;
        let sites = links::known_sites(conn, now)?;
        let observed = conn
            .prepare("SELECT DISTINCT node FROM polls WHERE ok = 1 AND ts >= ?1")?
            .query_map([now - fresh_ms], |r| r.get::<_, String>(0))?
            .collect::<Result<_, _>>()?;
        let anchor = Some(here).filter(|h| sites.contains(*h));
        let topo = links::topology(&pairs, &sites, anchor);
        let pos = positions(conn, now)?;
        let services = services(conn, now)?
            .into_iter()
            .map(|svc| Placed {
                site: place(&svc, &sites, host_sites),
                svc,
            })
            .collect();
        let replicas = crate::replicas::sites(conn, now)?
            .intersection(&sites)
            .cloned()
            .collect();
        Ok(Self {
            now,
            here: here.to_string(),
            pairs,
            sites,
            observed,
            topo,
            pos,
            services,
            replicas,
        })
    }

    /// The site reports are judged from: here, if the graph knows it.
    pub fn anchor(&self) -> Option<&str> {
        self.topo.anchor.as_deref()
    }
}

/// Each node's latest reported position in the last week. Polls come from other
/// stations, so anything outside the usual ranges is ignored, and so is a position far
/// from all the others, which is usually a sign slip that would stretch every map.
fn positions(conn: &Connection, now: i64) -> Result<BTreeMap<String, Pos>> {
    let mut st = conn.prepare(
        "SELECT node, lat, lon FROM polls
         WHERE ok = 1 AND ts > ?1 AND lat IS NOT NULL AND lon IS NOT NULL ORDER BY ts",
    )?;
    let mut out = BTreeMap::new();
    for r in st.query_map(params![now - SERVICE_MEMORY], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, f64>(1)?,
            r.get::<_, f64>(2)?,
        ))
    })? {
        let (node, lat, lon) = r?;
        if valid_lat_lon(lat, lon) {
            out.insert(node, Pos { lat, lon });
        }
    }
    let mut lats: Vec<f64> = out.values().map(|p| p.lat).collect();
    let mut lons: Vec<f64> = out.values().map(|p| p.lon).collect();
    if let (Some(lat), Some(lon)) = (median(&mut lats), median(&mut lons)) {
        let centre = Pos { lat, lon };
        out.retain(|_, p| crate::geo::distance_km(centre, *p) <= MAX_SPREAD_KM);
    }
    Ok(out)
}

fn short(s: &str) -> String {
    s.chars().take(MAX_TEXT).collect()
}

/// Every service advertised in the last week, once per link.
fn services(conn: &Connection, now: i64) -> Result<Vec<Service>> {
    let mut st = conn.prepare(
        "SELECT DISTINCT l.json FROM polls p JOIN lists l ON l.hash = p.ads_hash WHERE p.ok = 1 AND p.ts > ?1",
    )?;
    let mut out: BTreeMap<String, Service> = BTreeMap::new();
    for j in st.query_map(params![now - SERVICE_MEMORY], |r| r.get::<_, String>(0))? {
        let items: Vec<Value> = serde_json::from_str(&j?).unwrap_or_default();
        for v in items {
            if out.len() >= MAX_SERVICES {
                break;
            }
            // one bad entry from another station shouldn't hide the rest of its list
            let Ok(s) = serde_json::from_value::<Service>(v) else {
                continue;
            };
            let s = Service {
                host: short(&s.host),
                name: short(&s.name),
                link: short(&s.link),
                protocol: short(&s.protocol),
            };
            if !s.name.trim().is_empty() {
                out.entry(s.link.clone()).or_insert(s);
            }
        }
    }
    let mut v: Vec<Service> = out.into_values().collect();
    v.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.link.cmp(&b.link)));
    Ok(v)
}

/// The host name in a service link: "http://tst-pi.local.mesh:8080/x" -> "tst-pi".
fn link_host(link: &str) -> Option<String> {
    let rest = link.split_once("://").map_or(link, |(_, r)| r);
    let host = rest.split(['/', ':', '?', '#']).next()?;
    let n = norm_node(host);
    valid_node(&n).then_some(n)
}

/// Which site a service runs at: its host if that is a site, a configured host mapping,
/// or the host in its link. None when none of those is a known site.
pub fn place(
    svc: &Service,
    sites: &BTreeSet<String>,
    host_sites: &BTreeMap<String, String>,
) -> Option<String> {
    let host = Some(norm_node(&svc.host)).filter(|h| valid_node(h));
    [host, link_host(&svc.link)]
        .into_iter()
        .flatten()
        .find_map(|h| {
            let site = host_sites.get(&h).cloned().unwrap_or(h);
            sites.contains(&site).then_some(site)
        })
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use ridgewarden_client::event::list_hash;
    use ridgewarden_client::mesh::Mesh;
    use ridgewarden_client::mesh::testing::Fake;
    use ridgewarden_linkdoctor::history::Notes;
    use serde_json::json;

    pub const MIN: i64 = 60_000;
    pub const NOW: i64 = 1_800_000_000_000;

    /// The simulator's demo mesh, all links up, polled from every site, seen from
    /// tst-shack. Lopez hangs off cuesta, shack off madonna, and paso off north, which
    /// hangs off blackmtn; cuesta, madonna and blackmtn form a ring.
    pub const DEMO: &[(&str, &str)] = &[
        ("tst-cuesta", "tst-lopez"),
        ("tst-cuesta", "tst-madonna"),
        ("tst-cuesta", "tst-blackmtn"),
        ("tst-madonna", "tst-blackmtn"),
        ("tst-madonna", "tst-shack"),
        ("tst-blackmtn", "tst-north"),
        ("tst-north", "tst-paso"),
    ];

    pub fn demo_pos(n: &str) -> (f64, f64) {
        match n {
            "tst-cuesta" => (35.33, -120.62),
            "tst-madonna" => (35.27, -120.68),
            "tst-blackmtn" => (35.38, -120.55),
            "tst-north" => (35.52, -120.68),
            "tst-paso" => (35.63, -120.69),
            "tst-lopez" => (35.18, -120.55),
            _ => (35.30, -120.66),
        }
    }

    pub fn fake() -> Fake {
        Fake::new(vec![
            Box::new(Mesh),
            Box::new(Notes),
            Box::new(crate::replicas::Replicas),
        ])
    }

    /// Two polls of every site, five minutes apart, with each site's links and position,
    /// and a service list from tst-shack.
    pub fn demo_log(links: &[(&str, &str)], services: Value) -> Fake {
        let mut l = fake();
        let hash = list_hash(&services);
        l.add(
            "mesh.list",
            json!({"hash": hash, "list": services}),
            NOW - 20 * MIN,
        );
        let mut sites: BTreeSet<&str> = BTreeSet::new();
        for (a, b) in links {
            sites.insert(a);
            sites.insert(b);
        }
        for t in [NOW - 10 * MIN, NOW - 5 * MIN] {
            for s in &sites {
                let nbs: Vec<Value> = links
                    .iter()
                    .filter_map(|(a, b)| {
                        let other = if a == s {
                            b
                        } else if b == s {
                            a
                        } else {
                            return None;
                        };
                        Some(json!({"neighbor": other, "type": "RF", "snr": 20.0, "quality": 95.0}))
                    })
                    .collect();
                let (lat, lon) = demo_pos(s);
                let mut p = json!({"node": s, "ok": true, "lat": lat, "lon": lon, "links": nbs});
                if *s == "tst-shack" {
                    p["ads_hash"] = json!(hash);
                }
                l.add("node.poll", p, t);
            }
        }
        l
    }

    pub fn demo_services() -> Value {
        json!([
            {"host": "tst-cuesta", "name": "Winlink Gateway", "link": "http://tst-cuesta.local.mesh:8080/", "protocol": "tcp"},
            {"host": "10.1.0.5", "name": "Paso Camera", "link": "http://tst-paso.local.mesh/cam", "protocol": "tcp"},
            {"host": "tst-shack", "name": "Chat Server", "link": "http://tst-shack.local.mesh:8080/", "protocol": "tcp"},
            {"host": "tst-pi", "name": "Wiki", "link": "http://tst-pi.local.mesh/", "protocol": "tcp"},
            {"host": "tst-lopez", "name": "", "link": "http://tst-lopez.local.mesh/", "protocol": "tcp"},
            {"name": "broken entry"}
        ])
    }

    pub fn demo_net() -> Net {
        let l = demo_log(DEMO, demo_services());
        Net::build(&l.conn, NOW, 20 * MIN, "tst-shack", &BTreeMap::new()).unwrap()
    }

    #[test]
    fn builds_the_live_graph_with_positions_and_services() {
        let n = demo_net();
        assert_eq!(n.sites.len(), 7);
        assert_eq!(n.anchor(), Some("tst-shack"));
        assert_eq!(
            n.pos["tst-paso"],
            Pos {
                lat: 35.63,
                lon: -120.69
            }
        );
        let names: Vec<&str> = n.services.iter().map(|p| p.svc.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["Chat Server", "Paso Camera", "Wiki", "Winlink Gateway"],
            "nameless and malformed entries are dropped"
        );
        let site = |name: &str| {
            n.services
                .iter()
                .find(|p| p.svc.name == name)
                .unwrap()
                .site
                .clone()
        };
        assert_eq!(site("Winlink Gateway").as_deref(), Some("tst-cuesta"));
        assert_eq!(
            site("Paso Camera").as_deref(),
            Some("tst-paso"),
            "placed by the host in its link"
        );
        assert_eq!(site("Wiki"), None, "a LAN host nobody mapped");
    }

    #[test]
    fn host_mapping_places_lan_services() {
        let l = demo_log(DEMO, demo_services());
        let map = [("tst-pi".to_string(), "tst-north".to_string())].into();
        let n = Net::build(&l.conn, NOW, 20 * MIN, "tst-shack", &map).unwrap();
        let wiki = n.services.iter().find(|p| p.svc.name == "Wiki").unwrap();
        assert_eq!(wiki.site.as_deref(), Some("tst-north"));
    }

    #[test]
    fn bad_positions_are_ignored_and_the_latest_good_one_wins() {
        let mut l = fake();
        for (t, lat, lon) in [
            (NOW - 30 * MIN, 35.1, -120.1),
            (NOW - 20 * MIN, 35.2, -120.2),
            (NOW - 10 * MIN, 0.0, 0.0),
            (NOW - 5 * MIN, 135.0, -120.0),
        ] {
            l.add(
                "node.poll",
                json!({"node": "a", "ok": true, "lat": lat, "lon": lon}),
                t,
            );
        }
        for (node, lon) in [("b", -120.3), ("c", 120.6)] {
            l.add(
                "node.poll",
                json!({"node": node, "ok": true, "lat": 35.3, "lon": lon}),
                NOW - MIN,
            );
        }
        let p = positions(&l.conn, NOW).unwrap();
        assert!(p.contains_key("b"));
        assert!(
            !p.contains_key("c"),
            "east instead of west: half a world away"
        );
        assert_eq!(
            p["a"],
            Pos {
                lat: 35.2,
                lon: -120.2
            }
        );
    }

    #[test]
    fn link_hosts_are_read_from_urls() {
        assert_eq!(
            link_host("http://TST-Pi.local.mesh:8080/x").as_deref(),
            Some("tst-pi")
        );
        assert_eq!(link_host("tst-node/x").as_deref(), Some("tst-node"));
        assert_eq!(link_host("http://10.1.2.3/"), None);
    }
}
