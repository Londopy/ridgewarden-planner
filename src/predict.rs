//! Path predictions from terrain and ITM, and their calibration against what the live
//! links measure.
//!
//! A prediction is a received signal level: transmit power and antenna gains from the
//! config, minus ITM's basic transmission loss over the terrain profile. Every RF link the
//! mesh reports gives a measured signal level for the same path, so the difference
//! between the two (the residual) is a free measurement of how wrong the prediction is
//! here. The median residual over the links in an area is the correction applied to a new
//! path there, and the spread of the residuals is its uncertainty. The correction also
//! absorbs wrong guesses about power, gains and cable loss, which is the point.

use std::collections::BTreeMap;

use anyhow::Result;
use ridgewarden_client::util::median;

use crate::config::{Config, Radio, RadioCfg};
use crate::geo;
use crate::itm::{self, Mode};
use crate::net::{Net, Pos};
use crate::record::Measured;
use crate::terrain::Terrain;

/// Shortest step between profile points; about one sample of a 1 arc-second tile.
const MIN_STEP_M: f64 = 30.0;
/// Profiles have at most this many intervals.
const STEPS: f64 = 600.0;
/// A region needs this many calibrated links to get its own correction.
const MIN_REGION_LINKS: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Prediction {
    pub km: f64,
    /// basic transmission loss, dB
    pub loss_db: f64,
    /// None below ITM's 1 km limit, where only free-space loss is used
    pub mode: Option<Mode>,
    /// antenna heights above the terrain at each end
    pub h_a: f64,
    pub h_b: f64,
}

impl Prediction {
    /// Received level at `rx` from `tx`, dBm.
    pub fn signal(&self, tx: &Radio, rx: &Radio) -> f64 {
        tx.tx_dbm + tx.gain_dbi - tx.cable_db + rx.gain_dbi - rx.cable_db - self.loss_db
    }

    pub fn mode_word(&self) -> &'static str {
        self.mode
            .map_or("under 1 km: free space only", |m| m.word())
    }
}

/// Antenna height above the terrain: a GPS altitude, made relative to the geoid and the
/// tile's surface, or the configured height above ground.
fn agl(r: &Radio, geoid_m: Option<f64>, ground: f64) -> f64 {
    match (r.alt_m, geoid_m) {
        (Some(alt), Some(n)) => (alt - n - ground).max(0.5),
        _ => r.height_m,
    }
}

/// Predict the path from `a` to `b`. None when the terrain doesn't cover it.
pub fn path(
    terrain: &Terrain,
    cfg: &RadioCfg,
    (a, ra): (Pos, &Radio),
    (b, rb): (Pos, &Radio),
) -> Result<Option<Prediction>> {
    let km = geo::distance_km(a, b);
    let step = (km * 1000.0 / STEPS).max(MIN_STEP_M);
    let Some((xi, heights)) = terrain.profile(a, b, step) else {
        return Ok(None);
    };
    let h_a = agl(ra, cfg.geoid_m, heights[0]);
    let h_b = agl(rb, cfg.geoid_m, heights[heights.len() - 1]);
    if km < 1.0 {
        return Ok(Some(Prediction {
            km,
            loss_db: geo::fspl_db(km.max(0.001), ra.band_mhz),
            mode: None,
            h_a,
            h_b,
        }));
    }
    let loss = itm::p2p(h_a, h_b, &itm::pfl(xi, &heights), &cfg.itm(ra.band_mhz))?;
    Ok(Some(Prediction {
        km,
        loss_db: loss.db,
        mode: Some(loss.mode),
        h_a,
        h_b,
    }))
}

/// One end of a measured link against its prediction.
#[derive(Clone, Debug, PartialEq)]
pub struct EndCal {
    /// the receiving node
    pub rx: String,
    pub tx: String,
    pub samples: usize,
    pub measured: f64,
    pub spread: Option<f64>,
    pub predicted: f64,
    /// measured minus predicted: positive when the path is better than ITM thinks
    pub residual: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LinkCal {
    pub a: String,
    pub b: String,
    pub pred: Prediction,
    pub ends: Vec<EndCal>,
    /// the mean of the ends' residuals
    pub residual: f64,
    pub mid: Pos,
}

/// The correction for one area, from the links measured there.
#[derive(Clone, Debug, PartialEq)]
pub struct Correction {
    /// None for the whole mesh
    pub region: Option<String>,
    /// add this to a prediction
    pub db: f64,
    /// how far one link's residual typically is from `db`: 1.4826 times the median absolute
    /// deviation, which matches the standard deviation for well-behaved data; None with a
    /// single link
    pub sd: Option<f64>,
    pub links: usize,
    pub samples: usize,
}

/// How much calibration shrinks the error, each link predicted from the others only.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Loo {
    pub links: usize,
    pub rms_raw: f64,
    pub rms_calibrated: f64,
}

impl Loo {
    pub fn words(&self) -> String {
        if self.rms_calibrated < self.rms_raw {
            format!(
                "Calibration takes the typical error from {:.1} dB to {:.1} dB, predicting each of \
                 the {} links from the others only.",
                self.rms_raw, self.rms_calibrated, self.links
            )
        } else {
            format!(
                "Calibration doesn't help yet: predicting each of the {} links from the others \
                 only, the typical error is {:.1} dB without it and {:.1} dB with it. The links \
                 disagree more than they share an offset; check the radio settings per site.",
                self.links, self.rms_raw, self.rms_calibrated
            )
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Calibration {
    pub links: Vec<LinkCal>,
    /// RF links measured but not predicted, and why
    pub skipped: Vec<(String, String, &'static str)>,
    pub mesh: Option<Correction>,
    pub regions: Vec<Correction>,
    pub loo: Option<Loo>,
    /// measured noise floor per site
    pub noise: BTreeMap<String, f64>,
}

fn correction(region: Option<&str>, links: &[&LinkCal]) -> Option<Correction> {
    if links.is_empty() {
        return None;
    }
    // medians, so one badly predicted path (a ridge ITM overrates) can't move them all
    let mut r: Vec<f64> = links.iter().map(|l| l.residual).collect();
    let mid = median(&mut r)?;
    let sd = if links.len() > 1 {
        let mut dev: Vec<f64> = r.iter().map(|x| (x - mid).abs()).collect();
        median(&mut dev).map(|mad| 1.4826 * mad)
    } else {
        None
    };
    Some(Correction {
        region: region.map(str::to_string),
        db: mid,
        sd,
        links: links.len(),
        samples: links
            .iter()
            .flat_map(|l| l.ends.iter().map(|e| e.samples))
            .sum(),
    })
}

/// The correction that applies at `at`: its region's, when that has enough links, else
/// the whole mesh's.
fn correction_at(cfg: &Config, links: &[&LinkCal], at: Pos) -> Option<Correction> {
    if let Some(r) = cfg.regions.iter().find(|r| r.contains(at.lat, at.lon)) {
        let inside: Vec<&LinkCal> = links
            .iter()
            .copied()
            .filter(|l| r.contains(l.mid.lat, l.mid.lon))
            .collect();
        if inside.len() >= MIN_REGION_LINKS {
            return correction(Some(&r.name), &inside);
        }
    }
    correction(None, links)
}

fn midpoint(a: Pos, b: Pos) -> Pos {
    Pos {
        lat: (a.lat + b.lat) / 2.0,
        lon: (a.lon + b.lon) / 2.0,
    }
}

fn site_noise(measured: &[Measured]) -> BTreeMap<String, f64> {
    let mut by: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    for e in measured.iter().flat_map(|m| &m.ends) {
        if let Some(n) = e.noise {
            by.entry(e.node.clone()).or_default().push(n);
        }
    }
    by.into_iter()
        .filter_map(|(k, mut v)| Some((k, median(&mut v)?)))
        .collect()
}

/// Predict every measured RF link and fit the corrections.
pub fn calibrate(net: &Net, measured: &[Measured], terrain: &Terrain, cfg: &Config) -> Calibration {
    let mut cal = Calibration {
        noise: site_noise(measured),
        ..Default::default()
    };
    for m in measured {
        let (Some(pa), Some(pb)) = (net.pos.get(&m.a), net.pos.get(&m.b)) else {
            cal.skipped
                .push((m.a.clone(), m.b.clone(), "a position is missing"));
            continue;
        };
        let (ra, rb) = (cfg.radio.site(&m.a), cfg.radio.site(&m.b));
        let pred = match path(terrain, &cfg.radio, (*pa, &ra), (*pb, &rb)) {
            Ok(Some(p)) => p,
            Ok(None) => {
                cal.skipped
                    .push((m.a.clone(), m.b.clone(), "no terrain for the path"));
                continue;
            }
            Err(e) => {
                tracing::debug!("predicting {} - {}: {e:#}", m.a, m.b);
                cal.skipped
                    .push((m.a.clone(), m.b.clone(), "outside ITM's range"));
                continue;
            }
        };
        let ends: Vec<EndCal> = m
            .ends
            .iter()
            .filter_map(|e| {
                let measured = e.signal?;
                let (tx, rx) = if e.node == m.a {
                    (&rb, &ra)
                } else {
                    (&ra, &rb)
                };
                let predicted = pred.signal(tx, rx);
                Some(EndCal {
                    rx: e.node.clone(),
                    tx: e.neighbor.clone(),
                    samples: e.signal_n,
                    measured,
                    spread: e.spread,
                    predicted,
                    residual: measured - predicted,
                })
            })
            .collect();
        if ends.is_empty() {
            cal.skipped
                .push((m.a.clone(), m.b.clone(), "no signal readings"));
            continue;
        }
        cal.links.push(LinkCal {
            residual: ends.iter().map(|e| e.residual).sum::<f64>() / ends.len() as f64,
            mid: midpoint(*pa, *pb),
            a: m.a.clone(),
            b: m.b.clone(),
            pred,
            ends,
        });
    }
    let all: Vec<&LinkCal> = cal.links.iter().collect();
    cal.mesh = correction(None, &all);
    cal.regions = cfg
        .regions
        .iter()
        .filter_map(|r| {
            let inside: Vec<&LinkCal> = all
                .iter()
                .copied()
                .filter(|l| r.contains(l.mid.lat, l.mid.lon))
                .collect();
            correction(Some(&r.name), &inside)
        })
        .collect();
    cal.loo = leave_one_out(cfg, &all);
    cal
}

fn all_but<'a>(links: &[&'a LinkCal], i: usize) -> Vec<&'a LinkCal> {
    links
        .iter()
        .enumerate()
        .filter(|(j, _)| *j != i)
        .map(|(_, x)| *x)
        .collect()
}

fn leave_one_out(cfg: &Config, links: &[&LinkCal]) -> Option<Loo> {
    if links.len() < 3 {
        return None;
    }
    let (mut raw, mut fixed) = (0.0, 0.0);
    for (i, l) in links.iter().enumerate() {
        let c = correction_at(cfg, &all_but(links, i), l.mid)?;
        raw += l.residual.powi(2);
        fixed += (l.residual - c.db).powi(2);
    }
    let n = links.len() as f64;
    Some(Loo {
        links: links.len(),
        rms_raw: (raw / n).sqrt(),
        rms_calibrated: (fixed / n).sqrt(),
    })
}

/// A predicted link that doesn't exist yet, from a candidate place to a site.
#[derive(Clone, Debug, PartialEq)]
pub struct Guess {
    pub pred: Prediction,
    /// the weaker direction's SNR, corrected when there is a correction
    pub snr: f64,
    pub correction: Option<Correction>,
    pub workable: bool,
}

impl Guess {
    /// "SNR 18 dB +/- 4, calibrated on 6 links (300 samples)", in plain ASCII.
    pub fn words(&self) -> String {
        match &self.correction {
            Some(c) => {
                let pm = c.sd.map_or(String::new(), |sd| format!(" +/- {sd:.0}"));
                let area = c
                    .region
                    .as_ref()
                    .map_or(String::new(), |r| format!(" in {r}"));
                format!(
                    "SNR {:.0} dB{pm}, calibrated on {} link{}{area} ({} samples)",
                    self.snr,
                    c.links,
                    if c.links == 1 { "" } else { "s" },
                    c.samples
                )
            }
            None => format!(
                "SNR {:.0} dB, uncalibrated: no measured link has terrain yet, and ITM alone is often off by 10 dB or more",
                self.snr
            ),
        }
    }
}

impl Calibration {
    /// The correction for measured link `i`, fitted on the other links only, so its own
    /// measurement can't make the prediction look right.
    pub fn correction_without(&self, cfg: &Config, i: usize) -> Option<Correction> {
        let links: Vec<&LinkCal> = self.links.iter().collect();
        correction_at(cfg, &all_but(&links, i), self.links.get(i)?.mid)
    }

    /// Predict a new link from a place (with `ra`) to an existing site.
    pub fn guess(
        &self,
        terrain: &Terrain,
        cfg: &Config,
        (at, ra): (Pos, &Radio),
        (site, pos): (&str, Pos),
    ) -> Result<Option<Guess>> {
        let rb = cfg.radio.site(site);
        let Some(pred) = path(terrain, &cfg.radio, (at, ra), (pos, &rb))? else {
            return Ok(None);
        };
        let links: Vec<&LinkCal> = self.links.iter().collect();
        let correction = correction_at(cfg, &links, midpoint(at, pos));
        let fix = correction.as_ref().map_or(0.0, |c| c.db);
        let noise_site = self.noise.get(site).copied().unwrap_or(cfg.radio.noise_dbm);
        let at_site = pred.signal(ra, &rb) + fix - noise_site;
        let at_place = pred.signal(&rb, ra) + fix - cfg.radio.noise_dbm;
        let snr = at_site.min(at_place);
        Ok(Some(Guess {
            pred,
            snr,
            workable: snr >= cfg.radio.min_snr_db,
            correction,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::EXAMPLE;
    use crate::net::tests::demo_net;
    use crate::record::End;
    use crate::terrain::tests::synthetic;

    fn cfg() -> Config {
        toml::from_str(EXAMPLE).unwrap()
    }

    /// Flat terrain at 100 m over N35W121 and N35W122.
    fn flat() -> (tempfile::TempDir, Terrain) {
        let dir = tempfile::tempdir().unwrap();
        let t = Terrain::new(dir.path());
        for lon in [-121, -122] {
            t.store(35, lon, &synthetic(|_, _| 100)).unwrap();
        }
        (dir, t)
    }

    fn end(node: &str, nb: &str, signal: f64) -> End {
        End {
            node: node.into(),
            neighbor: nb.into(),
            n: 100,
            snr: Some(signal + 95.0),
            signal: Some(signal),
            signal_n: 100,
            spread: Some(2.0),
            noise: Some(-95.0),
        }
    }

    /// Every demo link measured `offset` dB better than predicted, plus a little per-link
    /// scatter so the spread is real.
    fn measured(net: &Net, t: &Terrain, cfg: &Config, offset: f64) -> Vec<Measured> {
        net.pairs
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let (ra, rb) = (cfg.radio.site(&p.a), cfg.radio.site(&p.b));
                let pred = path(t, &cfg.radio, (net.pos[&p.a], &ra), (net.pos[&p.b], &rb))
                    .unwrap()
                    .unwrap();
                let jitter = (i % 3) as f64 - 1.0;
                let s = pred.signal(&ra, &rb) + offset + jitter;
                Measured {
                    a: p.a.clone(),
                    b: p.b.clone(),
                    km: Some(pred.km),
                    fspl: None,
                    ends: vec![end(&p.a, &p.b, s), end(&p.b, &p.a, s)],
                }
            })
            .collect()
    }

    #[test]
    fn heights_from_gps_altitude_use_the_geoid_and_the_ground() {
        let mut r = cfg().radio.default_radio();
        assert_eq!(
            agl(&r, Some(-33.0), 400.0),
            10.0,
            "no altitude: height above ground"
        );
        r.alt_m = Some(400.0);
        // 400 m above the ellipsoid is 433 m above the geoid, 33 m above ground at 400 m
        assert_eq!(agl(&r, Some(-33.0), 400.0), 33.0);
    }

    #[test]
    fn calibration_learns_the_offset_and_shrinks_the_error() {
        let net = demo_net();
        let (_d, t) = flat();
        let cfg = cfg();
        let m = measured(&net, &t, &cfg, -12.0);
        let cal = calibrate(&net, &m, &t, &cfg);
        assert_eq!(cal.links.len(), net.pairs.len());
        let c = cal.mesh.as_ref().unwrap();
        assert!((c.db + 12.0).abs() < 0.5, "{}", c.db);
        assert!(c.sd.unwrap() < 2.0);
        let loo = cal.loo.unwrap();
        assert!(loo.rms_raw > 11.0, "{loo:?}");
        assert!(loo.rms_calibrated < 2.0, "{loo:?}");
        assert!(
            loo.words()
                .starts_with("Calibration takes the typical error from")
        );
        let worse = Loo {
            links: 4,
            rms_raw: 5.0,
            rms_calibrated: 6.0,
        };
        assert!(worse.words().starts_with("Calibration doesn't help yet"));
    }

    #[test]
    fn a_new_link_gets_the_correction_and_its_uncertainty() {
        let net = demo_net();
        let (_d, t) = flat();
        let cfg = cfg();
        let cal = calibrate(&net, &measured(&net, &t, &cfg, -12.0), &t, &cfg);
        let place = Pos {
            lat: 35.45,
            lon: -120.6,
        };
        let r = cfg.radio.default_radio();
        let g = cal
            .guess(&t, &cfg, (place, &r), ("tst-paso", net.pos["tst-paso"]))
            .unwrap()
            .unwrap();
        let raw = Calibration::default()
            .guess(&t, &cfg, (place, &r), ("tst-paso", net.pos["tst-paso"]))
            .unwrap()
            .unwrap();
        assert!(
            (raw.snr - g.snr - 12.0).abs() < 0.5,
            "{} vs {}",
            raw.snr,
            g.snr
        );
        assert!(
            g.words().contains("+/- 1, calibrated on 7 links"),
            "{}",
            g.words()
        );
        assert!(raw.words().contains("uncalibrated"));
        let off = Pos {
            lat: 37.0,
            lon: -120.6,
        };
        assert!(
            cal.guess(&t, &cfg, (off, &r), ("tst-paso", net.pos["tst-paso"]))
                .unwrap()
                .is_none(),
            "no terrain there"
        );
    }

    #[test]
    fn a_region_with_enough_links_gets_its_own_correction() {
        let net = demo_net();
        let (_d, t) = flat();
        let mut cfg = cfg();
        // the north of the demo mesh holds one link: too few to stand alone
        cfg.regions = vec![crate::config::Region {
            name: "North".into(),
            lat_min: 35.5,
            lat_max: 36.0,
            lon_min: -121.0,
            lon_max: -120.0,
        }];
        let cal = calibrate(&net, &measured(&net, &t, &cfg, -12.0), &t, &cfg);
        let links: Vec<&LinkCal> = cal.links.iter().collect();
        let c = correction_at(
            &cfg,
            &links,
            Pos {
                lat: 35.6,
                lon: -120.7,
            },
        )
        .unwrap();
        assert_eq!(c.region, None, "falls back to the whole mesh");
        assert_eq!(cal.regions[0].links, 1);
    }
}
