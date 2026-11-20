//! Elevation tiles in the SRTM `.hgt` format and terrain profiles between two points.
//!
//! An `.hgt` tile covers one degree square, named for its south-west corner (N35W121).
//! It is a square grid of big-endian signed 16-bit heights in metres, 1201 or 3601 a
//! side (3 or 1 arc-second), rows from north to south, columns from west to east, with
//! -32768 marking a void. SRTM and Copernicus GLO-30 are surface models: they include
//! trees and buildings, and their heights are above the geoid, not the GPS ellipsoid.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::geo::distance_km;
use crate::inflate;
use crate::net::Pos;

const VOID: i16 = -32768;
/// Tiles kept decoded in memory; a 1 arc-second tile is 26 MB.
const CACHE_TILES: usize = 8;
/// A profile never has more points than this, whatever its length.
const MAX_POINTS: usize = 1200;

pub struct Tile {
    lat: i32,
    lon: i32,
    n: usize,
    data: Vec<i16>,
}

/// "N35W121" for the tile whose south-west corner is 35 N, 121 W.
pub fn tile_name(lat: i32, lon: i32) -> String {
    format!(
        "{}{:02}{}{:03}",
        if lat >= 0 { 'N' } else { 'S' },
        lat.abs(),
        if lon >= 0 { 'E' } else { 'W' },
        lon.abs()
    )
}

/// The corner a tile name stands for, from "N35W121" or "n35w121.hgt(.gz)".
pub fn parse_name(name: &str) -> Option<(i32, i32)> {
    let base = name.split('.').next()?.to_ascii_uppercase();
    let b = base.as_bytes();
    if b.len() != 7 {
        return None;
    }
    let lat: i32 = base.get(1..3)?.parse().ok()?;
    let lon: i32 = base.get(4..7)?.parse().ok()?;
    let lat = match b[0] {
        b'N' => lat,
        b'S' => -lat,
        _ => return None,
    };
    let lon = match b[3] {
        b'E' => lon,
        b'W' => -lon,
        _ => return None,
    };
    ((-90..90).contains(&lat) && (-180..180).contains(&lon)).then_some((lat, lon))
}

/// The tile a point falls in.
pub fn tile_of(p: Pos) -> (i32, i32) {
    (p.lat.floor() as i32, p.lon.floor() as i32)
}

/// Every tile touching the box around these points, grown by `margin_km` on each side.
pub fn tiles_around(points: &[Pos], margin_km: f64) -> BTreeSet<(i32, i32)> {
    let mut out = BTreeSet::new();
    let Some(lat_min) = points.iter().map(|p| p.lat).reduce(f64::min) else {
        return out;
    };
    let lat_max = points.iter().map(|p| p.lat).fold(lat_min, f64::max);
    let lon_min = points.iter().map(|p| p.lon).fold(f64::INFINITY, f64::min);
    let lon_max = points
        .iter()
        .map(|p| p.lon)
        .fold(f64::NEG_INFINITY, f64::max);
    let dlat = margin_km / 111.2;
    let dlon = margin_km / (111.2 * ((lat_min + lat_max) / 2.0).to_radians().cos().max(0.1));
    let (s, w) = tile_of(Pos {
        lat: (lat_min - dlat).max(-89.999),
        lon: (lon_min - dlon).max(-179.999),
    });
    let (n, e) = tile_of(Pos {
        lat: (lat_max + dlat).min(89.999),
        lon: (lon_max + dlon).min(179.999),
    });
    for lat in s..=n {
        for lon in w..=e {
            out.insert((lat, lon));
        }
    }
    out
}

impl Tile {
    /// Check and decode a tile's bytes (already decompressed).
    pub fn parse(lat: i32, lon: i32, bytes: &[u8]) -> Result<Self> {
        let n = match bytes.len() {
            2_884_802 => 1201,
            25_934_402 => 3601,
            len => bail!(
                "{} is {len} bytes; an .hgt tile is 2884802 (3 arc-second) or 25934402 (1 arc-second)",
                tile_name(lat, lon)
            ),
        };
        let data: Vec<i16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| i16::from_be_bytes(*c))
            .collect();
        let valid = data.iter().filter(|v| **v != VOID).count();
        if valid < data.len() / 2 {
            bail!(
                "{} is mostly voids; is it really an elevation tile?",
                tile_name(lat, lon)
            );
        }
        // some tiles carry sea-floor depths; a byte-swapped file reads as wild values
        if data
            .iter()
            .any(|v| *v != VOID && !(-11_000..=9000).contains(v))
        {
            bail!(
                "{} has heights outside -11000 to 9000 m; is it big-endian .hgt?",
                tile_name(lat, lon)
            );
        }
        Ok(Self { lat, lon, n, data })
    }

    /// A sample's height. Depths below 100 m are sea floor, and radio sees the water's
    /// surface there; shallower ones may be real land below sea level.
    fn at(&self, row: usize, col: usize) -> Option<f64> {
        let v = self.data[row * self.n + col];
        (v != VOID).then_some(if v < -100 { 0.0 } else { v as f64 })
    }

    /// Height at a point inside the tile, bilinear between the four nearest samples; a
    /// void among them falls back to the nearest sample that has a height.
    pub fn elevation(&self, p: Pos) -> Option<f64> {
        let last = (self.n - 1) as f64;
        let y = (self.lat as f64 + 1.0 - p.lat) * last;
        let x = (p.lon - self.lon as f64) * last;
        if !(0.0..=last).contains(&y) || !(0.0..=last).contains(&x) {
            return None;
        }
        let (r0, c0) = (y.floor() as usize, x.floor() as usize);
        let (r1, c1) = ((r0 + 1).min(self.n - 1), (c0 + 1).min(self.n - 1));
        let (fy, fx) = (y - r0 as f64, x - c0 as f64);
        let corners = [
            (self.at(r0, c0), (1.0 - fy) * (1.0 - fx)),
            (self.at(r0, c1), (1.0 - fy) * fx),
            (self.at(r1, c0), fy * (1.0 - fx)),
            (self.at(r1, c1), fy * fx),
        ];
        if corners.iter().all(|(v, _)| v.is_some()) {
            return Some(corners.iter().map(|(v, w)| v.unwrap_or(0.0) * w).sum());
        }
        corners
            .iter()
            .filter_map(|(v, w)| Some((v.as_ref()?, w)))
            .max_by(|a, b| a.1.total_cmp(b.1))
            .map(|(v, _)| *v)
    }
}

/// Decoded tiles by corner; None for a tile that was missing or unreadable when looked at.
type Cache = HashMap<(i32, i32), (Option<Arc<Tile>>, Instant)>;
/// A missing tile is looked for again after this, since another process may import it.
const RECHECK: Duration = Duration::from_secs(30);

/// A folder of `.hgt` tiles, decoded on demand.
pub struct Terrain {
    dir: PathBuf,
    cache: Mutex<Cache>,
}

impl Terrain {
    pub fn new(dir: &Path) -> Self {
        Self {
            dir: dir.to_path_buf(),
            cache: Mutex::new(HashMap::new()),
        }
    }

    pub fn path_of(&self, lat: i32, lon: i32) -> PathBuf {
        self.dir.join(format!("{}.hgt", tile_name(lat, lon)))
    }

    pub fn has(&self, lat: i32, lon: i32) -> bool {
        self.path_of(lat, lon).is_file()
    }

    /// Tiles on disk, by corner.
    pub fn list(&self) -> Result<BTreeSet<(i32, i32)>> {
        let Ok(rd) = std::fs::read_dir(&self.dir) else {
            return Ok(BTreeSet::new());
        };
        let mut out = BTreeSet::new();
        for e in rd {
            let name = e?.file_name().to_string_lossy().to_string();
            if name.to_ascii_lowercase().ends_with(".hgt")
                && let Some(c) = parse_name(&name)
            {
                out.insert(c);
            }
        }
        Ok(out)
    }

    fn tile(&self, lat: i32, lon: i32) -> Option<Arc<Tile>> {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((t, at)) = cache.get(&(lat, lon))
            && (t.is_some() || at.elapsed() < RECHECK)
        {
            return t.clone();
        }
        let t = match std::fs::read(self.path_of(lat, lon)) {
            Ok(bytes) => match Tile::parse(lat, lon, &bytes) {
                Ok(t) => Some(Arc::new(t)),
                Err(e) => {
                    tracing::warn!("terrain: {e:#}");
                    None
                }
            },
            Err(_) => None,
        };
        if cache.len() >= CACHE_TILES {
            cache.clear();
        }
        cache.insert((lat, lon), (t.clone(), Instant::now()));
        t
    }

    /// Forget decoded tiles, after tiles were added or replaced on disk.
    pub fn reload(&self) {
        self.cache.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    pub fn elevation(&self, p: Pos) -> Option<f64> {
        let (lat, lon) = tile_of(p);
        self.tile(lat, lon)?.elevation(p)
    }

    /// Heights along the great circle from `a` to `b`, evenly spaced, about `step_m` apart:
    /// (spacing in metres, heights). None where any point has no terrain.
    pub fn profile(&self, a: Pos, b: Pos, step_m: f64) -> Option<(f64, Vec<f64>)> {
        let d = distance_km(a, b) * 1000.0;
        let n = ((d / step_m).ceil() as usize).clamp(2, MAX_POINTS - 1);
        let mut out = Vec::with_capacity(n + 1);
        for i in 0..=n {
            out.push(self.elevation(intermediate(a, b, i as f64 / n as f64))?);
        }
        Some((d / n as f64, out))
    }

    /// Check a tile file and copy it into the folder, unzipping it if it is gzipped.
    /// The tile is named for its corner, so the file name must say which tile it is.
    pub fn import(&self, src: &Path) -> Result<(i32, i32)> {
        let name = src
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let Some((lat, lon)) = parse_name(&name) else {
            bail!(
                "{} isn't named like a tile (N35W121.hgt or N35W121.hgt.gz), so its position is unknown",
                src.display()
            );
        };
        let bytes = std::fs::read(src).with_context(|| format!("reading {}", src.display()))?;
        self.store(lat, lon, &bytes)?;
        Ok((lat, lon))
    }

    /// Check tile bytes (gzipped or not) and write them into the folder.
    pub fn store(&self, lat: i32, lon: i32, bytes: &[u8]) -> Result<()> {
        let raw = if bytes.starts_with(&[0x1f, 0x8b]) {
            inflate::gunzip(bytes, 26_000_000)?
        } else {
            bytes.to_vec()
        };
        Tile::parse(lat, lon, &raw)?;
        std::fs::create_dir_all(&self.dir)?;
        let path = self.path_of(lat, lon);
        let tmp = path.with_extension("part");
        std::fs::write(&tmp, &raw).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &path)?;
        self.reload();
        Ok(())
    }
}

/// The point a fraction `f` of the way along the great circle from `a` to `b`.
fn intermediate(a: Pos, b: Pos, f: f64) -> Pos {
    let (la1, lo1, la2, lo2) = (
        a.lat.to_radians(),
        a.lon.to_radians(),
        b.lat.to_radians(),
        b.lon.to_radians(),
    );
    let delta = distance_km(a, b) / 6371.0088;
    if delta < 1e-12 {
        return a;
    }
    let (ka, kb) = (
        ((1.0 - f) * delta).sin() / delta.sin(),
        (f * delta).sin() / delta.sin(),
    );
    let x = ka * la1.cos() * lo1.cos() + kb * la2.cos() * lo2.cos();
    let y = ka * la1.cos() * lo1.sin() + kb * la2.cos() * lo2.sin();
    let z = ka * la1.sin() + kb * la2.sin();
    Pos {
        lat: z.atan2(x.hypot(y)).to_degrees(),
        lon: y.atan2(x).to_degrees(),
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// A 3 arc-second tile whose height is `f(row, col)`.
    pub fn synthetic(f: impl Fn(usize, usize) -> i16) -> Vec<u8> {
        let mut out = Vec::with_capacity(1201 * 1201 * 2);
        for r in 0..1201 {
            for c in 0..1201 {
                out.extend_from_slice(&f(r, c).to_be_bytes());
            }
        }
        out
    }

    #[test]
    fn names_round_trip() {
        assert_eq!(tile_name(35, -121), "N35W121");
        assert_eq!(tile_name(-1, 5), "S01E005");
        assert_eq!(parse_name("n35w121.hgt.gz"), Some((35, -121)));
        assert_eq!(parse_name("S01E005.hgt"), Some((-1, 5)));
        assert_eq!(parse_name("N95W121.hgt"), None);
        assert_eq!(parse_name("terrain.hgt"), None);
        assert_eq!(
            tile_of(Pos {
                lat: 35.3,
                lon: -120.6
            }),
            (35, -121)
        );
    }

    #[test]
    fn reads_heights_in_row_and_column_order() {
        // height rises 1 m per row going south and 2 m per column going east
        let bytes = synthetic(|r, c| (r + 2 * c) as i16);
        let t = Tile::parse(35, -121, &bytes).unwrap();
        let at = |lat: f64, lon: f64| t.elevation(Pos { lat, lon }).unwrap();
        assert_eq!(
            at(36.0, -121.0),
            0.0,
            "north-west corner is the first sample"
        );
        assert_eq!(
            at(35.0, -121.0),
            1200.0,
            "south-west corner is the last row"
        );
        assert_eq!(at(36.0, -120.0), 2400.0, "north-east corner");
        // half a sample east of the north-west corner interpolates
        assert!((at(36.0, -121.0 + 0.5 / 1200.0) - 1.0).abs() < 1e-9);
        assert!(
            t.elevation(Pos {
                lat: 34.9,
                lon: -120.5
            })
            .is_none()
        );
    }

    #[test]
    fn voids_fall_back_to_the_nearest_height() {
        let bytes = synthetic(|r, c| if (r, c) == (0, 0) { VOID } else { 50 });
        let t = Tile::parse(35, -121, &bytes).unwrap();
        assert_eq!(
            t.elevation(Pos {
                lat: 36.0,
                lon: -121.0
            }),
            Some(50.0)
        );
    }

    #[test]
    fn sea_floor_reads_as_sea_level() {
        let bytes = synthetic(|r, _| if r < 600 { -2000 } else { -50 });
        let t = Tile::parse(35, -121, &bytes).unwrap();
        assert_eq!(
            t.elevation(Pos {
                lat: 35.9,
                lon: -120.5
            }),
            Some(0.0)
        );
        assert_eq!(
            t.elevation(Pos {
                lat: 35.1,
                lon: -120.5
            }),
            Some(-50.0)
        );
    }

    #[test]
    fn refuses_wrong_sizes_and_little_endian_files() {
        assert!(Tile::parse(35, -121, &[0; 100]).is_err());
        let le: Vec<u8> = synthetic(|_, _| 300)
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|c| [c[1], c[0]])
            .collect();
        assert!(
            Tile::parse(35, -121, &le).is_err(),
            "300 read little-endian is 11265"
        );
    }

    #[test]
    fn profiles_follow_the_path_and_need_every_tile() {
        let dir = tempfile::tempdir().unwrap();
        let t = Terrain::new(dir.path());
        // a ridge along the middle column of N35W121
        t.store(
            35,
            -121,
            &synthetic(|_, c| if (598..=602).contains(&c) { 900 } else { 100 }),
        )
        .unwrap();
        let a = Pos {
            lat: 35.5,
            lon: -120.9,
        };
        let b = Pos {
            lat: 35.5,
            lon: -120.1,
        };
        let (xi, h) = t.profile(a, b, 90.0).unwrap();
        assert!((xi * (h.len() - 1) as f64 - distance_km(a, b) * 1000.0).abs() < 1.0);
        assert_eq!(h[0], 100.0);
        assert!(h.iter().any(|v| *v > 500.0), "crosses the ridge");
        let off = Pos {
            lat: 35.5,
            lon: -119.5,
        };
        assert!(t.profile(a, off, 90.0).is_none(), "N35W120 is missing");
        assert_eq!(t.list().unwrap(), [(35, -121)].into());
    }

    #[test]
    fn imports_gzipped_tiles_by_file_name() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("in");
        std::fs::create_dir_all(&src).unwrap();
        let t = Terrain::new(&dir.path().join("tiles"));
        let plain = src.join("N35W121.hgt");
        std::fs::write(&plain, synthetic(|_, _| 10)).unwrap();
        assert_eq!(t.import(&plain).unwrap(), (35, -121));
        let gz = src.join("n36w121.hgt.gz");
        std::fs::copy("tests/data/n36w121.hgt.gz", &gz).unwrap();
        assert_eq!(t.import(&gz).unwrap(), (36, -121));
        assert_eq!(
            t.elevation(Pos {
                lat: 36.5,
                lon: -120.5
            }),
            Some(7.0)
        );
        let unnamed = src.join("tile.hgt");
        std::fs::write(&unnamed, synthetic(|_, _| 10)).unwrap();
        assert!(t.import(&unnamed).is_err());
    }

    #[test]
    fn tiles_around_cover_the_margin() {
        let pts = [Pos {
            lat: 35.05,
            lon: -120.95,
        }];
        let t = tiles_around(&pts, 10.0);
        assert_eq!(t, [(34, -122), (34, -121), (35, -122), (35, -121)].into());
    }
}
