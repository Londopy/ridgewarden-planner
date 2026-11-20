//! Hilltop search: the high points in the terrain around the mesh, each tried as a
//! candidate linked to every site within reach, so ranking can say which would add the
//! most redundancy. Access, power and lease are unknown for these until someone looks.

use crate::config::Candidate;
use crate::geo::distance_km;
use crate::net::{Net, Pos};
use crate::terrain::Terrain;

const SPACING_M: f64 = 250.0;
/// at most this many grid points a side; wider areas get a coarser grid
const MAX_CELLS: usize = 400;
/// a hill is the highest point within this many grid steps
const WINDOW: usize = 4;
const MIN_APART_KM: f64 = 2.0;
/// a hill this close to an existing site is that site
const NEAR_SITE_KM: f64 = 1.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Hill {
    pub pos: Pos,
    pub elev: f64,
}

struct Grid {
    lat0: f64,
    lon0: f64,
    dlat: f64,
    dlon: f64,
    rows: usize,
    cols: usize,
}

impl Grid {
    fn around(points: &[Pos], margin_km: f64) -> Option<Self> {
        let lat_min = points.iter().map(|p| p.lat).reduce(f64::min)?;
        let lat_max = points.iter().map(|p| p.lat).reduce(f64::max)?;
        let lon_min = points.iter().map(|p| p.lon).reduce(f64::min)?;
        let lon_max = points.iter().map(|p| p.lon).reduce(f64::max)?;
        let k = ((lat_min + lat_max) / 2.0).to_radians().cos().max(0.1);
        let (mlat, mlon) = (margin_km / 111.2, margin_km / (111.2 * k));
        let (lat0, lat1) = (lat_min - mlat, lat_max + mlat);
        let (lon0, lon1) = (lon_min - mlon, lon_max + mlon);
        let step = |span_deg: f64, km_per_deg: f64| {
            let cells = (span_deg * km_per_deg * 1000.0 / SPACING_M).ceil() as usize;
            cells.clamp(2, MAX_CELLS)
        };
        let rows = step(lat1 - lat0, 111.2);
        let cols = step(lon1 - lon0, 111.2 * k);
        Some(Self {
            lat0,
            lon0,
            dlat: (lat1 - lat0) / rows as f64,
            dlon: (lon1 - lon0) / cols as f64,
            rows: rows + 1,
            cols: cols + 1,
        })
    }

    fn pos(&self, r: usize, c: usize) -> Pos {
        Pos {
            lat: self.lat0 + r as f64 * self.dlat,
            lon: self.lon0 + c as f64 * self.dlon,
        }
    }
}

/// Up to `count` hills, highest first, at least 2 km apart, away from existing sites, and
/// within `max_km` of at least two of them, since a hill that can reach one site adds no
/// redundancy.
pub fn find(
    terrain: &Terrain,
    around: &[Pos],
    margin_km: f64,
    max_km: f64,
    count: usize,
) -> Vec<Hill> {
    let Some(g) = Grid::around(around, margin_km) else {
        return vec![];
    };
    let h: Vec<Option<f64>> = (0..g.rows)
        .flat_map(|r| (0..g.cols).map(move |c| (r, c)))
        .map(|(r, c)| terrain.elevation(g.pos(r, c)))
        .collect();
    let at = |r: usize, c: usize| h[r * g.cols + c];
    let mut peaks = vec![];
    for r in WINDOW..g.rows.saturating_sub(WINDOW) {
        for c in WINDOW..g.cols.saturating_sub(WINDOW) {
            let Some(v) = at(r, c) else { continue };
            let highest = (r - WINDOW..=r + WINDOW)
                .flat_map(|y| (c - WINDOW..=c + WINDOW).map(move |x| (y, x)))
                .all(|(y, x)| at(y, x).is_some_and(|w| w < v || (w == v && (y, x) >= (r, c))));
            if highest {
                peaks.push(Hill {
                    pos: g.pos(r, c),
                    elev: v,
                });
            }
        }
    }
    peaks.sort_by(|a, b| b.elev.total_cmp(&a.elev));
    let mut out: Vec<Hill> = vec![];
    for p in peaks {
        let clear = out
            .iter()
            .all(|q| distance_km(p.pos, q.pos) >= MIN_APART_KM)
            && around
                .iter()
                .all(|s| distance_km(p.pos, *s) >= NEAR_SITE_KM)
            && around
                .iter()
                .filter(|s| distance_km(p.pos, **s) <= max_km)
                .count()
                >= 2;
        if clear {
            out.push(p);
            if out.len() == count {
                break;
            }
        }
    }
    out
}

/// Each hill as a candidate linked to every located site within `max_km`.
pub fn candidates(hills: &[Hill], net: &Net, max_km: f64) -> Vec<Candidate> {
    hills
        .iter()
        .filter_map(|h| {
            let links: Vec<String> = net
                .pos
                .iter()
                .filter(|(s, p)| net.sites.contains(*s) && distance_km(h.pos, **p) <= max_km)
                .map(|(s, _)| s.clone())
                .collect();
            (!links.is_empty()).then(|| {
                Candidate::at(
                    format!("Hilltop, {:.0} m", h.elev),
                    h.pos.lat,
                    h.pos.lon,
                    links,
                )
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::tests::demo_net;
    use crate::terrain::tests::synthetic;

    #[test]
    fn finds_the_peaks_highest_first_and_apart() {
        let dir = tempfile::tempdir().unwrap();
        let t = Terrain::new(dir.path());
        // two cones on flat ground: a 600 m one and a 400 m one
        let cone = |r: usize, c: usize, (pr, pc): (usize, usize), top: f64| {
            let d = ((r as f64 - pr as f64).powi(2) + (c as f64 - pc as f64).powi(2)).sqrt();
            (top - d * 5.0).max(0.0)
        };
        t.store(
            35,
            -121,
            &synthetic(|r, c| {
                (100.0 + cone(r, c, (300, 300), 500.0).max(cone(r, c, (800, 900), 300.0))) as i16
            }),
        )
        .unwrap();
        let around = [
            Pos {
                lat: 35.5,
                lon: -120.5,
            },
            Pos {
                lat: 35.5,
                lon: -120.6,
            },
        ];
        let hills = find(&t, &around, 40.0, 60.0, 5);
        assert!(
            find(&t, &around[..1], 40.0, 60.0, 5).is_empty(),
            "one site in reach: no redundancy to add"
        );
        assert!(hills.len() >= 2, "{hills:?}");
        // row 300 is 0.25 degrees below the north edge, column 300 a quarter of the way east
        let top = hills[0];
        assert!((top.elev - 600.0).abs() < 15.0, "{top:?}");
        assert!((top.pos.lat - 35.75).abs() < 0.01 && (top.pos.lon + 120.75).abs() < 0.01);
        assert!((hills[1].elev - 400.0).abs() < 15.0, "{:?}", hills[1]);
        for (i, a) in hills.iter().enumerate() {
            for b in &hills[i + 1..] {
                assert!(distance_km(a.pos, b.pos) >= MIN_APART_KM);
            }
        }
    }

    #[test]
    fn hills_link_to_sites_in_reach() {
        let net = demo_net();
        let hill = Hill {
            pos: Pos {
                lat: 35.6,
                lon: -120.7,
            },
            elev: 500.0,
        };
        let c = &candidates(&[hill], &net, 10.0)[0];
        assert_eq!(c.links, vec!["tst-north", "tst-paso"]);
        assert!(candidates(&[hill], &net, 1.0).is_empty());
    }
}
