# Changelog

All notable changes to ridgewarden-planner are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). The section for a version is
what appears on its GitHub release page. Validated in CI by
[patchnotes](https://github.com/Londopy/patchnotes).

## [Unreleased]

### Added

- Runs as its own system user, `ridgewarden-planner`, in the core's `ridgewarden` group: it can sign events through the core but not read the station key.
- The live mesh as a graph from the core's polls and crawl, drawn as a map from each node's reported position, with no JavaScript and no map tiles.
- Single points of failure, worst first, each with what it would cut off and which services would go out of reach.
- What if: take out any sites and links by clicking the map or ticking boxes, and get a plain-words report of who loses contact with this site, which of them can still reach each other, and which advertised services become unreachable.
- Candidate sites, from the config or typed into a form, ranked by the sites they protect from any single failure and the single points of failure they remove.
- A Links page with each RF link's measured SNR and signal against its length, and free-space path loss as a labelled baseline.
- GeoJSON export of sites, links and candidates.
- `/api/whatif` for the what-if report as JSON.
- `[host_sites]` to place services that run on a computer behind a node.
- Terrain predictions with ITM (Longley-Rice), ported from NTIA's reference code and checked against NTIA's example cases.
- Calibration against the live links: each measured link's predicted and measured signal, a correction per configured region and for the whole mesh with its spread as the error bar, and how much it shrinks the error, predicting each link from the others.
- Candidate links predicted and corrected, with links too weak to work left out of the score.
- Candidate filters for access, power, lease and owner, and an antenna height per candidate.
- A hilltop search that finds high points around the mesh and ranks them by the redundancy they would add.
- Terrain from SRTM-format `.hgt` tiles, gzipped or not, imported with `terrain import` and shared with the other sites over the mesh through the core's blob store.
- GPS antenna altitudes (`alt_m`) converted with the local geoid height, since the tiles are on the geoid and GPS is on the ellipsoid.
- How many copies of the replicated chat (ridgewarden-failover) a failure leaves reachable.
- A PDF report: the mesh, a map, single points of failure, prediction accuracy and the candidates ranked.
- The split case: a failure that leaves no clear largest part says the mesh splits.
- `planner.prediction` events for each measured RF link, corrected by the other links only, so Link Doctor can show predicted against measured; republished when a prediction moves by more than 1 dB, or daily. Without the core's token the planner stays read-only and says so.

[Unreleased]: https://github.com/Londopy/ridgewarden-planner/commits/main
