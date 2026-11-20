//! Distances, the free-space baseline, and the GeoJSON export (RFC 7946).

use std::collections::{BTreeMap, BTreeSet};

use ridgewarden_client::graph::Edge;
use ridgewarden_linkdoctor::links::LinkState;
use serde_json::{Value, json};

use crate::config::Config;
use crate::net::{Net, Pos};
use crate::predict::Calibration;

/// Mean earth radius (IUGG), km.
const EARTH_KM: f64 = 6371.0088;

/// Great-circle distance (haversine).
pub fn distance_km(a: Pos, b: Pos) -> f64 {
    let (la1, la2) = (a.lat.to_radians(), b.lat.to_radians());
    let dla = la2 - la1;
    let dlo = (b.lon - a.lon).to_radians();
    let h = (dla / 2.0).sin().powi(2) + la1.cos() * la2.cos() * (dlo / 2.0).sin().powi(2);
    2.0 * EARTH_KM * h.sqrt().asin()
}

/// Free-space path loss in dB: what a perfectly clear path loses to distance alone.
pub fn fspl_db(km: f64, mhz: f64) -> f64 {
    20.0 * km.log10() + 20.0 * mhz.log10() + 32.44
}

pub fn state_word(s: LinkState) -> &'static str {
    match s {
        LinkState::Up => "up",
        LinkState::Down => "down",
        LinkState::Unknown => "unknown",
    }
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

fn point(p: Option<&Pos>) -> Value {
    p.map_or(
        Value::Null,
        |p| json!({"type": "Point", "coordinates": [p.lon, p.lat]}),
    )
}

fn line(a: Option<&Pos>, b: Option<&Pos>) -> Value {
    match (a, b) {
        (Some(a), Some(b)) => {
            json!({"type": "LineString", "coordinates": [[a.lon, a.lat], [b.lon, b.lat]]})
        }
        _ => Value::Null,
    }
}

fn km(a: Option<&Pos>, b: Option<&Pos>) -> Option<f64> {
    Some(round2(distance_km(*a?, *b?)))
}

fn feature(geometry: Value, properties: Value) -> Value {
    json!({"type": "Feature", "geometry": geometry, "properties": properties})
}

/// Sites, links and configured candidates. Anything without a known position is still
/// listed, with a null geometry, so the export is complete.
pub fn geojson(net: &Net, cfg: &Config, cal: &Calibration) -> Value {
    let aps: BTreeMap<&String, &BTreeSet<String>> =
        net.topo.aps.iter().map(|(n, c)| (n, c)).collect();
    let bridges: BTreeMap<&Edge, &BTreeSet<String>> =
        net.topo.bridges.iter().map(|(e, c)| (e, c)).collect();
    let mut out = vec![];
    for s in &net.sites {
        out.push(feature(
            point(net.pos.get(s)),
            json!({
                "kind": "site",
                "node": s,
                "name": cfg.label(s),
                "here": *s == net.here,
                "reports_links": net.observed.contains(s),
                "single_point_of_failure": aps.contains_key(s),
                "cuts_off": aps.get(s).copied().cloned().unwrap_or_default(),
            }),
        ));
    }
    for p in &net.pairs {
        let (a, b) = (net.pos.get(&p.a), net.pos.get(&p.b));
        let e = p.edge();
        out.push(feature(
            line(a, b),
            json!({
                "kind": "link",
                "a": p.a,
                "b": p.b,
                "type": p.link_type,
                "state": state_word(p.state),
                "snr": p.snr(),
                "quality": p.quality(),
                "distance_km": km(a, b),
                "single_point_of_failure": bridges.contains_key(&e),
                "cuts_off": bridges.get(&e).copied().cloned().unwrap_or_default(),
                "measured_minus_predicted_db": cal
                    .links
                    .iter()
                    .find(|l| l.a == p.a && l.b == p.b)
                    .map(|l| round2(l.residual)),
            }),
        ));
    }
    for c in &cfg.candidates {
        let at = Pos {
            lat: c.lat,
            lon: c.lon,
        };
        out.push(feature(
            point(Some(&at)),
            json!({"kind": "candidate", "name": c.name, "note": c.note}),
        ));
        for l in &c.links {
            let to = net.pos.get(l);
            out.push(feature(
                line(Some(&at), to),
                json!({"kind": "proposed", "candidate": c.name, "to": l, "distance_km": km(Some(&at), to)}),
            ));
        }
    }
    json!({"type": "FeatureCollection", "features": out})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Candidate, EXAMPLE};
    use crate::net::tests::demo_net;

    #[test]
    fn distances_match_known_values() {
        // one degree of latitude is about 111.2 km
        let d = distance_km(
            Pos {
                lat: 35.0,
                lon: -120.0,
            },
            Pos {
                lat: 36.0,
                lon: -120.0,
            },
        );
        assert!((d - 111.19).abs() < 0.05, "{d}");
        assert!((fspl_db(1.0, 5800.0) - 107.71).abs() < 0.01);
        // ten times the distance costs 20 dB
        assert!((fspl_db(10.0, 5800.0) - fspl_db(1.0, 5800.0) - 20.0).abs() < 1e-9);
    }

    #[test]
    fn export_is_geojson_with_lon_lat_order() {
        let net = demo_net();
        let mut cfg: Config = toml::from_str(EXAMPLE).unwrap();
        cfg.candidates = vec![Candidate::at(
            "Hill".into(),
            35.25,
            -120.55,
            vec!["tst-lopez".into(), "tst-gone".into()],
        )];
        let g = geojson(&net, &cfg, &Calibration::default());
        assert_eq!(g["type"], "FeatureCollection");
        let f = g["features"].as_array().unwrap();
        let find = |kind: &str, key: &str, val: &str| {
            f.iter()
                .find(|x| x["properties"]["kind"] == kind && x["properties"][key] == val)
                .unwrap()
        };
        let paso = find("site", "node", "tst-paso");
        assert_eq!(paso["geometry"]["coordinates"], json!([-120.69, 35.63]));
        assert_eq!(paso["properties"]["single_point_of_failure"], false);
        let north = find("site", "node", "tst-north");
        assert_eq!(north["properties"]["cuts_off"], json!(["tst-paso"]));
        let link = f
            .iter()
            .find(|x| x["properties"]["a"] == "tst-north" && x["properties"]["b"] == "tst-paso")
            .unwrap();
        assert_eq!(link["geometry"]["type"], "LineString");
        assert_eq!(link["properties"]["state"], "up");
        assert_eq!(link["properties"]["single_point_of_failure"], true);
        assert!(link["properties"]["distance_km"].as_f64().unwrap() > 12.0);
        assert_eq!(
            find("proposed", "to", "tst-gone")["geometry"],
            Value::Null,
            "no position, still listed"
        );
        assert_eq!(
            find("candidate", "name", "Hill")["geometry"]["coordinates"],
            json!([-120.55, 35.25])
        );
    }
}
