//! The mesh drawn as inline SVG from each node's reported position. Every site and
//! working link is a plain link into the what-if page, so the map is interactive without
//! any JavaScript.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use ridgewarden_client::graph::Edge;
use ridgewarden_client::util::esc;

use crate::config::{Candidate, Config};
use crate::geo::state_word;
use crate::impact::Scenario;
use crate::net::{Net, Pos};

const W: f64 = 640.0;
const MAX_H: f64 = 460.0;
const PAD: f64 = 26.0;
/// room on the right for the last site's label
const PAD_RIGHT: f64 = 110.0;

const STYLE: &str = "<style>\
svg.map{width:100%;height:auto;display:block;margin:12px 0 6px;border:1px solid var(--line);border-radius:6px}\
.map .faded{opacity:.25}\
.map .l{stroke-width:3;stroke-linecap:round}.map .up{stroke:var(--green)}\
.map .unknown{stroke:var(--unknown);stroke-dasharray:6 4}\
.map .down{stroke:var(--red);stroke-dasharray:2 5;stroke-width:2}\
.map .bridge{stroke-width:6}.map line.gone{stroke:var(--red);stroke-width:6;opacity:.5}\
.map .hit{stroke:transparent;stroke-width:18}\
.map .plan{stroke:var(--fg);stroke-dasharray:3 4;stroke-width:1.5}\
.map circle{fill:var(--fg);stroke:var(--card);stroke-width:2}\
.map circle.ap{fill:var(--yellow)}.map circle.lost{fill:var(--red)}\
.map circle.gone{fill:var(--card);stroke:var(--red);stroke-width:3}\
.map circle.ring{fill:none;stroke:var(--fg);stroke-width:1.5}\
.map rect{fill:var(--card);stroke:var(--fg);stroke-width:2}\
.map text{font-size:13px;fill:var(--fg);paint-order:stroke;stroke:var(--card);stroke-width:3px}\
.map a:focus circle,.map a:hover circle{stroke:var(--fg);stroke-width:3}\
.map a:focus .hit,.map a:hover .hit{stroke:var(--line)}\
</style>";

pub struct Map<'a> {
    pub net: &'a Net,
    pub cfg: &'a Config,
    /// what is taken out now; clicking toggles a site or link in it
    pub sc: &'a Scenario,
    /// sites that lose contact in this scenario
    pub lost: &'a BTreeSet<String>,
    pub candidates: &'a [Candidate],
}

struct Frame {
    min_lon: f64,
    max_lat: f64,
    k: f64,
    scale: f64,
    dx: f64,
    dy: f64,
    h: f64,
}

impl Frame {
    /// An equirectangular fit of the points into the drawing, centred.
    fn fit(pts: &[Pos]) -> Option<Self> {
        let min_lat = pts.iter().map(|p| p.lat).reduce(f64::min)?;
        let max_lat = pts.iter().map(|p| p.lat).reduce(f64::max)?;
        let min_lon = pts.iter().map(|p| p.lon).reduce(f64::min)?;
        let max_lon = pts.iter().map(|p| p.lon).reduce(f64::max)?;
        let k = ((min_lat + max_lat) / 2.0).to_radians().cos();
        let span_x = ((max_lon - min_lon) * k).max(1e-3);
        let span_y = (max_lat - min_lat).max(1e-3);
        let inner_w = W - PAD - PAD_RIGHT;
        let scale = (inner_w / span_x).min((MAX_H - 2.0 * PAD) / span_y);
        let h = (span_y * scale + 2.0 * PAD).max(160.0);
        Some(Self {
            min_lon,
            max_lat,
            k,
            scale,
            dx: PAD + (inner_w - span_x * scale) / 2.0,
            dy: (h - span_y * scale) / 2.0,
            h,
        })
    }

    fn xy(&self, p: Pos) -> (f64, f64) {
        (
            self.dx + (p.lon - self.min_lon) * self.k * self.scale,
            self.dy + (self.max_lat - p.lat) * self.scale,
        )
    }
}

impl Map<'_> {
    fn href(&self, sc: Scenario) -> String {
        format!("/whatif?{}", sc.query().replace('&', "&amp;"))
    }

    /// The map, or None when no site has reported a position.
    pub fn render(&self) -> Option<String> {
        let net = self.net;
        let l = |n: &str| self.cfg.label(n);
        let cands: Vec<(&Candidate, Pos)> = self
            .candidates
            .iter()
            .map(|c| {
                (
                    c,
                    Pos {
                        lat: c.lat,
                        lon: c.lon,
                    },
                )
            })
            .collect();
        let placed: BTreeMap<&String, Pos> = net
            .sites
            .iter()
            .filter_map(|s| Some((s, *net.pos.get(s)?)))
            .collect();
        let all: Vec<Pos> = placed
            .values()
            .copied()
            .chain(cands.iter().map(|c| c.1))
            .collect();
        let f = Frame::fit(&all)?;
        let mut s = String::new();
        let _ = write!(
            s,
            "<svg class=map viewBox=\"0 0 {W} {:.0}\" role=group aria-label=\"Map of the mesh\">{STYLE}\
             <rect x=\"0\" y=\"0\" width=\"{W}\" height=\"{:.0}\" style=\"fill:var(--card);stroke:none\"/>",
            f.h, f.h
        );
        let bridges: BTreeSet<&Edge> = net.topo.bridges.iter().map(|(e, _)| e).collect();
        let edges = net.topo.graph.edges();
        for p in &net.pairs {
            let (Some(a), Some(b)) = (placed.get(&p.a), placed.get(&p.b)) else {
                continue;
            };
            let ((x1, y1), (x2, y2)) = (f.xy(*a), f.xy(*b));
            let e = p.edge();
            let mut class = format!("l {}", state_word(p.state));
            if bridges.contains(&e) {
                class.push_str(" bridge");
            }
            if self.sc.links.contains(&e) {
                class.push_str(" gone");
            } else if self.sc.sites.contains(&p.a) || self.sc.sites.contains(&p.b) {
                class.push_str(" faded");
            }
            let mut title = format!("{} - {}: {}", l(&p.a), l(&p.b), state_word(p.state));
            if let Some(snr) = p.snr() {
                let _ = write!(title, ", SNR {snr:.0} dB");
            }
            let seg = format!("x1=\"{x1:.1}\" y1=\"{y1:.1}\" x2=\"{x2:.1}\" y2=\"{y2:.1}\"");
            if edges.contains(&e) {
                let _ = write!(
                    s,
                    "<a href=\"{}\"><title>{}</title><line class=hit {seg}/><line class=\"{class}\" {seg}/></a>",
                    self.href(self.sc.toggle_link(&e)),
                    esc(&title)
                );
            } else {
                let _ = write!(
                    s,
                    "<line class=\"{class}\" {seg}><title>{}</title></line>",
                    esc(&title)
                );
            }
        }
        for (c, at) in &cands {
            let (x1, y1) = f.xy(*at);
            for to in c.links.iter().filter_map(|t| placed.get(t)) {
                let (x2, y2) = f.xy(*to);
                let _ = write!(
                    s,
                    "<line class=plan x1=\"{x1:.1}\" y1=\"{y1:.1}\" x2=\"{x2:.1}\" y2=\"{y2:.1}\"/>"
                );
            }
        }
        let aps: BTreeSet<&String> = net.topo.aps.iter().map(|(n, _)| n).collect();
        for (site, at) in &placed {
            let (x, y) = f.xy(*at);
            let class = if self.sc.sites.contains(*site) {
                "gone"
            } else if self.lost.contains(*site) {
                "lost"
            } else if aps.contains(site) {
                "ap"
            } else {
                ""
            };
            let ring = if **site == net.here {
                format!("<circle class=ring cx=\"{x:.1}\" cy=\"{y:.1}\" r=\"11\"/>")
            } else {
                String::new()
            };
            let _ = write!(
                s,
                "<a href=\"{}\"><title>{}</title>{ring}<circle class=\"{class}\" cx=\"{x:.1}\" cy=\"{y:.1}\" r=\"7\"/>\
                 <text x=\"{:.1}\" y=\"{:.1}\">{}</text></a>",
                self.href(self.sc.toggle_site(site)),
                esc(&l(site)),
                x + 11.0,
                y + 4.5,
                esc(&l(site))
            );
        }
        for (c, at) in &cands {
            let (x, y) = f.xy(*at);
            let _ = write!(
                s,
                "<g><title>Candidate: {}</title><rect x=\"{:.1}\" y=\"{:.1}\" width=\"12\" height=\"12\" transform=\"rotate(45 {x:.1} {y:.1})\"/>\
                 <text x=\"{:.1}\" y=\"{:.1}\">{}</text></g>",
                esc(&c.name),
                x - 6.0,
                y - 6.0,
                x + 11.0,
                y + 4.5,
                esc(&c.name)
            );
        }
        s.push_str("</svg>");
        Some(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::EXAMPLE;
    use crate::net::tests::demo_net;

    #[test]
    fn sites_and_links_toggle_the_scenario() {
        let net = demo_net();
        let cfg: Config = toml::from_str(EXAMPLE).unwrap();
        let sc = Scenario::default().toggle_site("tst-cuesta");
        let lost = ["tst-lopez".to_string()].into();
        let svg = Map {
            net: &net,
            cfg: &cfg,
            sc: &sc,
            lost: &lost,
            candidates: &[],
        }
        .render()
        .unwrap();
        // clicking cuesta again brings it back; clicking paso adds it
        assert!(svg.contains("<a href=\"/whatif?\">"));
        assert!(svg.contains("href=\"/whatif?site=tst-cuesta&amp;site=tst-paso\""));
        assert!(svg.contains("href=\"/whatif?site=tst-cuesta&amp;link=tst-north%7Ctst-paso\""));
        assert!(svg.contains("<circle class=\"gone\""));
        assert!(svg.contains("<circle class=\"lost\""));
        assert_eq!(svg.matches("class=ring").count(), 1, "here is marked");
    }

    #[test]
    fn nothing_to_draw_without_positions() {
        let mut net = demo_net();
        net.pos.clear();
        let cfg: Config = toml::from_str(EXAMPLE).unwrap();
        let m = Map {
            net: &net,
            cfg: &cfg,
            sc: &Scenario::default(),
            lost: &BTreeSet::new(),
            candidates: &[],
        };
        assert!(m.render().is_none());
    }
}
