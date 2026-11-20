//! `planner.toml`.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use ridgewarden_client::util::{norm_node, valid_node};
use serde::{Deserialize, Serialize};

use crate::itm::{Climate, Params, Polarization};

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// the ridgewarden core on this box
    #[serde(default = "d_core_url")]
    pub core_url: String,
    /// the core's `local.token`, to share terrain tiles and predictions; without it the
    /// planner is read-only
    #[serde(default = "d_token")]
    pub token_file: PathBuf,
    /// the core's data_dir: tiles go to other sites through its `imports` folder and
    /// arrive through its `exports` folder
    #[serde(default = "d_core_data_dir")]
    pub core_data_dir: PathBuf,
    #[serde(default = "d_data_dir")]
    pub data_dir: PathBuf,
    #[serde(default = "d_listen")]
    pub listen: SocketAddr,
    /// link data older than this counts as stale; make it a few poll (or crawl) rounds
    #[serde(default = "d_fresh")]
    pub fresh_s: u64,
    /// friendly site names: node name -> label
    #[serde(default)]
    pub site_names: BTreeMap<String, String>,
    /// services on LAN hosts behind a node: host name -> the site's node name
    #[serde(default)]
    pub host_sites: BTreeMap<String, String>,
    #[serde(default)]
    pub terrain: TerrainCfg,
    #[serde(default)]
    pub radio: RadioCfg,
    /// areas calibrated on their own, e.g. a corridor with different terrain
    #[serde(default, rename = "region")]
    pub regions: Vec<Region>,
    #[serde(default, rename = "candidate")]
    pub candidates: Vec<Candidate>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct TerrainCfg {
    /// offer tiles imported here to every other site, through the core's blob store
    #[serde(default = "yes")]
    pub share: bool,
    /// fetch tiles other sites shared, when they cover this mesh
    #[serde(default = "yes")]
    pub fetch_shared: bool,
    /// how far around the sites and candidates terrain is wanted
    #[serde(default = "d_margin")]
    pub margin_km: f64,
}

impl Default for TerrainCfg {
    fn default() -> Self {
        Self {
            share: true,
            fetch_shared: true,
            margin_km: d_margin(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ClimateName {
    Equatorial,
    ContinentalSubtropical,
    MaritimeSubtropical,
    Desert,
    ContinentalTemperate,
    MaritimeTemperateLand,
    MaritimeTemperateSea,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Ground {
    Average,
    Poor,
    Good,
    FreshWater,
    SeaWater,
}

impl Ground {
    /// Relative permittivity and conductivity (S/m), as in ITM's documentation.
    fn constants(self) -> (f64, f64) {
        match self {
            Ground::Average => (15.0, 0.005),
            Ground::Poor => (4.0, 0.001),
            Ground::Good => (25.0, 0.020),
            Ground::FreshWater => (81.0, 0.010),
            Ground::SeaWater => (81.0, 5.0),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Pol {
    Horizontal,
    Vertical,
}

/// Radio settings for predictions; each can be set per site under `[radio.sites."node"]`.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct RadioCfg {
    #[serde(default = "d_band")]
    pub band_mhz: f64,
    #[serde(default = "d_tx")]
    pub tx_dbm: f64,
    #[serde(default = "d_gain")]
    pub gain_dbi: f64,
    #[serde(default)]
    pub cable_db: f64,
    /// antenna height above the ground the terrain tiles show
    #[serde(default = "d_height")]
    pub height_m: f64,
    /// noise floor for sites that haven't measured their own
    #[serde(default = "d_noise")]
    pub noise_dbm: f64,
    /// SNR a link needs to count as workable
    #[serde(default = "d_min_snr")]
    pub min_snr_db: f64,
    #[serde(default = "d_climate")]
    pub climate: ClimateName,
    /// surface refractivity, N-units; 301 is ITM's usual value
    #[serde(default = "d_refractivity")]
    pub refractivity: f64,
    #[serde(default = "d_pol")]
    pub polarization: Pol,
    #[serde(default = "d_ground")]
    pub ground: Ground,
    /// geoid height (EGM2008) for this area, to turn GPS (ellipsoid) altitudes into
    /// heights above the terrain tiles' datum; needed only with `alt_m`
    #[serde(default)]
    pub geoid_m: Option<f64>,
    /// the longest link the hilltop search will propose
    #[serde(default = "d_max_link")]
    pub max_link_km: f64,
    #[serde(default)]
    pub sites: BTreeMap<String, SiteRadio>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SiteRadio {
    pub band_mhz: Option<f64>,
    pub tx_dbm: Option<f64>,
    pub gain_dbi: Option<f64>,
    pub cable_db: Option<f64>,
    pub height_m: Option<f64>,
    /// the antenna's GPS altitude (WGS84 ellipsoid), instead of a height above ground
    pub alt_m: Option<f64>,
}

impl Default for RadioCfg {
    fn default() -> Self {
        toml::from_str("").expect("radio defaults")
    }
}

/// One site's radio, with the defaults filled in.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Radio {
    pub band_mhz: f64,
    pub tx_dbm: f64,
    pub gain_dbi: f64,
    pub cable_db: f64,
    pub height_m: f64,
    pub alt_m: Option<f64>,
}

impl RadioCfg {
    pub fn site(&self, node: &str) -> Radio {
        let s = self
            .sites
            .iter()
            .find(|(k, _)| norm_node(k) == node)
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        Radio {
            band_mhz: s.band_mhz.unwrap_or(self.band_mhz),
            tx_dbm: s.tx_dbm.unwrap_or(self.tx_dbm),
            gain_dbi: s.gain_dbi.unwrap_or(self.gain_dbi),
            cable_db: s.cable_db.unwrap_or(self.cable_db),
            height_m: s.height_m.unwrap_or(self.height_m),
            alt_m: s.alt_m,
        }
    }

    /// The defaults, for a place that isn't a site yet.
    pub fn default_radio(&self) -> Radio {
        Radio {
            band_mhz: self.band_mhz,
            tx_dbm: self.tx_dbm,
            gain_dbi: self.gain_dbi,
            cable_db: self.cable_db,
            height_m: self.height_m,
            alt_m: None,
        }
    }

    /// ITM's inputs for a link on `band_mhz`, at the median (50% time, location and
    /// situation), in mobile mode without location variability, as NTIA's examples use.
    pub fn itm(&self, band_mhz: f64) -> Params {
        let (epsilon, sigma) = self.ground.constants();
        Params {
            climate: match self.climate {
                ClimateName::Equatorial => Climate::Equatorial,
                ClimateName::ContinentalSubtropical => Climate::ContinentalSubtropical,
                ClimateName::MaritimeSubtropical => Climate::MaritimeSubtropical,
                ClimateName::Desert => Climate::Desert,
                ClimateName::ContinentalTemperate => Climate::ContinentalTemperate,
                ClimateName::MaritimeTemperateLand => Climate::MaritimeTemperateOverLand,
                ClimateName::MaritimeTemperateSea => Climate::MaritimeTemperateOverSea,
            },
            n_0: self.refractivity,
            f_mhz: band_mhz,
            pol: match self.polarization {
                Pol::Horizontal => Polarization::Horizontal,
                Pol::Vertical => Polarization::Vertical,
            },
            epsilon,
            sigma,
            mdvar: 12,
            time: 50.0,
            location: 50.0,
            situation: 50.0,
        }
    }

    fn check(&self) -> Result<()> {
        let bands =
            std::iter::once(self.band_mhz).chain(self.sites.values().filter_map(|s| s.band_mhz));
        for b in bands {
            if !(20.0..=20_000.0).contains(&b) {
                bail!("radio band_mhz {b} is outside 20 to 20000 MHz, the range ITM covers");
            }
        }
        let heights =
            std::iter::once(self.height_m).chain(self.sites.values().filter_map(|s| s.height_m));
        for h in heights {
            if !(0.5..=3000.0).contains(&h) {
                bail!("radio height_m {h} is outside 0.5 to 3000 m");
            }
        }
        if !(250.0..=400.0).contains(&self.refractivity) {
            bail!("radio.refractivity must be 250 to 400 N-units");
        }
        if self.max_link_km <= 0.0 {
            bail!("radio.max_link_km must be above zero");
        }
        for (node, s) in &self.sites {
            if !valid_node(&norm_node(node)) {
                bail!("radio.sites: {node:?} is not a node name");
            }
            if s.alt_m.is_some() && self.geoid_m.is_none() {
                bail!(
                    "radio.sites.{node:?} gives alt_m, a GPS altitude; set radio.geoid_m (the EGM2008 geoid height there) so it can be compared with the terrain"
                );
            }
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Region {
    pub name: String,
    pub lat_min: f64,
    pub lat_max: f64,
    pub lon_min: f64,
    pub lon_max: f64,
}

impl Region {
    pub fn contains(&self, lat: f64, lon: f64) -> bool {
        (self.lat_min..=self.lat_max).contains(&lat) && (self.lon_min..=self.lon_max).contains(&lon)
    }
}

/// Getting to the site, best last so filters can say "at least".
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Access {
    None,
    Foot,
    #[serde(rename = "4wd")]
    Offroad,
    Road,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Power {
    None,
    Solar,
    Grid,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Lease {
    No,
    Pending,
    Yes,
}

impl Access {
    pub fn word(self) -> &'static str {
        match self {
            Access::None => "no access",
            Access::Foot => "on foot",
            Access::Offroad => "4WD",
            Access::Road => "road",
        }
    }
}

impl Power {
    pub fn word(self) -> &'static str {
        match self {
            Power::None => "no power",
            Power::Solar => "solar",
            Power::Grid => "grid power",
        }
    }
}

impl Lease {
    pub fn word(self) -> &'static str {
        match self {
            Lease::No => "no lease",
            Lease::Pending => "lease pending",
            Lease::Yes => "lease agreed",
        }
    }
}

/// A place a new node could go, and the existing sites it could link to.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Candidate {
    pub name: String,
    pub lat: f64,
    pub lon: f64,
    pub links: Vec<String>,
    #[serde(default)]
    pub note: String,
    /// antenna height above ground, when it differs from the radio default
    #[serde(default)]
    pub height_m: Option<f64>,
    #[serde(default)]
    pub access: Option<Access>,
    #[serde(default)]
    pub power: Option<Power>,
    #[serde(default)]
    pub lease: Option<Lease>,
    #[serde(default)]
    pub owner: String,
}

pub const MAX_NAME: usize = 60;
const MAX_LINKS: usize = 50;
const MAX_TEXT: usize = 300;

impl Candidate {
    /// A candidate at a point with nothing else known about it.
    pub fn at(name: String, lat: f64, lon: f64, links: Vec<String>) -> Self {
        Self {
            name,
            lat,
            lon,
            links,
            note: String::new(),
            height_m: None,
            access: None,
            power: None,
            lease: None,
            owner: String::new(),
        }
    }

    /// Normalize link names and check the rest; the same rules apply to config and the form.
    pub fn checked(mut self) -> Result<Self> {
        self.name = self.name.trim().to_string();
        if self.name.is_empty() || self.name.chars().count() > MAX_NAME {
            bail!("a candidate needs a name of 1 to {MAX_NAME} characters");
        }
        if !valid_lat_lon(self.lat, self.lon) {
            bail!(
                "candidate {:?}: {}, {} is not a latitude and longitude",
                self.name,
                self.lat,
                self.lon
            );
        }
        if self.links.len() > MAX_LINKS {
            bail!("candidate {:?}: at most {MAX_LINKS} links", self.name);
        }
        if self.note.chars().count() > MAX_TEXT || self.owner.chars().count() > MAX_TEXT {
            bail!(
                "candidate {:?}: note and owner are limited to {MAX_TEXT} characters",
                self.name
            );
        }
        if let Some(h) = self.height_m
            && !(0.5..=3000.0).contains(&h)
        {
            bail!("candidate {:?}: height_m must be 0.5 to 3000 m", self.name);
        }
        let mut links = vec![];
        for l in &self.links {
            let n = norm_node(l);
            if !valid_node(&n) {
                bail!("candidate {:?}: {l:?} is not a node name", self.name);
            }
            if !links.contains(&n) {
                links.push(n);
            }
        }
        if links.is_empty() {
            bail!(
                "candidate {:?} needs at least one site it could link to",
                self.name
            );
        }
        self.links = links;
        Ok(self)
    }
}

/// Inside the usual ranges, and not 0,0, which is what an unset position often reads as.
pub fn valid_lat_lon(lat: f64, lon: f64) -> bool {
    (-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon) && (lat, lon) != (0.0, 0.0)
}

fn yes() -> bool {
    true
}
fn d_core_url() -> String {
    "http://127.0.0.1:8790".into()
}
fn d_token() -> PathBuf {
    PathBuf::from("/var/lib/ridgewarden/local.token")
}
fn d_core_data_dir() -> PathBuf {
    PathBuf::from("/var/lib/ridgewarden")
}
fn d_data_dir() -> PathBuf {
    PathBuf::from("/var/lib/ridgewarden-planner")
}
fn d_listen() -> SocketAddr {
    "0.0.0.0:8794".parse().unwrap()
}
fn d_fresh() -> u64 {
    960
}
fn d_margin() -> f64 {
    20.0
}
fn d_band() -> f64 {
    5800.0
}
fn d_tx() -> f64 {
    27.0
}
fn d_gain() -> f64 {
    23.0
}
fn d_height() -> f64 {
    10.0
}
fn d_noise() -> f64 {
    -95.0
}
fn d_min_snr() -> f64 {
    15.0
}
fn d_climate() -> ClimateName {
    ClimateName::ContinentalTemperate
}
fn d_refractivity() -> f64 {
    301.0
}
fn d_pol() -> Pol {
    Pol::Vertical
}
fn d_ground() -> Ground {
    Ground::Average
}
fn d_max_link() -> f64 {
    50.0
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let cfg: Config =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        cfg.checked()
    }

    fn checked(mut self) -> Result<Self> {
        if self.fresh_s < 60 {
            bail!("fresh_s must be at least 60");
        }
        if !(0.0..=200.0).contains(&self.terrain.margin_km) {
            bail!("terrain.margin_km must be 0 to 200");
        }
        self.radio.check()?;
        for r in &self.regions {
            if r.name.trim().is_empty() || r.lat_min >= r.lat_max || r.lon_min >= r.lon_max {
                bail!(
                    "region {:?} needs a name and lat_min < lat_max, lon_min < lon_max",
                    r.name
                );
            }
        }
        for (host, site) in &self.host_sites {
            if !valid_node(&norm_node(host)) || !valid_node(&norm_node(site)) {
                bail!("host_sites: {host:?} = {site:?} must map a host name to a node name");
            }
        }
        self.host_sites = self
            .host_sites
            .iter()
            .map(|(h, s)| (norm_node(h), norm_node(s)))
            .collect();
        self.candidates = self
            .candidates
            .into_iter()
            .map(Candidate::checked)
            .collect::<Result<_>>()?;
        let mut names: Vec<&str> = self.candidates.iter().map(|c| c.name.as_str()).collect();
        names.sort();
        if let Some(w) = names.windows(2).find(|w| w[0] == w[1]) {
            bail!("two candidates are called {:?}", w[0]);
        }
        Ok(self)
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("planner.db")
    }

    pub fn terrain_dir(&self) -> PathBuf {
        self.data_dir.join("terrain")
    }

    pub fn fresh_ms(&self) -> i64 {
        self.fresh_s as i64 * 1000
    }

    pub fn label(&self, node: &str) -> String {
        self.site_names
            .iter()
            .find(|(k, _)| norm_node(k) == node)
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| node.to_string())
    }
}

pub const EXAMPLE: &str = r#"# ridgewarden-planner configuration
core_url = "http://127.0.0.1:8790"
token_file = "/var/lib/ridgewarden/local.token"   # to share tiles and predictions; without it, read-only
core_data_dir = "/var/lib/ridgewarden"            # the core's data_dir
data_dir = "/var/lib/ridgewarden-planner"
listen = "0.0.0.0:8794"
fresh_s = 960      # link data older than this is stale; a few poll or crawl rounds

[site_names]
# "tst-ridge" = "Ridge"

# Services on a computer behind a node, rather than on the node itself, can't be placed
# on a site from the service list alone. Say where they are:
[host_sites]
# "tst-pi" = "tst-ridge"

[terrain]
share = true        # offer tiles imported here to every other site over the mesh
fetch_shared = true # take tiles other sites shared, when they cover this mesh
margin_km = 20      # how far around the sites and candidates terrain is wanted

# What the predictions assume. Calibration against the measured links corrects for
# what's wrong here on average, but per-site values make each link's prediction better.
[radio]
band_mhz = 5800
tx_dbm = 27         # transmit power at the radio
gain_dbi = 23       # antenna gain
cable_db = 0        # cable and connector loss at each end
height_m = 10       # antenna above the ground the terrain tiles show (trees and roofs included)
noise_dbm = -95     # for sites that haven't measured their noise floor
min_snr_db = 15     # what a link needs to count as workable
climate = "continental-temperate"   # ITM radio climate
refractivity = 301
polarization = "vertical"
ground = "average"  # average, poor, good, fresh-water or sea-water
max_link_km = 50    # the longest link the hilltop search proposes
# geoid_m = -33     # EGM2008 geoid height here; needed only for alt_m below

# [radio.sites."tst-ridge"]
# height_m = 18
# gain_dbi = 30
# alt_m = 812       # GPS altitude of the antenna instead of height_m

# Areas calibrated on their own, e.g. a coastal corridor with different terrain.
# [[region]]
# name = "Coast"
# lat_min = 34.8
# lat_max = 35.6
# lon_min = -121.0
# lon_max = -120.5

# Places a new node could go. Each is scored by the single points of failure it removes
# and the sites it protects. You can also try one out on the Candidates page.
# [[candidate]]
# name = "Hill 1200"
# lat = 35.41
# lon = -120.60
# links = ["tst-ridge", "tst-valley"]
# note = "owner says yes; needs solar"
# height_m = 12
# access = "4wd"    # road, 4wd, foot or none
# power = "solar"   # grid, solar or none
# lease = "pending" # yes, pending or no
# owner = "County parks"
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn uncommented() -> String {
        // turn the commented-out settings on, leaving the prose comments alone
        EXAMPLE
            .lines()
            .map(|l| match l.strip_prefix("# ") {
                Some(rest) if rest.starts_with('[') || rest.contains(" = ") => rest,
                _ => l,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn example_parses_with_and_without_its_comments() {
        let c: Config = toml::from_str::<Config>(EXAMPLE)
            .unwrap()
            .checked()
            .unwrap();
        assert_eq!(c.listen.port(), 8794);
        assert_eq!(c.radio.min_snr_db, 15.0);
        let c: Config = toml::from_str::<Config>(&uncommented())
            .unwrap()
            .checked()
            .unwrap();
        let cand = &c.candidates[0];
        assert_eq!(cand.links, vec!["tst-ridge", "tst-valley"]);
        assert_eq!(
            (cand.access, cand.power, cand.lease),
            (
                Some(Access::Offroad),
                Some(Power::Solar),
                Some(Lease::Pending)
            )
        );
        assert_eq!(c.host_sites["tst-pi"], "tst-ridge");
        let r = c.radio.site("tst-ridge");
        assert_eq!((r.height_m, r.gain_dbi, r.tx_dbm), (18.0, 30.0, 27.0));
        assert_eq!(r.alt_m, Some(812.0));
        assert!(c.regions[0].contains(35.2, -120.7));
    }

    #[test]
    fn a_gps_altitude_needs_the_geoid() {
        let text = uncommented().replace("geoid_m = -33", "");
        let c: Config = toml::from_str(&text).unwrap();
        let e = c.checked().unwrap_err().to_string();
        assert!(e.contains("geoid_m"), "{e}");
    }

    fn cand(lat: f64, links: &[&str]) -> Candidate {
        Candidate::at(
            "Hill".into(),
            lat,
            -120.0,
            links.iter().map(|s| s.to_string()).collect(),
        )
    }

    #[test]
    fn candidates_are_checked() {
        let c = cand(35.0, &["TST-Cuesta.local.mesh", "tst-cuesta"])
            .checked()
            .unwrap();
        assert_eq!(c.links, vec!["tst-cuesta"]);
        assert!(cand(95.0, &["tst-cuesta"]).checked().is_err());
        assert!(cand(35.0, &[]).checked().is_err());
        assert!(cand(35.0, &["not a node"]).checked().is_err());
    }

    #[test]
    fn duplicate_candidate_names_are_refused() {
        let mut c: Config = toml::from_str(EXAMPLE).unwrap();
        c.candidates = vec![cand(35.0, &["a"]), cand(36.0, &["b"])];
        assert!(c.checked().is_err());
    }

    #[test]
    fn access_power_and_lease_order_from_worst_to_best() {
        assert!(Access::Road > Access::Offroad && Access::Offroad > Access::Foot);
        assert!(Power::Grid > Power::Solar && Lease::Yes > Lease::Pending);
    }
}
