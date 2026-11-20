# ridgewarden-planner

[![CI](https://github.com/Londopy/ridgewarden-planner/actions/workflows/ci.yml/badge.svg)](https://github.com/Londopy/ridgewarden-planner/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/Londopy/ridgewarden-planner)](https://github.com/Londopy/ridgewarden-planner/releases/latest)
[![changelog](https://img.shields.io/endpoint?url=https://raw.githubusercontent.com/Londopy/ridgewarden-planner/gh-pages/changelog-badge.json)](CHANGELOG.md)
[![License](https://img.shields.io/github/license/Londopy/ridgewarden-planner)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-stable-orange)](https://www.rust-lang.org)
[![Linux](https://img.shields.io/badge/platform-Linux%20(Pi%20%7C%20x86__64)-555)](#building-from-source)

**See what breaks if a site dies, and where the next node would help most.**

A ridgewarden app that turns the live AREDN mesh into a graph you can break on purpose. Take out any site or link and it says, in plain words, which sites lose contact and which advertised services go out of reach. It predicts radio paths over the terrain with the Longley-Rice model (ITM), checks every prediction against what the real links measure, and uses the difference to correct predictions for links that don't exist yet. Give it places a new node could go, or let it search the hilltops, and it ranks them by the redundancy they add.

It needs a [ridgewarden](https://github.com/Londopy/ridgewarden) core running on the same box. The core is the only part that polls the AREDN node and syncs with other sites; this app reads the core's event feed. It writes to the core only to share terrain tiles and to publish link predictions for Link Doctor; without the core's token it is read-only and says so. It uses [Link Doctor](https://github.com/Londopy/ridgewarden-linkdoctor)'s link analysis, so with the same `fresh_s` the two agree on which links are up. Spec sheet 03 of the AREDN app suite.

## Features

- A map of the mesh drawn from each node's reported position, with single points of failure marked. It is plain SVG with links in it, so it works in any browser without JavaScript and without map tiles from the internet.
- Single points of failure, worst first: "Losing Ridge cuts Valley and Harbor off from Shack. 1 service becomes unreachable from Shack: Harbor Camera."
- **What if**: click sites and links on the map, or tick them in the form, to take any combination out. The report says who loses contact with this site, which of them can still reach each other, which services become unreachable, already were, or can't be placed on a site, and how many copies of the replicated chat ([ridgewarden-failover](https://github.com/Londopy/ridgewarden-failover)) stay reachable.
- **Predictions**: ITM point-to-point over terrain profiles, a Rust port of NTIA's reference code, checked against NTIA's own example cases. Paths under 1 km, below ITM's range, use free-space loss and say so.
- **Calibration**: every RF link the mesh reports is predicted and compared with the signal level its two ends measured over the last week. The median difference is the correction for new paths, per configured region where there are enough links, and the spread between links is the error bar: "SNR 18 dB +/- 4, calibrated on 6 links (300 samples)". The Links page shows by how much calibration shrinks the error, predicting each link from the others only.
- **Candidates**: each candidate site (a name, a position and the existing sites it could link to) is scored by the sites it protects, meaning no single site or link failure could cut them off any more, and by the single points of failure it removes. Each proposed link is predicted and corrected; a link predicted below the workable SNR isn't counted. Filter by access, power, lease and owner. Candidates come from the config, or try one in the form.
- **Hilltops**: searches the terrain around the mesh for high points, links each to every site in reach that predicts workable, and ranks them like candidates.
- **Terrain** from SRTM-format `.hgt` tiles (gzipped or not), imported once at any site and shared with every other site over the mesh through the core's blob store, only to the sites whose area they cover.
- **Predictions for Link Doctor**: each measured RF link's predicted SNR, corrected by the other links only, is published as a `planner.prediction` event so Link Doctor can show predicted against measured on its link pages. A link is published again only when its prediction moves by more than 1 dB, or once a day.
- **PDF report** for grant applications and meetings: the mesh, a map, every single point of failure, prediction accuracy and the candidates ranked.
- **GeoJSON** export of sites, links (with state, SNR, how far the prediction was off, and whether each is a single point of failure) and candidates with their proposed links.
- Says "partial picture" when some sites' links aren't visible, rather than guessing. Turn on the core's `[crawl]` to fill in links between sites without a core.
- `/api/whatif?site=<node>&link=<a>|<b>` returns a what-if report as JSON.

## Limits

- Fetching terrain from the internet is not built in: download the tiles yourself (see Terrain below) and import them. Fetching needs an HTTPS client the app doesn't have yet.
- No cross-check against the Signal-Server fork of ITM; the port is checked against NTIA's published example cases instead.
- Predictions use the transmit power, antenna gains and heights in the config. Calibration corrects them on average; per-site values make each link better.
- Whether a backbone team used the planner to choose a site, a success measure in the spec, isn't something software can record.

## Install

On a Raspberry Pi or Linux box that already runs the [ridgewarden core](https://github.com/Londopy/ridgewarden):

```sh
curl -fsSL https://github.com/Londopy/ridgewarden-planner/releases/latest/download/install.sh | sudo sh
```

Or take the `.deb` from the [releases page](https://github.com/Londopy/ridgewarden-planner/releases/latest): `sudo apt install ./ridgewarden-planner_<version>_arm64.deb` (`_amd64` on a PC). The install script checks the download against the release key your core already trusts. Either way `/etc/ridgewarden/planner.toml` is written if missing, and the service starts once the core is set up. The page is on port 8794.

Every release also has each download as a `.torrent`, with its GitHub download as the web seed, and signed `SHA256SUMS.txt` checksums.

The service runs as its own user, `ridgewarden-planner`, in the core's `ridgewarden` group, so it can sign events through the core but can't read the station's key. Run commands that touch the app's data as that user, so files keep the right owner: `sudo -u ridgewarden-planner ridgewarden-planner --config /etc/ridgewarden/planner.toml <command>`.

## Usage

```sh
ridgewarden-planner init --config /etc/ridgewarden/planner.toml
ridgewarden-planner run  --config /etc/ridgewarden/planner.toml
```

The page is on `http://<pi>:8794/`. Add it to the core's dashboard with an `[[app]]` entry in `ridgewarden.toml`.

The planner sees the links of every site that runs a core, plus any node the core crawls. For a whole-mesh picture, turn on `[crawl]` with `follow_links = true` in one core's config (agree the polling load with the node owners first). The map and the predictions need each node's location set in its AREDN settings; sites without one are listed under the map and still count everywhere else.

### Terrain

Predictions need 1 by 1 degree elevation tiles covering the mesh, in the SRTM `.hgt` format (big-endian 16-bit heights, 1201 or 3601 a side), named for their south-west corner: `N35W121.hgt`. Good sources:

- SRTM 1 arc-second from NASA Earthdata or USGS EarthExplorer (free account).
- Copernicus GLO-30, converted. Its tiles are 3600 samples a side and the SRTM format needs 3601, so resample while converting: `gdal_translate -of SRTMHGT -outsize 3601 3601 Copernicus_DSM_COG_10_N35_00_W121_00_DEM.tif N35W121.hgt`.
- The public terrain tiles on AWS (`elevation-tiles-prod`, `skadi` folder), already gzipped `.hgt`. Near the coast they carry sea-floor depths; anything deeper than 100 m is read as sea level.

The Terrain page lists the tiles this mesh needs. At any one site with internet, as the service's user so the files stay readable to it:

```sh
sudo -u ridgewarden ridgewarden-planner --config /etc/ridgewarden/planner.toml terrain import N35W121.hgt.gz N35W122.hgt.gz
```

Each tile is checked (size, byte order, voids), stored, and offered to every other site through the core's blob store at low priority. Other planners fetch the tiles that cover their own mesh within a few sync rounds. `ridgewarden-planner terrain list` shows what is on disk.

Both datasets are surface models: heights include trees and roofs, so give antenna heights above those. Heights are above the geoid (EGM96 for SRTM, EGM2008 for GLO-30); a GPS antenna altitude is above the WGS84 ellipsoid, so `alt_m` needs `radio.geoid_m`, the geoid height there.

## Configuration

`/etc/ridgewarden/planner.toml` (`init` writes a commented one):

```toml
core_url = "http://127.0.0.1:8790"
token_file = "/var/lib/ridgewarden/local.token"   # to share tiles and predictions; without it, read-only
core_data_dir = "/var/lib/ridgewarden"            # the core's data_dir
data_dir = "/var/lib/ridgewarden-planner"
listen = "0.0.0.0:8794"
fresh_s = 960       # link data older than this is stale; a few poll or crawl rounds

[site_names]
"tst-ridge" = "Ridge"

# services on a computer behind a node; the service list alone doesn't say which site
[host_sites]
"tst-pi" = "tst-ridge"

[terrain]
share = true        # offer tiles imported here to every other site
fetch_shared = true # take tiles other sites shared, when they cover this mesh
margin_km = 20      # how far around the sites and candidates terrain is wanted

[radio]
band_mhz = 5800
tx_dbm = 27
gain_dbi = 23
cable_db = 0
height_m = 10       # above the ground the tiles show
noise_dbm = -95     # for sites that haven't measured their noise floor
min_snr_db = 15     # what a link needs to count as workable
climate = "continental-temperate"
refractivity = 301
polarization = "vertical"
ground = "average"  # average, poor, good, fresh-water or sea-water
max_link_km = 50    # longest link the hilltop search proposes
geoid_m = -33       # EGM2008 geoid height here; only needed with alt_m

[radio.sites."tst-ridge"]
height_m = 18
gain_dbi = 30
# alt_m = 812       # the antenna's GPS altitude instead of height_m

[[region]]          # an area calibrated on its own, once it has 3 measured links
name = "Coast"
lat_min = 34.8
lat_max = 35.6
lon_min = -121.0
lon_max = -120.5

[[candidate]]
name = "Hill 1200"
lat = 35.41
lon = -120.60
links = ["tst-ridge", "tst-valley"]   # existing sites it could link to
note = "owner says yes; needs solar"
height_m = 12
access = "4wd"      # road, 4wd, foot or none
power = "solar"     # grid, solar or none
lease = "pending"   # yes, pending or no
owner = "County parks"
```

Reports are judged from this site's node: "cuts Valley off from Shack" means Shack can no longer reach Valley. If this site itself is taken out, the report is judged from the largest part left, and when a failure leaves no clear largest part it says the mesh splits. Candidate scores are judged against the largest part of the mesh instead, so every site ranks them the same.

Predictions are the median (50% of the time, locations and situations). The measured side is the median signal level each node reported for the link over the last week; a link's two ends are averaged. Paths whose ends lack a position or terrain are listed as not predicted, with the reason.

Links with no fresh data are assumed to still be up, so a link that merely went quiet isn't treated as broken. A link needs two polls in a row without its neighbor to count as down.

The pages have no login, like the other ridgewarden apps: anyone who can reach the port sees the map, the candidates and their notes. Keep `listen` on a LAN address, or leave out notes you wouldn't post on the mesh.

The app believes only events from stations the core trusts: its own key, plus the keys listed under `[trust]` in `ridgewarden.toml`.

## Building from source

This repo builds against the core's shared client crate and Link Doctor, so check them out next to it:

```sh
git clone https://github.com/Londopy/ridgewarden
git clone https://github.com/Londopy/ridgewarden-linkdoctor
git clone https://github.com/Londopy/ridgewarden-planner
cd ridgewarden-planner
cargo build --release
cargo test
```

## Contributing

Issues and pull requests are welcome. See [CONTRIBUTING.md](CONTRIBUTING.md), and add a line under `Unreleased` in [CHANGELOG.md](CHANGELOG.md) for anything a user would notice.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
