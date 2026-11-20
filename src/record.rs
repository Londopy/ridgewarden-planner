//! What each RF link measures against how long it is: the start of a calibration record.
//! Free-space path loss is shown as a baseline only; nothing here predicts a path.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use anyhow::Result;
use ridgewarden_client::graph::edge;
use ridgewarden_client::util::{esc, median};
use ridgewarden_linkdoctor::history::{DAY, quantile};
use ridgewarden_linkdoctor::links::WEAK_SNR;
use rusqlite::{Connection, params};

use crate::geo;
use crate::net::Pos;

pub const WINDOW: i64 = 7 * DAY;

/// One end's medians over the window: what `node` hears from `neighbor`.
#[derive(Clone, Debug, PartialEq)]
pub struct End {
    pub node: String,
    pub neighbor: String,
    /// readings with an SNR
    pub n: usize,
    pub snr: Option<f64>,
    pub signal: Option<f64>,
    /// readings with a signal level, and half their 10th to 90th percentile range
    pub signal_n: usize,
    pub spread: Option<f64>,
    pub noise: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Measured {
    pub a: String,
    pub b: String,
    pub km: Option<f64>,
    pub fspl: Option<f64>,
    pub ends: Vec<End>,
}

#[derive(Default)]
struct Cols {
    snr: Vec<f64>,
    signal: Vec<f64>,
    noise: Vec<f64>,
}

/// Every RF link heard in the last week, shortest first, with its length when both ends
/// have a position.
pub fn measured(
    conn: &Connection,
    now: i64,
    pos: &BTreeMap<String, Pos>,
    mhz: f64,
) -> Result<Vec<Measured>> {
    let mut st = conn.prepare(
        "SELECT node, neighbor, snr, signal, noise FROM link_samples
         WHERE ts > ?1 AND coalesce(link_type, 'RF') = 'RF'",
    )?;
    let mut by_end: BTreeMap<(String, String), Cols> = BTreeMap::new();
    for r in st.query_map(params![now - WINDOW], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<f64>>(2)?,
            r.get::<_, Option<f64>>(3)?,
            r.get::<_, Option<f64>>(4)?,
        ))
    })? {
        let (node, nb, snr, signal, noise) = r?;
        // readings from other stations; anything outside these is a broken report
        let e = by_end.entry((node, nb)).or_default();
        e.snr.extend(snr.filter(|v| (-50.0..=100.0).contains(v)));
        e.signal
            .extend(signal.filter(|v| (-150.0..=30.0).contains(v)));
        e.noise
            .extend(noise.filter(|v| (-150.0..=-30.0).contains(v)));
    }
    let mut links: BTreeMap<(String, String), Vec<End>> = BTreeMap::new();
    for ((node, neighbor), mut c) in by_end {
        let spread = match (quantile(&mut c.signal, 0.1), quantile(&mut c.signal, 0.9)) {
            (Some(lo), Some(hi)) => Some((hi - lo) / 2.0),
            _ => None,
        };
        links.entry(edge(&node, &neighbor)).or_default().push(End {
            n: c.snr.len(),
            snr: median(&mut c.snr),
            signal_n: c.signal.len(),
            signal: median(&mut c.signal),
            spread,
            noise: median(&mut c.noise),
            node,
            neighbor,
        });
    }
    let mut out: Vec<Measured> = links
        .into_iter()
        .map(|((a, b), ends)| {
            let km = match (pos.get(&a), pos.get(&b)) {
                (Some(x), Some(y)) => Some(geo::distance_km(*x, *y)),
                _ => None,
            };
            Measured {
                fspl: km.filter(|k| *k > 0.0).map(|k| geo::fspl_db(k, mhz)),
                km,
                a,
                b,
                ends,
            }
        })
        .collect();
    out.sort_by(|x, y| {
        x.km.unwrap_or(f64::INFINITY)
            .total_cmp(&y.km.unwrap_or(f64::INFINITY))
            .then_with(|| (&x.a, &x.b).cmp(&(&y.a, &y.b)))
    });
    Ok(out)
}

const W: f64 = 640.0;
const H: f64 = 240.0;
const LEFT: f64 = 46.0;
const RIGHT: f64 = 10.0;
const TOP: f64 = 12.0;
const BOTTOM: f64 = 30.0;

/// SNR against distance, one dot per link end, as inline SVG.
pub fn scatter(links: &[Measured], label: &dyn Fn(&str) -> String) -> String {
    let pts: Vec<(f64, f64, String)> = links
        .iter()
        .filter_map(|m| Some((m.km?, m)))
        .flat_map(|(km, m)| {
            m.ends.iter().filter_map(move |e| {
                let snr = e.snr?;
                Some((
                    km,
                    snr,
                    format!(
                        "{} hears {}: {snr:.0} dB at {km:.1} km ({} samples)",
                        label(&e.node),
                        label(&e.neighbor),
                        e.n
                    ),
                ))
            })
        })
        .collect();
    let mut s = String::from(
        "<svg class=chart viewBox=\"0 0 640 240\" role=img aria-label=\"SNR against link length\"><title>SNR against link length</title>",
    );
    if pts.is_empty() {
        let _ = write!(
            s,
            "<text x=\"{}\" y=\"{}\" class=axis text-anchor=middle>no RF links with both ends located yet</text></svg>",
            W / 2.0,
            H / 2.0
        );
        return s;
    }
    let max_km = pts.iter().map(|p| p.0).fold(1.0_f64, f64::max) * 1.05;
    let lo = pts.iter().map(|p| p.1).fold(WEAK_SNR, f64::min) - 3.0;
    let hi = pts.iter().map(|p| p.1).fold(WEAK_SNR, f64::max) + 3.0;
    let x = |k: f64| LEFT + k / max_km * (W - LEFT - RIGHT);
    let y = |v: f64| TOP + (hi - v) / (hi - lo) * (H - TOP - BOTTOM);
    let _ = write!(
        s,
        "<rect class=frame x=\"{LEFT}\" y=\"{TOP}\" width=\"{}\" height=\"{}\"/>",
        W - LEFT - RIGHT,
        H - TOP - BOTTOM
    );
    for v in [hi - 3.0, (hi + lo) / 2.0, lo + 3.0] {
        let _ = write!(
            s,
            "<text class=axis x=\"{}\" y=\"{:.1}\" text-anchor=end>{v:.0} dB</text>",
            LEFT - 4.0,
            y(v) + 4.0
        );
    }
    // ticks on round numbers: 1, 2 or 5 times a power of ten, three to ten of them
    let mut step = 10f64.powf(max_km.log10().floor());
    if max_km / step < 3.0 {
        step /= 2.0;
    }
    if max_km / step < 3.0 {
        step /= 2.5;
    }
    let digits = usize::from(step < 1.0);
    let ticks = (0..).map(|i| i as f64 * step).take_while(|k| *k <= max_km);
    for k in ticks {
        let _ = write!(
            s,
            "<text class=axis x=\"{:.1}\" y=\"{}\" text-anchor=middle>{k:.digits$} km</text>",
            x(k),
            H - 10.0
        );
    }
    let _ = write!(
        s,
        "<line class=rule x1=\"{LEFT}\" x2=\"{}\" y1=\"{y:.1}\" y2=\"{y:.1}\"><title>{WEAK_SNR:.0} dB: weak below this</title></line>",
        W - RIGHT,
        y = y(WEAK_SNR)
    );
    for (k, v, t) in pts {
        let _ = write!(
            s,
            "<circle class=d1 cx=\"{:.1}\" cy=\"{:.1}\" r=\"4\"><title>{}</title></circle>",
            x(k),
            y(v),
            esc(&t)
        );
    }
    s.push_str("</svg>");
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::tests::{MIN, NOW, fake};
    use serde_json::json;

    #[test]
    fn medians_per_end_with_distance_and_baseline() {
        let mut l = fake();
        for (i, snr) in [20.0, 22.0, 40.0].into_iter().enumerate() {
            l.add(
                "node.poll",
                json!({"node": "a", "ok": true, "links": [
                    {"neighbor": "b", "type": "RF", "snr": snr, "signal": -70.0 - i as f64},
                    {"neighbor": "c", "type": "DTD", "quality": 100.0}
                ]}),
                NOW - (10 - i as i64) * MIN,
            );
        }
        l.add(
            "node.poll",
            json!({"node": "b", "ok": true, "links": [{"neighbor": "a", "snr": 18.0}]}),
            NOW - MIN,
        );
        // an old sample outside the window doesn't count
        l.add(
            "node.poll",
            json!({"node": "b", "ok": true, "links": [{"neighbor": "a", "snr": 50.0}]}),
            NOW - 8 * DAY,
        );
        let pos: BTreeMap<String, Pos> = [
            (
                "a".to_string(),
                Pos {
                    lat: 35.0,
                    lon: -120.0,
                },
            ),
            (
                "b".to_string(),
                Pos {
                    lat: 35.1,
                    lon: -120.0,
                },
            ),
        ]
        .into();
        let m = measured(&l.conn, NOW, &pos, 5800.0).unwrap();
        assert_eq!(m.len(), 1, "the DtD link isn't an RF path");
        let ab = &m[0];
        assert!((ab.km.unwrap() - 11.12).abs() < 0.01);
        assert!((ab.fspl.unwrap() - geo::fspl_db(ab.km.unwrap(), 5800.0)).abs() < 1e-9);
        assert_eq!(
            ab.ends[0],
            End {
                node: "a".into(),
                neighbor: "b".into(),
                n: 3,
                snr: Some(22.0),
                signal: Some(-71.0),
                signal_n: 3,
                spread: Some(1.0),
                noise: None,
            }
        );
        assert_eq!((ab.ends[1].n, ab.ends[1].snr), (1, Some(18.0)));
        let svg = scatter(&m, &|n| n.to_uppercase());
        assert_eq!(svg.matches("<circle").count(), 2);
        assert!(svg.contains("A hears B: 22 dB at 11.1 km (3 samples)"));
        assert!(svg.contains(">2 km<") && svg.contains(">10 km<") && !svg.contains(">12 km<"));
    }
}
