//! "What if this dies": which sites lose contact and which advertised services become
//! unreachable from here, in plain words.

use std::collections::BTreeSet;

use ridgewarden_client::graph::{Edge, Graph, edge};
use ridgewarden_client::util::norm_node;

use crate::net::{Net, Placed};

/// Sites and links taken out.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Scenario {
    pub sites: BTreeSet<String>,
    pub links: BTreeSet<Edge>,
}

impl Scenario {
    /// From `site=<node>` and `link=<a>|<b>` query pairs. Anything that isn't a site or a
    /// working link in the graph is dropped.
    pub fn from_query(q: &[(String, String)], net: &Net) -> Self {
        let edges = net.topo.graph.edges();
        let mut sc = Scenario::default();
        for (k, v) in q {
            match k.as_str() {
                "site" => {
                    let n = norm_node(v);
                    if net.sites.contains(&n) {
                        sc.sites.insert(n);
                    }
                }
                "link" => {
                    if let Some((a, b)) = v.split_once('|') {
                        let e = edge(&norm_node(a), &norm_node(b));
                        if edges.contains(&e) {
                            sc.links.insert(e);
                        }
                    }
                }
                _ => {}
            }
        }
        sc
    }

    pub fn is_empty(&self) -> bool {
        self.sites.is_empty() && self.links.is_empty()
    }

    /// The query string for this scenario. Node names are DNS labels, so only the
    /// separator needs encoding.
    pub fn query(&self) -> String {
        self.sites
            .iter()
            .map(|s| format!("site={s}"))
            .chain(self.links.iter().map(|(a, b)| format!("link={a}%7C{b}")))
            .collect::<Vec<_>>()
            .join("&")
    }

    pub fn toggle_site(&self, s: &str) -> Self {
        let mut sc = self.clone();
        if !sc.sites.remove(s) {
            sc.sites.insert(s.to_string());
        }
        sc
    }

    pub fn toggle_link(&self, e: &Edge) -> Self {
        let mut sc = self.clone();
        if !sc.links.remove(e) {
            sc.links.insert(e.clone());
        }
        sc
    }
}

/// Whether a service can still be reached from the site the report is judged from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Reach {
    /// its site is one of those taken out
    SiteDown,
    /// its site loses contact
    CutOff,
    /// its site was cut off before this
    AlreadyCut,
    /// we can't tell which site it runs at
    Unplaced,
    Reachable,
}

impl Reach {
    pub fn word(self) -> &'static str {
        match self {
            Reach::SiteDown => "its site is down",
            Reach::CutOff => "cut off",
            Reach::AlreadyCut => "already cut off",
            Reach::Unplaced => "site unknown",
            Reach::Reachable => "still reachable",
        }
    }
}

pub struct Impact {
    /// the site this is judged from; None means the largest part of the mesh
    pub from: Option<String>,
    /// this planner's own site, when it is among those taken out
    pub here_down: Option<String>,
    /// what is left has no single largest part to judge from
    pub split: bool,
    /// sites that lose contact, not counting those taken out or already cut off
    pub lost: BTreeSet<String>,
    /// the lost sites grouped by which can still reach each other
    pub islands: Vec<BTreeSet<String>>,
    pub already: BTreeSet<String>,
    pub services: Vec<(Placed, Reach)>,
    /// sites carrying the replicated chat that can be reached, before and after
    pub replicas: Option<(BTreeSet<String>, BTreeSet<String>)>,
}

/// Who a failure separates from the main part of the mesh.
pub struct Cut {
    /// no part is clearly the main one, so every part counts as cut off
    pub split: bool,
    /// the parts cut off, without sites that were cut off before
    pub islands: Vec<BTreeSet<String>>,
    /// sites already outside the main part before the failure
    pub already: BTreeSet<String>,
}

impl Cut {
    pub fn lost(&self) -> BTreeSet<String> {
        self.islands.iter().flatten().cloned().collect()
    }
}

/// Which part counts as the mesh: the one holding `from`, or else the one keeping most of
/// `before`. None when two parts tie for that, as when a hub with only spokes dies.
fn main_part(
    comps: &[BTreeSet<String>],
    from: Option<&str>,
    before: &BTreeSet<String>,
) -> Option<usize> {
    if let Some(i) = from.and_then(|f| comps.iter().position(|c| c.contains(f))) {
        return Some(i);
    }
    let kept: Vec<usize> = comps
        .iter()
        .map(|c| c.intersection(before).count())
        .collect();
    let best = *kept.iter().max()?;
    (kept.iter().filter(|k| **k == best).count() == 1)
        .then(|| kept.iter().position(|k| *k == best))
        .flatten()
}

/// What taking out `nodes` and `edges` cuts off from `from`, or from the largest part of
/// the mesh when `from` is None.
pub fn cut_off(
    g: &Graph,
    nodes: &BTreeSet<String>,
    edges: &BTreeSet<Edge>,
    from: Option<&str>,
) -> Cut {
    let base = g.components(&BTreeSet::new(), &BTreeSet::new());
    let main0 = base
        .first()
        .and_then(|largest| main_part(&base, from, largest));
    let before = main0.map(|i| base[i].clone()).unwrap_or_default();
    let already: BTreeSet<String> = g
        .nodes()
        .filter(|n| !before.contains(*n))
        .cloned()
        .collect();
    let comps = g.components(nodes, edges);
    let main = main_part(&comps, from, &before);
    let islands = comps
        .iter()
        .enumerate()
        .filter(|(i, _)| Some(*i) != main)
        .map(|(_, c)| c.difference(&already).cloned().collect::<BTreeSet<_>>())
        .filter(|c| !c.is_empty())
        .collect();
    Cut {
        split: main.is_none() && comps.len() > 1,
        islands,
        already,
    }
}

pub fn impact(net: &Net, sc: &Scenario) -> Impact {
    let here_down = net
        .anchor()
        .filter(|a| sc.sites.contains(*a))
        .map(str::to_string);
    let from = net.anchor().filter(|_| here_down.is_none());
    let cut = cut_off(&net.topo.graph, &sc.sites, &sc.links, from);
    let lost = cut.lost();
    let services = net
        .services
        .iter()
        .map(|p| {
            let r = match &p.site {
                None => Reach::Unplaced,
                Some(s) if sc.sites.contains(s) => Reach::SiteDown,
                Some(s) if cut.already.contains(s) => Reach::AlreadyCut,
                Some(s) if lost.contains(s) => Reach::CutOff,
                Some(_) => Reach::Reachable,
            };
            (p.clone(), r)
        })
        .collect();
    let replicas = (!net.replicas.is_empty()).then(|| {
        let before: BTreeSet<String> = net.replicas.difference(&cut.already).cloned().collect();
        let after = before
            .iter()
            .filter(|s| !sc.sites.contains(*s) && !lost.contains(*s))
            .cloned()
            .collect();
        (before, after)
    });
    Impact {
        from: from.map(str::to_string),
        here_down,
        split: cut.split,
        replicas,
        lost,
        islands: cut.islands,
        already: cut.already,
        services,
    }
}

/// "a", "a and b", "a, b and c"
pub fn list(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

impl Impact {
    /// Services lost by this scenario (not those already out of reach).
    pub fn lost_services(&self) -> impl Iterator<Item = &(Placed, Reach)> {
        self.services
            .iter()
            .filter(|(_, r)| matches!(r, Reach::SiteDown | Reach::CutOff))
    }

    /// The report in sentences, with sites named by `label`.
    pub fn sentences(&self, sc: &Scenario, label: &dyn Fn(&str) -> String) -> Vec<String> {
        let names = |s: &BTreeSet<String>| list(&s.iter().map(|n| label(n)).collect::<Vec<_>>());
        let what: Vec<String> = sc
            .sites
            .iter()
            .map(|s| label(s))
            .chain(
                sc.links
                    .iter()
                    .map(|(a, b)| format!("the {} - {} link", label(a), label(b))),
            )
            .collect();
        let what = list(&what);
        let from = match &self.from {
            Some(f) => label(f),
            None if self.split => "some of the other parts".into(),
            None => "the largest part of the mesh".into(),
        };
        let mut out = vec![];
        out.push(if self.split {
            format!(
                "Losing {what} splits the mesh into {} parts that can't reach each other.",
                self.islands.len()
            )
        } else if self.lost.is_empty() {
            match &self.from {
                Some(_) => format!("Losing {what} cuts nobody else off from {from}."),
                None => format!("Losing {what} cuts nobody else off."),
            }
        } else {
            format!("Losing {what} cuts {} off from {from}.", names(&self.lost))
        });
        if let Some(h) = &self.here_down {
            out.push(format!(
                "{} is this planner's own site, so this is judged from the largest part left.",
                label(h)
            ));
        }
        if self.split || self.islands.len() > 1 || self.islands.iter().any(|i| i.len() > 1) {
            for i in &self.islands {
                out.push(if i.len() == 1 {
                    format!("{} is left on its own.", names(i))
                } else {
                    format!("{} can still reach each other.", names(i))
                });
            }
        }
        let lost: Vec<String> = self
            .lost_services()
            .map(|(p, _)| p.svc.name.clone())
            .collect();
        if !lost.is_empty() {
            out.push(format!(
                "{} {} unreachable from {from}: {}.",
                lost.len(),
                if lost.len() == 1 {
                    "service becomes"
                } else {
                    "services become"
                },
                list(&lost)
            ));
        }
        if let Some((before, after)) = &self.replicas
            && after.len() < before.len()
        {
            out.push(if after.is_empty() {
                format!(
                    "The replicated chat (ridgewarden-failover) loses every copy {from} could reach: it ran at {}.",
                    names(before)
                )
            } else {
                format!(
                    "The replicated chat (ridgewarden-failover) keeps {} of its {} copies reachable from {from}: {}.",
                    after.len(),
                    before.len(),
                    names(after)
                )
            });
        }
        if !self.already.is_empty() {
            out.push(format!(
                "{} {} already cut off before this.",
                names(&self.already),
                if self.already.len() == 1 {
                    "was"
                } else {
                    "were"
                }
            ));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::tests::{DEMO, NOW, demo_log, demo_net, demo_services};
    use crate::net::{Net, tests::MIN};
    use std::collections::BTreeMap;

    fn q(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn id(n: &str) -> String {
        n.to_string()
    }

    fn reach(i: &Impact, name: &str) -> Reach {
        i.services
            .iter()
            .find(|(p, _)| p.svc.name == name)
            .unwrap()
            .1
    }

    #[test]
    fn losing_cuesta_cuts_off_lopez_and_its_own_service() {
        let net = demo_net();
        let sc = Scenario::from_query(&q(&[("site", "TST-CUESTA")]), &net);
        let i = impact(&net, &sc);
        assert_eq!(i.lost, ["tst-lopez".to_string()].into());
        assert_eq!(reach(&i, "Winlink Gateway"), Reach::SiteDown);
        assert_eq!(reach(&i, "Paso Camera"), Reach::Reachable);
        assert_eq!(reach(&i, "Wiki"), Reach::Unplaced);
        let s = i.sentences(&sc, &id);
        assert_eq!(s[0], "Losing tst-cuesta cuts tst-lopez off from tst-shack.");
        assert_eq!(
            s[1],
            "1 service becomes unreachable from tst-shack: Winlink Gateway."
        );
    }

    #[test]
    fn a_link_cut_reports_the_island_and_its_services() {
        let net = demo_net();
        let sc = Scenario::from_query(&q(&[("link", "tst-north|tst-blackmtn")]), &net);
        assert_eq!(sc.links.len(), 1);
        let i = impact(&net, &sc);
        assert_eq!(
            i.lost,
            ["tst-north".to_string(), "tst-paso".to_string()].into()
        );
        assert_eq!(reach(&i, "Paso Camera"), Reach::CutOff);
        let s = i.sentences(&sc, &id);
        assert_eq!(
            s[0],
            "Losing the tst-blackmtn - tst-north link cuts tst-north and tst-paso off from tst-shack."
        );
        assert_eq!(s[1], "tst-north and tst-paso can still reach each other.");
    }

    #[test]
    fn a_redundant_link_cuts_nobody_off() {
        let net = demo_net();
        let sc = Scenario::from_query(&q(&[("link", "tst-cuesta|tst-madonna")]), &net);
        let i = impact(&net, &sc);
        assert!(i.lost.is_empty());
        assert_eq!(
            i.sentences(&sc, &id)[0],
            "Losing the tst-cuesta - tst-madonna link cuts nobody else off from tst-shack."
        );
    }

    #[test]
    fn losing_here_is_judged_from_the_largest_part() {
        let net = demo_net();
        let sc = Scenario::from_query(&q(&[("site", "tst-shack")]), &net);
        let i = impact(&net, &sc);
        assert!(i.here_down.is_some() && i.from.is_none());
        assert_eq!(
            i.sentences(&sc, &id)[1],
            "tst-shack is this planner's own site, so this is judged from the largest part left."
        );
        assert!(i.lost.is_empty());
        assert_eq!(reach(&i, "Chat Server"), Reach::SiteDown);
    }

    #[test]
    fn two_failures_together_split_the_mesh() {
        let net = demo_net();
        let sc = Scenario::from_query(
            &q(&[("site", "tst-cuesta"), ("link", "tst-blackmtn|tst-north")]),
            &net,
        );
        let i = impact(&net, &sc);
        assert_eq!(i.islands.len(), 2);
        let s = i.sentences(&sc, &id);
        assert_eq!(
            s[0],
            "Losing tst-cuesta and the tst-blackmtn - tst-north link cuts tst-lopez, tst-north and tst-paso off from tst-shack."
        );
        assert!(s.contains(&"tst-lopez is left on its own.".to_string()));
    }

    #[test]
    fn sites_already_cut_off_are_not_blamed_on_the_scenario() {
        // north - paso is down: paso is already gone before anything else fails
        let mut links = DEMO.to_vec();
        links.retain(|l| *l != ("tst-north", "tst-paso"));
        let mut l = demo_log(&links, demo_services());
        // north heard paso until recently; two polls without it make the link down
        for t in [NOW - 60 * MIN, NOW - 50 * MIN] {
            l.add(
                "node.poll",
                serde_json::json!({"node": "tst-north", "ok": true, "links": [
                    {"neighbor": "tst-paso", "snr": 20.0},
                    {"neighbor": "tst-blackmtn", "snr": 20.0}
                ]}),
                t,
            );
        }
        let net = Net::build(&l.conn, NOW, 20 * MIN, "tst-shack", &BTreeMap::new()).unwrap();
        let sc = Scenario::from_query(&q(&[("site", "tst-blackmtn")]), &net);
        let i = impact(&net, &sc);
        assert_eq!(i.lost, ["tst-north".to_string()].into());
        assert_eq!(i.already, ["tst-paso".to_string()].into());
        assert_eq!(reach(&i, "Paso Camera"), Reach::AlreadyCut);
        assert_eq!(
            i.sentences(&sc, &id).last().unwrap(),
            "tst-paso was already cut off before this."
        );
    }

    #[test]
    fn unknown_sites_and_links_are_dropped_from_the_query() {
        let net = demo_net();
        let sc = Scenario::from_query(
            &q(&[
                ("site", "tst-nowhere"),
                ("link", "tst-lopez|tst-paso"),
                ("link", "garbage"),
            ]),
            &net,
        );
        assert!(sc.is_empty());
    }

    #[test]
    fn scenarios_round_trip_through_the_query_and_toggle() {
        let net = demo_net();
        let sc = Scenario::default()
            .toggle_site("tst-cuesta")
            .toggle_link(&edge("tst-north", "tst-paso"));
        assert_eq!(sc.query(), "site=tst-cuesta&link=tst-north%7Ctst-paso");
        let back = Scenario::from_query(
            &q(&[("site", "tst-cuesta"), ("link", "tst-north|tst-paso")]),
            &net,
        );
        assert_eq!(back, sc);
        assert!(sc.toggle_site("tst-cuesta").sites.is_empty());
    }

    fn graph(links: &[(&str, &str)]) -> Graph {
        let mut g = Graph::default();
        for (a, b) in links {
            g.add_edge(a, b);
        }
        g
    }

    fn set(xs: &[&str]) -> BTreeSet<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn without_a_viewpoint_the_part_that_was_largest_stays_main() {
        // a chain h1..h9 and a separate ring r1..r5: losing h5 leaves two chain halves of
        // four, both smaller than the ring, yet neither half is "the mesh" over the other
        let mut links: Vec<(String, String)> = (1..9)
            .map(|i| (format!("h{i}"), format!("h{}", i + 1)))
            .collect();
        links.extend((1..=5).map(|i| (format!("r{i}"), format!("r{}", i % 5 + 1))));
        let refs: Vec<(&str, &str)> = links
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let g = graph(&refs);
        let c = cut_off(&g, &set(&["h5"]), &BTreeSet::new(), None);
        assert!(c.split, "two equal halves of the chain");
        assert_eq!(c.lost().len(), 8);
        assert_eq!(c.already.len(), 5, "the ring was never part of the chain");
        let c = cut_off(&g, &set(&["h8"]), &BTreeSet::new(), None);
        assert!(!c.split);
        assert_eq!(c.lost(), set(&["h9"]));
    }

    #[test]
    fn losing_a_hub_with_only_spokes_splits_the_mesh() {
        let g = graph(&[("hub", "n1"), ("hub", "n2"), ("hub", "n3")]);
        let c = cut_off(&g, &set(&["hub"]), &BTreeSet::new(), None);
        assert!(c.split);
        assert_eq!(c.lost(), set(&["n1", "n2", "n3"]));
        let c = cut_off(&g, &set(&["hub"]), &BTreeSet::new(), Some("n2"));
        assert!(!c.split);
        assert_eq!(c.lost(), set(&["n1", "n3"]));
    }

    #[test]
    fn the_replicated_chat_counts_its_reachable_copies() {
        let mut l = demo_log(DEMO, demo_services());
        for site in ["tst-shack", "tst-paso"] {
            l.add(
                crate::replicas::SITE,
                serde_json::json!({"node": site}),
                NOW - 2 * MIN,
            );
        }
        let net = Net::build(&l.conn, NOW, 20 * MIN, "tst-shack", &BTreeMap::new()).unwrap();
        let sc = Scenario::from_query(&q(&[("site", "tst-north")]), &net);
        let i = impact(&net, &sc);
        assert!(
            i.sentences(&sc, &id).contains(
                &"The replicated chat (ridgewarden-failover) keeps 1 of its 2 copies reachable from tst-shack: tst-shack."
                    .to_string()
            ),
            "{:?}",
            i.sentences(&sc, &id)
        );
        let sc = Scenario::from_query(&q(&[("site", "tst-cuesta")]), &net);
        assert!(
            !impact(&net, &sc)
                .sentences(&sc, &id)
                .iter()
                .any(|s| s.contains("replicated")),
            "no copy lost, nothing to say"
        );
    }

    #[test]
    fn lists_read_like_english() {
        let v = |xs: &[&str]| xs.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(list(&v(&["a"])), "a");
        assert_eq!(list(&v(&["a", "b"])), "a and b");
        assert_eq!(list(&v(&["a", "b", "c"])), "a, b and c");
    }
}
