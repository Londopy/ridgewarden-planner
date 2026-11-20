//! Candidate sites ranked by the redundancy they add: how many single points of failure
//! they remove and how many sites no single failure could cut off any more. Judged against
//! the largest part of the mesh rather than from here, so every site ranks them the same.

use std::collections::{BTreeMap, BTreeSet};

use ridgewarden_client::graph::{Edge, Graph};

use crate::config::{Access, Candidate, Config, Lease, Power};
use crate::geo;
use crate::impact::cut_off;
use crate::net::{Net, Pos};
use crate::predict::{Calibration, Guess};
use crate::terrain::Terrain;

/// What predictions need: terrain, the radio settings and the calibration.
pub struct Predictor<'a> {
    pub terrain: &'a Terrain,
    pub cfg: &'a Config,
    pub cal: &'a Calibration,
}

/// A proposed link from a candidate to an existing site.
pub struct CandLink {
    pub site: String,
    pub km: Option<f64>,
    /// None when there is no terrain for the path
    pub guess: Option<Guess>,
    /// counted in the score: predicted workable, or not predicted at all
    pub counted: bool,
}

pub struct Ranked {
    pub cand: Candidate,
    /// typed into the form rather than configured
    pub entered: bool,
    pub links: Vec<CandLink>,
    /// link targets the mesh doesn't know
    pub unknown: Vec<String>,
    /// sites and links that would stop being single points of failure
    pub removed_sites: Vec<String>,
    pub removed_links: Vec<Edge>,
    /// sites a single failure could cut off now, but not with the candidate in place
    pub protected: BTreeSet<String>,
    /// sites cut off now that it would bring back
    pub reconnects: BTreeSet<String>,
    /// sites a single failure could still cut off
    pub still_exposed: BTreeSet<String>,
}

impl Ranked {
    pub fn removed(&self) -> usize {
        self.removed_sites.len() + self.removed_links.len()
    }

    pub fn counted(&self) -> usize {
        self.links.iter().filter(|l| l.counted).count()
    }
}

/// The human limits on a site, as minimums: "at least 4WD access".
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Filter {
    pub access: Option<Access>,
    pub power: Option<Power>,
    pub lease: Option<Lease>,
    /// part of the owner's name, any case
    pub owner: String,
}

impl Filter {
    pub fn is_empty(&self) -> bool {
        *self == Filter::default()
    }

    /// A candidate passes when it meets every limit set; an unknown value never does.
    pub fn keeps(&self, c: &Candidate) -> bool {
        fn at_least<T: PartialOrd>(min: Option<T>, v: Option<T>) -> bool {
            min.is_none_or(|m| v.is_some_and(|v| v >= m))
        }
        at_least(self.access, c.access)
            && at_least(self.power, c.power)
            && at_least(self.lease, c.lease)
            && (self.owner.trim().is_empty()
                || c.owner
                    .to_lowercase()
                    .contains(&self.owner.trim().to_lowercase()))
    }
}

/// Single points of failure and what each would cut off from the largest part of the mesh.
pub struct Spofs {
    pub sites: BTreeMap<String, BTreeSet<String>>,
    pub links: BTreeMap<Edge, BTreeSet<String>>,
    /// sites already outside the largest part
    pub cut_now: BTreeSet<String>,
}

impl Spofs {
    /// `ignore` is left out of every cut: a candidate hanging off one link is its own
    /// problem, not the mesh's.
    pub fn of(g: &Graph, ignore: Option<&str>) -> Self {
        let (aps, bridges) = g.cut_points();
        let no_sites = BTreeSet::new();
        let no_links = BTreeSet::new();
        let keep = |set: BTreeSet<String>| -> BTreeSet<String> {
            set.into_iter()
                .filter(|n| Some(n.as_str()) != ignore)
                .collect()
        };
        let sites = aps
            .into_iter()
            .map(|x| {
                let gone = [x.clone()].into();
                (x, keep(cut_off(g, &gone, &no_links, None).lost()))
            })
            .filter(|(_, c)| !c.is_empty())
            .collect();
        let links = bridges
            .into_iter()
            .map(|e| {
                let gone = [e.clone()].into();
                (e, keep(cut_off(g, &no_sites, &gone, None).lost()))
            })
            .filter(|(_, c)| !c.is_empty())
            .collect();
        Self {
            sites,
            links,
            cut_now: keep(cut_off(g, &no_sites, &no_links, None).already),
        }
    }

    /// Every site some single failure would cut off.
    pub fn exposed(&self) -> BTreeSet<String> {
        self.sites
            .values()
            .chain(self.links.values())
            .flatten()
            .cloned()
            .collect()
    }
}

fn link(net: &Net, pred: Option<&Predictor>, cand: &Candidate, site: &str) -> CandLink {
    let at = Pos {
        lat: cand.lat,
        lon: cand.lon,
    };
    let pos = net.pos.get(site).copied();
    let guess = match (pred, pos) {
        (Some(p), Some(pos)) => {
            let mut radio = p.cfg.radio.default_radio();
            radio.height_m = cand.height_m.unwrap_or(radio.height_m);
            match p.cal.guess(p.terrain, p.cfg, (at, &radio), (site, pos)) {
                Ok(g) => g,
                Err(e) => {
                    tracing::debug!("predicting {} - {site}: {e:#}", cand.name);
                    None
                }
            }
        }
        _ => None,
    };
    CandLink {
        site: site.to_string(),
        km: pos.map(|p| geo::distance_km(at, p)),
        counted: guess.as_ref().is_none_or(|g| g.workable),
        guess,
    }
}

fn score(
    net: &Net,
    base: &Spofs,
    pred: Option<&Predictor>,
    cand: Candidate,
    entered: bool,
) -> Ranked {
    // not a valid node name, so it can't collide with a real site
    let id = format!("candidate:{}", cand.name);
    let (known, unknown): (Vec<String>, Vec<String>) = cand
        .links
        .iter()
        .cloned()
        .partition(|l| net.sites.contains(l));
    let links: Vec<CandLink> = known.iter().map(|l| link(net, pred, &cand, l)).collect();
    let mut g = net.topo.graph.clone();
    g.add_node(&id);
    for l in links.iter().filter(|l| l.counted) {
        g.add_edge(&id, &l.site);
    }
    let after = Spofs::of(&g, Some(&id));
    let still_exposed = after.exposed();
    Ranked {
        links,
        unknown,
        removed_sites: base
            .sites
            .keys()
            .filter(|n| !after.sites.contains_key(*n))
            .cloned()
            .collect(),
        removed_links: base
            .links
            .keys()
            .filter(|e| !after.links.contains_key(*e))
            .cloned()
            .collect(),
        protected: base.exposed().difference(&still_exposed).cloned().collect(),
        reconnects: base.cut_now.difference(&after.cut_now).cloned().collect(),
        still_exposed,
        cand,
        entered,
    }
}

/// Score each candidate on its own against the live mesh, best first: most sites
/// protected, then most single points of failure removed, then fewest new links. With a
/// predictor, links predicted too weak to work are left out of the score.
pub fn rank(net: &Net, cands: Vec<(Candidate, bool)>, pred: Option<&Predictor>) -> Vec<Ranked> {
    let base = Spofs::of(&net.topo.graph, None);
    let mut out: Vec<Ranked> = cands
        .into_iter()
        .map(|(c, entered)| score(net, &base, pred, c, entered))
        .collect();
    out.sort_by(|a, b| {
        (b.protected.len() + b.reconnects.len())
            .cmp(&(a.protected.len() + a.reconnects.len()))
            .then_with(|| b.removed().cmp(&a.removed()))
            .then_with(|| (a.counted() == 0).cmp(&(b.counted() == 0)))
            .then_with(|| a.counted().cmp(&b.counted()))
            .then_with(|| a.cand.name.cmp(&b.cand.name))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::tests::demo_net;
    use ridgewarden_client::graph::edge;

    fn cand(name: &str, links: &[&str]) -> (Candidate, bool) {
        (
            Candidate::at(
                name.into(),
                35.25,
                -120.55,
                links.iter().map(|s| s.to_string()).collect(),
            ),
            false,
        )
    }

    fn set(xs: &[&str]) -> BTreeSet<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_site_closing_a_loop_protects_the_sites_behind_it() {
        let net = demo_net();
        assert_eq!(
            Spofs::of(&net.topo.graph, None).exposed(),
            set(&["tst-lopez", "tst-north", "tst-paso", "tst-shack"])
        );
        let r = rank(&net, vec![cand("Hill", &["tst-lopez", "tst-paso"])], None);
        let h = &r[0];
        // lopez - hill - paso closes a ring through cuesta, blackmtn and north
        assert_eq!(h.protected, set(&["tst-lopez", "tst-north", "tst-paso"]));
        assert_eq!(h.still_exposed, set(&["tst-shack"]));
        assert_eq!(
            h.removed_sites,
            vec!["tst-blackmtn", "tst-cuesta", "tst-north"]
        );
        assert_eq!(h.removed_links.len(), 3);
        assert!(h.links.iter().all(|l| l.km.is_some() && l.counted));
    }

    #[test]
    fn candidates_rank_by_sites_protected() {
        let net = demo_net();
        let r = rank(
            &net,
            vec![
                cand("Leaf", &["tst-cuesta"]),
                cand("Partial", &["tst-cuesta", "tst-north"]),
                cand("Ring", &["tst-lopez", "tst-paso"]),
                cand("Ghost", &["tst-nowhere"]),
            ],
            None,
        );
        let names: Vec<&str> = r.iter().map(|x| x.cand.name.as_str()).collect();
        assert_eq!(names, vec!["Ring", "Partial", "Leaf", "Ghost"]);
        let partial = &r[1];
        // cuesta - partial - north makes blackmtn-north redundant, but paso still hangs off north
        assert_eq!(partial.protected, set(&["tst-north"]));
        assert_eq!(
            partial.removed_links,
            vec![edge("tst-blackmtn", "tst-north")]
        );
        assert!(partial.still_exposed.contains("tst-paso"));
        let leaf = &r[2];
        assert!(leaf.protected.is_empty() && leaf.removed() == 0);
        assert_eq!(r[3].unknown, vec!["tst-nowhere"]);
    }

    #[test]
    fn links_predicted_too_weak_are_left_out() {
        let net = demo_net();
        let dir = tempfile::tempdir().unwrap();
        let terrain = Terrain::new(dir.path());
        for lon in [-121, -122] {
            terrain
                .store(35, lon, &crate::terrain::tests::synthetic(|_, _| 100))
                .unwrap();
        }
        let mut cfg: Config = toml::from_str(crate::config::EXAMPLE).unwrap();
        // tall masts, so 50 km over flat ground is line of sight
        cfg.radio.height_m = 100.0;
        let cal = Calibration::default();
        let ring = || vec![cand("Ring", &["tst-lopez", "tst-paso"])];
        let p = Predictor {
            terrain: &terrain,
            cfg: &cfg,
            cal: &cal,
        };
        let r = rank(&net, ring(), Some(&p));
        assert!(r[0].links.iter().all(|l| l.guess.is_some() && l.counted));
        assert_eq!(r[0].protected.len(), 3, "flat ground: both links work");
        cfg.radio.min_snr_db = 80.0;
        let p = Predictor {
            terrain: &terrain,
            cfg: &cfg,
            cal: &cal,
        };
        let r = rank(&net, ring(), Some(&p));
        assert_eq!(r[0].counted(), 0);
        assert!(
            r[0].protected.is_empty(),
            "links that won't work protect nobody"
        );
    }

    #[test]
    fn filters_need_known_values_at_least_as_good() {
        let mut c = Candidate::at("Hill".into(), 35.0, -120.0, vec!["a".into()]);
        c.access = Some(Access::Offroad);
        c.power = Some(Power::Solar);
        c.owner = "County Parks".into();
        let f = |access, power, lease, owner: &str| Filter {
            access,
            power,
            lease,
            owner: owner.into(),
        };
        assert!(Filter::default().keeps(&c));
        assert!(f(Some(Access::Foot), None, None, "").keeps(&c));
        assert!(!f(Some(Access::Road), None, None, "").keeps(&c));
        assert!(!f(None, Some(Power::Grid), None, "").keeps(&c));
        assert!(
            !f(None, None, Some(Lease::Pending), "").keeps(&c),
            "lease unknown"
        );
        assert!(f(None, None, None, "parks").keeps(&c));
        assert!(!f(None, None, None, "forest").keeps(&c));
    }
}
