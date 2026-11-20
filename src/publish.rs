//! Predictions for the measured RF links, published as `planner.prediction` events so
//! Link Doctor can show predicted against measured on its link pages. Each link's
//! correction is fitted on the other links only, so the link's own measurement can't make
//! its prediction look right. A link is published again only when its prediction moves by
//! more than 1 dB, or once a day, so the main log doesn't fill.

use anyhow::Result;
use ridgewarden_linkdoctor::history::DAY;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::json;

use crate::config::Config;
use crate::predict::{Calibration, LinkCal};

pub const KIND: &str = "planner.prediction";
const MIN_CHANGE_DB: f64 = 1.0;
/// what Link Doctor accepts; a path predicted outside it is not worth a log entry
const SNR_RANGE: std::ops::RangeInclusive<f64> = -20.0..=80.0;

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS published (
    a          TEXT NOT NULL,
    b          TEXT NOT NULL,
    snr        REAL NOT NULL,
    calibrated INTEGER NOT NULL,
    ts         INTEGER NOT NULL,
    PRIMARY KEY (a, b)
);";

#[derive(Clone, Debug, PartialEq)]
pub struct Due {
    pub a: String,
    pub b: String,
    pub snr: f64,
    pub calibrated: bool,
}

/// The weaker end's predicted SNR, corrected by the other links when they allow it.
fn predicted_snr(cal: &Calibration, cfg: &Config, i: usize, lc: &LinkCal) -> Option<(f64, bool)> {
    let fix = cal.correction_without(cfg, i);
    let snr = lc
        .ends
        .iter()
        .map(|e| {
            let noise = cal.noise.get(&e.rx).copied().unwrap_or(cfg.radio.noise_dbm);
            e.predicted + fix.as_ref().map_or(0.0, |c| c.db) - noise
        })
        .reduce(f64::min)?;
    // a correction from one other link has no spread, so it isn't a calibration yet
    let calibrated = fix.is_some_and(|c| c.links >= 2);
    Some(((snr * 10.0).round() / 10.0, calibrated))
}

/// Links whose prediction is new, has moved, or was last published a day ago.
pub fn due(conn: &Connection, cal: &Calibration, cfg: &Config, now: i64) -> Result<Vec<Due>> {
    let mut out = vec![];
    for (i, lc) in cal.links.iter().enumerate() {
        let Some((snr, calibrated)) = predicted_snr(cal, cfg, i, lc) else {
            continue;
        };
        if !SNR_RANGE.contains(&snr) {
            continue;
        }
        let last: Option<(f64, bool, i64)> = conn
            .query_row(
                "SELECT snr, calibrated, ts FROM published WHERE a = ?1 AND b = ?2",
                params![lc.a, lc.b],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let fresh = last.is_some_and(|(s, c, ts)| {
            (s - snr).abs() <= MIN_CHANGE_DB && c == calibrated && now - ts < DAY
        });
        if !fresh {
            out.push(Due {
                a: lc.a.clone(),
                b: lc.b.clone(),
                snr,
                calibrated,
            });
        }
    }
    Ok(out)
}

pub fn payload(d: &Due) -> serde_json::Value {
    json!({"a": d.a, "b": d.b, "snr_db": d.snr, "model": "ITM", "calibrated": d.calibrated})
}

/// Remember what was published, once the core has logged it.
pub fn record(conn: &Connection, d: &Due, now: i64) -> Result<()> {
    conn.execute(
        "INSERT INTO published (a, b, snr, calibrated, ts) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(a, b) DO UPDATE SET snr = excluded.snr, calibrated = excluded.calibrated,
             ts = excluded.ts",
        params![d.a, d.b, d.snr, d.calibrated, now],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::EXAMPLE;
    use crate::net::Pos;
    use crate::predict::{EndCal, Prediction};

    const NOW: i64 = 1_800_000_000_000;

    fn link(a: &str, b: &str, predicted: f64, residual: f64) -> LinkCal {
        let end = |rx: &str, tx: &str| EndCal {
            rx: rx.into(),
            tx: tx.into(),
            samples: 50,
            measured: predicted + residual,
            spread: None,
            predicted,
            residual,
        };
        LinkCal {
            a: a.into(),
            b: b.into(),
            pred: Prediction {
                km: 10.0,
                loss_db: 120.0,
                mode: None,
                h_a: 10.0,
                h_b: 10.0,
            },
            ends: vec![end(a, b), end(b, a)],
            residual,
            mid: Pos {
                lat: 35.3,
                lon: -120.6,
            },
        }
    }

    fn setup() -> (Connection, Calibration, Config) {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        let cal = Calibration {
            links: vec![
                link("a", "b", -70.0, -10.0),
                link("c", "d", -70.0, -12.0),
                link("e", "f", -70.0, -8.0),
            ],
            ..Default::default()
        };
        (conn, cal, toml::from_str(EXAMPLE).unwrap())
    }

    #[test]
    fn each_link_is_corrected_by_the_others_only() {
        let (conn, cal, cfg) = setup();
        let d = due(&conn, &cal, &cfg, NOW).unwrap();
        assert_eq!(d.len(), 3);
        // a-b: -70 dBm predicted, the other two links say 10 dB worse, noise -95
        assert_eq!(d[0].snr, 15.0);
        assert!(d[0].calibrated);
        assert_eq!(payload(&d[0])["model"], "ITM");
        // with a single other link there is no spread yet: published, not called calibrated
        let two = Calibration {
            links: cal.links[..2].to_vec(),
            ..Default::default()
        };
        let d = due(&conn, &two, &cfg, NOW).unwrap();
        assert_eq!((d[0].snr, d[0].calibrated), (13.0, false));
    }

    #[test]
    fn republished_only_when_it_moves_or_a_day_passes() {
        let (conn, mut cal, cfg) = setup();
        for d in due(&conn, &cal, &cfg, NOW).unwrap() {
            record(&conn, &d, NOW).unwrap();
        }
        assert!(due(&conn, &cal, &cfg, NOW + 3_600_000).unwrap().is_empty());
        // a-b's ends now predict 0.5 dB better: within a dB, nothing to say
        for e in &mut cal.links[0].ends {
            e.predicted += 0.5;
        }
        assert!(due(&conn, &cal, &cfg, NOW + 3_600_000).unwrap().is_empty());
        for e in &mut cal.links[0].ends {
            e.predicted += 1.0;
        }
        let d = due(&conn, &cal, &cfg, NOW + 3_600_000).unwrap();
        assert_eq!(d.len(), 1);
        assert_eq!((d[0].a.as_str(), d[0].snr), ("a", 16.5));
        assert_eq!(due(&conn, &cal, &cfg, NOW + DAY).unwrap().len(), 3);
    }

    #[test]
    fn paths_outside_what_link_doctor_takes_are_not_published() {
        let (conn, mut cal, cfg) = setup();
        for e in &mut cal.links[0].ends {
            e.predicted = -200.0;
        }
        let d = due(&conn, &cal, &cfg, NOW).unwrap();
        assert!(d.iter().all(|x| x.a != "a"));
    }
}
