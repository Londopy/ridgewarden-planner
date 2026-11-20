//! The printable report: the mesh as it is, what one failure would cut off, how far the
//! predictions are off, and the candidate sites ranked. Plain numbers for a grant writer.

use std::collections::BTreeSet;

use ridgewarden_client::util::short_time;
use ridgewarden_linkdoctor::links::LinkState;

use crate::View;
use crate::config::Config;
use crate::impact::{self, Scenario, list};
use crate::pdf::{self, Page, Writer};
use crate::rank::{Ranked, Spofs};

/// Sites and links drawn in a box on the current page.
fn map(w: &mut Writer, v: &View, cfg: &Config) {
    let net = &v.net;
    let pts: Vec<(&String, crate::net::Pos)> = net
        .sites
        .iter()
        .filter_map(|s| Some((s, *net.pos.get(s)?)))
        .collect();
    let Some(lat_min) = pts.iter().map(|p| p.1.lat).reduce(f64::min) else {
        return;
    };
    let lat_max = pts.iter().map(|p| p.1.lat).fold(lat_min, f64::max);
    let lon_min = pts.iter().map(|p| p.1.lon).fold(f64::INFINITY, f64::min);
    let lon_max = pts
        .iter()
        .map(|p| p.1.lon)
        .fold(f64::NEG_INFINITY, f64::max);
    let k = ((lat_min + lat_max) / 2.0).to_radians().cos();
    let (bw, bh) = (pdf::W - 2.0 * pdf::MARGIN, 300.0);
    let pad = 24.0;
    let sx = (bw - 2.0 * pad - 90.0) / ((lon_max - lon_min) * k).max(1e-3);
    let sy = (bh - 2.0 * pad) / (lat_max - lat_min).max(1e-3);
    let s = sx.min(sy);
    w.room(bh + 10.0);
    // centred across the box, labels included
    let used = ((lon_max - lon_min) * k * s + 2.0 * pad + 90.0).min(bw);
    let (x0, y0) = (pdf::MARGIN + (bw - used) / 2.0, w.y - bh);
    let xy = |p: crate::net::Pos| {
        (
            x0 + pad + (p.lon - lon_min) * k * s,
            y0 + pad + (p.lat - lat_min) * s,
        )
    };
    let bridges: BTreeSet<_> = net.topo.bridges.iter().map(|(e, _)| e).collect();
    let page: &mut Page = w.page();
    page.gray(0.0);
    page.rect((pdf::MARGIN, y0), (bw, bh));
    for p in &net.pairs {
        if let (Some(a), Some(b)) = (net.pos.get(&p.a), net.pos.get(&p.b)) {
            let thick = if bridges.contains(&p.edge()) {
                2.5
            } else {
                1.0
            };
            page.gray(if p.state == LinkState::Down { 0.6 } else { 0.2 });
            page.line(xy(*a), xy(*b), thick, p.state != LinkState::Up);
        }
    }
    page.gray(0.0);
    for (site, p) in &pts {
        let (x, y) = xy(*p);
        page.dot((x, y), 3.5);
        page.text(x + 6.0, y - 3.0, 8.0, false, &cfg.label(site));
    }
    w.y = y0 - 8.0;
    w.para(
        "Thick lines are links that are single points of failure; dashed lines are links \
         with no fresh data (light) or down (lighter).",
        8.0,
        false,
    );
}

fn candidate(w: &mut Writer, cfg: &Config, i: usize, r: &Ranked) {
    let l = |n: &str| cfg.label(n);
    w.room(60.0);
    w.para(&format!("{}. {}", i + 1, r.cand.name), 11.0, true);
    let mut facts = vec![format!("{:.4}, {:.4}", r.cand.lat, r.cand.lon)];
    facts.extend(r.cand.access.map(|a| a.word().to_string()));
    facts.extend(r.cand.power.map(|p| p.word().to_string()));
    facts.extend(r.cand.lease.map(|x| x.word().to_string()));
    if !r.cand.owner.is_empty() {
        facts.push(format!("owner {}", r.cand.owner));
    }
    w.para(&facts.join("; "), 9.0, false);
    let names = |s: &BTreeSet<String>| list(&s.iter().map(|n| l(n)).collect::<Vec<_>>());
    w.para(
        &if r.protected.is_empty() {
            "Protects no site from a single failure.".to_string()
        } else {
            format!("Protects {} from any single failure.", names(&r.protected))
        },
        10.0,
        false,
    );
    w.para(
        &format!(
            "Removes {} single point{} of failure.",
            r.removed(),
            if r.removed() == 1 { "" } else { "s" }
        ),
        10.0,
        false,
    );
    for link in &r.links {
        let km = link
            .km
            .map_or("position unknown".to_string(), |k| format!("{k:.1} km"));
        let pred = match &link.guess {
            Some(g) => format!(
                "{}, {}{}",
                g.pred.mode_word(),
                g.words(),
                if link.counted {
                    ""
                } else {
                    "; too weak, not counted"
                }
            ),
            None => "no terrain prediction".into(),
        };
        w.para(
            &format!("Link to {}: {km}; {pred}.", l(&link.site)),
            9.0,
            false,
        );
    }
}

/// The whole report as a PDF file.
pub fn build(v: &View, cfg: &Config, ranked: &[Ranked]) -> Vec<u8> {
    let net = &v.net;
    let l = |n: &str| cfg.label(n);
    let mut w = Writer::default();
    w.para("Mesh plan", 20.0, true);
    w.para(
        &format!(
            "Generated {} by ridgewarden-planner from the live mesh, judged from {}.",
            short_time(net.now),
            net.anchor()
                .map(l)
                .unwrap_or_else(|| "the largest part of the mesh".into())
        ),
        9.0,
        false,
    );
    let up = net
        .pairs
        .iter()
        .filter(|p| p.state == LinkState::Up)
        .count();
    let spofs = Spofs::of(&net.topo.graph, None);
    let exposed = spofs.exposed();
    w.heading("The mesh now");
    w.para(
        &format!(
            "{} sites and {} links, {up} of them up now. {} sites and {} links are single \
             points of failure. {} of the {} sites could be cut off by one failure{}.",
            net.sites.len(),
            net.pairs.len(),
            spofs.sites.len(),
            spofs.links.len(),
            exposed.len(),
            net.sites.len(),
            if exposed.is_empty() {
                String::new()
            } else {
                format!(
                    ": {}",
                    list(&exposed.iter().map(|n| l(n)).collect::<Vec<_>>())
                )
            }
        ),
        10.0,
        false,
    );
    map(&mut w, v, cfg);

    w.heading("Single points of failure");
    let scenarios: Vec<Scenario> = net
        .topo
        .aps
        .iter()
        .map(|(s, _)| Scenario::default().toggle_site(s))
        .chain(
            net.topo
                .bridges
                .iter()
                .map(|(e, _)| Scenario::default().toggle_link(e)),
        )
        .collect();
    if scenarios.is_empty() {
        w.para(
            "No single site or link failure would cut anyone off.",
            10.0,
            false,
        );
    }
    for sc in &scenarios {
        let s = impact::impact(net, sc).sentences(sc, &l).join(" ");
        w.para(&s, 10.0, false);
    }

    w.heading("Predictions against measurements");
    match (&v.cal.mesh, &v.cal.loo) {
        (Some(c), loo) => {
            w.para(
                &format!(
                    "Terrain predictions (ITM, Longley-Rice) for {} measured links were {:.1} dB {} \
                     typically{}, over {} signal readings.",
                    c.links,
                    c.db.abs(),
                    if c.db < 0.0 { "too optimistic" } else { "too pessimistic" },
                    c.sd.map_or(String::new(), |sd| format!(
                        ", give or take {sd:.1} dB from link to link"
                    )),
                    c.samples
                ),
                10.0,
                false,
            );
            if let Some(loo) = loo {
                w.para(&loo.words(), 10.0, false);
            }
        }
        (None, _) => w.para(
            "No measured link has terrain coverage yet, so predictions are not calibrated.",
            10.0,
            false,
        ),
    }
    for lc in &v.cal.links {
        w.para(
            &format!(
                "{} - {}: {:.1} km, {}; measured {:.0} dB {} the prediction.",
                l(&lc.a),
                l(&lc.b),
                lc.pred.km,
                lc.pred.mode_word(),
                lc.residual.abs(),
                if lc.residual < 0.0 { "below" } else { "above" }
            ),
            9.0,
            false,
        );
    }

    w.heading("Candidate sites");
    if ranked.is_empty() {
        w.para("No candidate sites are configured.", 10.0, false);
    }
    for (i, r) in ranked.iter().enumerate() {
        candidate(&mut w, cfg, i, r);
    }
    w.heading("Limits");
    w.para(
        "Links with no fresh data are assumed up. Links between sites nobody polls are \
         invisible. Predictions use the configured transmit power, antenna gains and heights, \
         and terrain tiles that include trees and buildings; calibration corrects them on \
         average, and the spread says how far a single link may still be off.",
        9.0,
        false,
    );
    w.finish("Mesh plan")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Candidate, EXAMPLE};
    use crate::net::tests::demo_net;
    use crate::predict::Calibration;

    #[test]
    fn report_is_a_valid_pdf_with_the_findings() {
        let cfg: Config = toml::from_str(EXAMPLE).unwrap();
        let v = View {
            net: demo_net(),
            measured: vec![],
            cal: Calibration::default(),
        };
        let ranked = crate::rank::rank(
            &v.net,
            vec![(
                Candidate::at(
                    "Hill".into(),
                    35.25,
                    -120.55,
                    vec!["tst-lopez".into(), "tst-paso".into()],
                ),
                false,
            )],
            None,
        );
        let s = crate::pdf::tests::check(&build(&v, &cfg, &ranked));
        assert!(s.contains("(Mesh plan) Tj"));
        assert!(s.contains("Losing tst-cuesta cuts tst-lopez off from tst-shack."));
        assert!(s.contains("(1. Hill) Tj"));
        assert!(s.contains("not calibrated"));
        assert!(s.contains(" c f"), "the map draws the sites");
    }
}
