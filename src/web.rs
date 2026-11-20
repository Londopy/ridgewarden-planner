//! The pages: the network with its single points of failure, what if, candidate sites,
//! the hilltop search, links with their predictions and calibration, terrain, and the
//! GeoJSON, JSON and PDF exports. Server-rendered; every form works without JavaScript.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use ridgewarden_client::ui::{Color, Page, pill};
use ridgewarden_client::util::{esc, norm_node};
use ridgewarden_linkdoctor::links::LinkState;
use serde_json::{Value, json};

use crate::config::{Access, Candidate, Lease, MAX_NAME, Power};
use crate::impact::{self, Impact, Reach, Scenario, list};
use crate::map::Map;
use crate::net::{Net, Pos};
use crate::predict::Correction;
use crate::rank::{self, Filter, Predictor, Ranked};
use crate::record::{self, Measured};
use crate::terrain::tile_name;
use crate::{App, View, geo, hills, report, share};

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/", get(network_page))
        .route("/whatif", get(whatif_page))
        .route("/candidates", get(candidates_page))
        .route("/hilltops", get(hilltops_page))
        .route("/links", get(links_page))
        .route("/terrain", get(terrain_page))
        .route("/report.pdf", get(report_pdf))
        .route("/export.geojson", get(geojson))
        .route("/api/whatif", get(whatif_json))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(app)
}

struct Fail(anyhow::Error);

impl IntoResponse for Fail {
    fn into_response(self) -> Response {
        tracing::error!("request failed: {:#}", self.0);
        (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for Fail {
    fn from(e: E) -> Self {
        Fail(e.into())
    }
}

type Q = Query<Vec<(String, String)>>;

const FORM_CSS: &str = "<style>.checks{display:flex;flex-wrap:wrap;gap:4px 16px;margin:4px 0 10px}\
fieldset{border:1px solid var(--line);border-radius:6px;margin:10px 0}\
input[type=text],input:not([type]),select{font:inherit;padding:4px 6px;max-width:100%}\
form p{margin:6px 0}button{font:inherit;padding:5px 14px}</style>";

fn page(app: &App, here: &str, active: &str, title: &str, body: &str) -> Html<String> {
    let who = if here.is_empty() {
        String::new()
    } else {
        app.cfg.label(here)
    };
    let page = Page {
        app: "ridgewarden-planner",
        nav: &[
            ("/", "Network"),
            ("/whatif", "What if"),
            ("/candidates", "Candidates"),
            ("/hilltops", "Hilltops"),
            ("/links", "Links"),
            ("/terrain", "Terrain"),
        ],
        active,
        who: &who,
        refresh: false,
    };
    Html(page.render(title, body))
}

fn label_fn(app: &App) -> impl Fn(&str) -> String + '_ {
    |n: &str| app.cfg.label(n)
}

fn names(app: &App, sites: &BTreeSet<String>) -> String {
    list(&sites.iter().map(|n| app.cfg.label(n)).collect::<Vec<_>>())
}

fn opt(v: Option<f64>, unit: &str) -> String {
    v.map(|x| format!("{x:.0}{unit}"))
        .unwrap_or_else(|| "&ndash;".into())
}

fn param<'a>(q: &'a [(String, String)], k: &str) -> Option<&'a str> {
    q.iter()
        .find(|(x, _)| x == k)
        .map(|(_, v)| v.trim())
        .filter(|v| !v.is_empty())
}

/// The empty state, or None when there is something to show.
fn nothing_yet(app: &App, net: &Net) -> Option<String> {
    net.sites.is_empty().then(|| {
        format!(
            "<p>No sites yet. The planner builds its picture from the polls in the core's feed \
             ({}). Once the core has polled its node, and crawled the mesh if <code>[crawl]</code> \
             is on, sites show up here within a minute.</p>",
            esc(&app.cfg.core_url)
        )
    })
}

/// Links between sites without a core are invisible unless the core crawls them.
fn partial_banner(app: &App, net: &Net) -> String {
    let unseen: BTreeSet<String> = net.sites.difference(&net.observed).cloned().collect();
    if unseen.is_empty() {
        return String::new();
    }
    format!(
        "<p class=\"banner small\">Partial picture: {} of {} sites report their links ({} {}). \
         Links between sites that nobody polls are invisible, so a single point of failure can be \
         missed or one can be listed that isn't. Turn on <code>[crawl]</code> in one core's \
         config to fill it in.</p>",
        net.sites.len() - unseen.len(),
        net.sites.len(),
        esc(&names(app, &unseen)),
        if unseen.len() == 1 {
            "does not"
        } else {
            "do not"
        }
    )
}

fn map_html(
    app: &App,
    net: &Net,
    sc: &Scenario,
    lost: &BTreeSet<String>,
    cands: &[Candidate],
) -> String {
    let unplaced: BTreeSet<String> = net
        .sites
        .iter()
        .filter(|s| !net.pos.contains_key(*s))
        .cloned()
        .collect();
    let mut b = match (Map {
        net,
        cfg: &app.cfg,
        sc,
        lost,
        candidates: cands,
    })
    .render()
    {
        Some(svg) => format!(
            "{svg}<p class=\"mute small\">Click a site or link to see what happens if it fails{}. \
             Yellow dots and thick lines are single points of failure; dashed gray links have no \
             fresh data and are assumed up; red dotted links are down. The ringed site is this one.{}{}</p>",
            if sc.is_empty() {
                ""
            } else {
                ", or click it again to bring it back"
            },
            if sc.is_empty() {
                ""
            } else {
                " Hollow red sites and faded red links are taken out; red sites lose contact."
            },
            if cands.is_empty() {
                ""
            } else {
                " Diamonds are candidates, with their proposed links dotted."
            }
        ),
        None => {
            "<p class=mute>No site has reported a position, so there is no map. Set each node's \
                 location in its AREDN settings; the lists below work without it.</p>"
                .into()
        }
    };
    if !unplaced.is_empty() && unplaced.len() < net.sites.len() {
        let _ = write!(
            b,
            "<p class=\"mute small\">Not on the map (no position reported): {}.</p>",
            esc(&names(app, &unplaced))
        );
    }
    b
}

/// Predictions are possible once any terrain is on disk.
fn predictor<'a>(app: &'a App, v: &'a View) -> Option<Predictor<'a>> {
    let any = app.terrain.list().is_ok_and(|t| !t.is_empty());
    any.then_some(Predictor {
        terrain: &app.terrain,
        cfg: &app.cfg,
        cal: &v.cal,
    })
}

fn correction_words(c: &Correction) -> String {
    format!(
        "ITM was typically {:.1} dB {} over {} link{}{}{}",
        c.db.abs(),
        if c.db < 0.0 {
            "too optimistic"
        } else {
            "too pessimistic"
        },
        c.links,
        if c.links == 1 { "" } else { "s" },
        c.sd.map_or(String::new(), |sd| format!(", give or take {sd:.1} dB")),
        c.region
            .as_ref()
            .map_or(String::new(), |r| format!(" in {r}"))
    )
}

async fn network_page(State(app): State<Arc<App>>) -> Result<Html<String>, Fail> {
    let here = app.here().await;
    let v = crate::view(&app, &here).await?;
    let net = &v.net;
    let mut b = String::from("<h1>Network</h1>");
    if let Some(empty) = nothing_yet(&app, net) {
        b.push_str(&empty);
        return Ok(page(&app, &here, "/", "Network", &b));
    }
    let count = |s: LinkState| net.pairs.iter().filter(|p| p.state == s).count();
    let _ = write!(
        b,
        "<p>{} sites and {} links: {} up, {} with no fresh data, {} down. Judged from {}.</p>",
        net.sites.len(),
        net.pairs.len(),
        count(LinkState::Up),
        count(LinkState::Unknown),
        count(LinkState::Down),
        esc(&net.anchor().map(|a| app.cfg.label(a)).unwrap_or_else(|| {
            "the largest part of the mesh (this site's node isn't known yet)".into()
        }))
    );
    if !net.replicas.is_empty() {
        let _ = write!(
            b,
            "<p>The replicated chat (ridgewarden-failover) runs at {}.</p>",
            esc(&names(&app, &net.replicas))
        );
    }
    b.push_str(&partial_banner(&app, net));
    b.push_str(&map_html(
        &app,
        net,
        &Scenario::default(),
        &BTreeSet::new(),
        &[],
    ));

    b.push_str("<h2>Single points of failure</h2>");
    let mut risks: Vec<(Scenario, Impact)> = net
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
        .map(|sc| {
            let i = impact::impact(net, &sc);
            (sc, i)
        })
        .collect();
    if risks.is_empty() {
        b.push_str("<p>No single site or link failure would cut anyone off.</p>");
    } else {
        risks.sort_by(|x, y| {
            (y.1.lost.len() + y.1.lost_services().count())
                .cmp(&(x.1.lost.len() + x.1.lost_services().count()))
        });
        b.push_str("<ul class=plain>");
        let label = label_fn(&app);
        for (sc, i) in &risks {
            let _ = write!(
                b,
                "<li>{} <a href=\"/whatif?{}\" class=small>details</a></li>",
                esc(&i.sentences(sc, &label).join(" ")),
                sc.query().replace('&', "&amp;")
            );
        }
        b.push_str("</ul>");
    }
    b.push_str("<h2>Predictions</h2>");
    match &v.cal.mesh {
        Some(c) => {
            let _ = write!(
                b,
                "<p>{}. <a href=\"/links\">Each link</a></p>",
                esc(&correction_words(c))
            );
        }
        None => b.push_str(
            "<p class=mute>No measured link has terrain under it yet, so nothing is predicted or \
             calibrated. <a href=\"/terrain\">Add terrain</a>.</p>",
        ),
    }
    b.push_str(
        "<h2>Export</h2><p><a href=\"/export.geojson\">Download GeoJSON</a> of the sites, links and \
         configured candidates, for QGIS, uMap or any other map tool. Sites and links without a \
         position are included with an empty geometry.</p>\
         <p><a href=\"/report.pdf\">Download the PDF report</a>: the mesh, its single points of \
         failure, prediction accuracy and the candidates ranked, for a grant application or a \
         meeting.</p>",
    );
    Ok(page(&app, &here, "/", "Network", &b))
}

fn reach_color(r: Reach) -> Color {
    match r {
        Reach::Reachable => Color::Green,
        Reach::SiteDown | Reach::CutOff => Color::Red,
        Reach::AlreadyCut => Color::Yellow,
        Reach::Unplaced => Color::Unknown,
    }
}

fn result_html(app: &App, sc: &Scenario, i: &Impact) -> String {
    let label = label_fn(app);
    let mut b = String::from("<h2>Result</h2>");
    for s in i.sentences(sc, &label) {
        let _ = write!(b, "<p>{}</p>", esc(&s));
    }
    if i.services.is_empty() {
        b.push_str("<p class=\"mute small\">No advertised services are known yet.</p>");
        return b;
    }
    let from = i
        .from
        .as_deref()
        .map(|f| app.cfg.label(f))
        .unwrap_or_else(|| "the largest part".into());
    let _ = write!(
        b,
        "<h3>Services, from {}</h3><div class=wrap><table><tr><th>Service</th><th>Site</th><th>Reachable?</th></tr>",
        esc(&from)
    );
    let mut rows: Vec<_> = i.services.iter().collect();
    rows.sort_by(|x, y| x.1.cmp(&y.1).then_with(|| x.0.svc.name.cmp(&y.0.svc.name)));
    for (p, r) in rows {
        let site = match &p.site {
            Some(s) => esc(&app.cfg.label(s)),
            None => format!("<span class=mute>{}</span>", esc(&p.svc.host)),
        };
        let _ = write!(
            b,
            "<tr><td>{}</td><td>{site}</td><td>{}</td></tr>",
            esc(&p.svc.name),
            pill(reach_color(*r), r.word())
        );
    }
    b.push_str("</table></div>");
    if i.services.iter().any(|(_, r)| *r == Reach::Unplaced) {
        b.push_str(
            "<p class=\"mute small\">\"Site unknown\": the service runs on a computer behind a node, \
             and the service list doesn't say which. Add it under <code>[host_sites]</code> in the \
             planner's config.</p>",
        );
    }
    b
}

fn checkbox(name: &str, value: &str, text: &str, on: bool) -> String {
    format!(
        "<label><input type=checkbox name={name} value=\"{}\"{}> {}</label>",
        esc(value),
        if on { " checked" } else { "" },
        esc(text)
    )
}

async fn whatif_page(State(app): State<Arc<App>>, Query(q): Q) -> Result<Html<String>, Fail> {
    let here = app.here().await;
    let v = crate::view(&app, &here).await?;
    let net = &v.net;
    let mut b = format!("{FORM_CSS}<h1>What if</h1>");
    if let Some(empty) = nothing_yet(&app, net) {
        b.push_str(&empty);
        return Ok(page(&app, &here, "/whatif", "What if", &b));
    }
    let sc = Scenario::from_query(&q, net);
    let from = net
        .anchor()
        .map(|a| app.cfg.label(a))
        .unwrap_or_else(|| "the largest part of the mesh".into());
    let _ = write!(
        b,
        "<p>Take out any sites and links, on the map or with the boxes below, to see who loses \
         contact with {} and which advertised services go out of reach. It starts from the links \
         working now; a link with no fresh data counts as working.</p>",
        esc(&from)
    );
    let lost = if sc.is_empty() {
        BTreeSet::new()
    } else {
        let i = impact::impact(net, &sc);
        b.push_str(&result_html(&app, &sc, &i));
        i.lost
    };
    b.push_str(&partial_banner(&app, net));
    b.push_str(&map_html(&app, net, &sc, &lost, &[]));

    b.push_str(
        "<form method=get action=\"/whatif\"><fieldset><legend>Sites</legend><div class=checks>",
    );
    for s in &net.sites {
        b.push_str(&checkbox(
            "site",
            s,
            &app.cfg.label(s),
            sc.sites.contains(s),
        ));
    }
    b.push_str("</div></fieldset><fieldset><legend>Working links (up, or no fresh data)</legend><div class=checks>");
    for e in net.topo.graph.edges() {
        let text = format!("{} - {}", app.cfg.label(&e.0), app.cfg.label(&e.1));
        let value = format!("{}|{}", e.0, e.1);
        b.push_str(&checkbox("link", &value, &text, sc.links.contains(&e)));
    }
    b.push_str("</div></fieldset><p><button type=submit>Show</button> <a href=\"/whatif\">Clear</a></p></form>");
    Ok(page(&app, &here, "/whatif", "What if", &b))
}

async fn whatif_json(State(app): State<Arc<App>>, Query(q): Q) -> Result<Json<Value>, Fail> {
    let here = app.here().await;
    let v = crate::view(&app, &here).await?;
    let net = &v.net;
    let sc = Scenario::from_query(&q, net);
    let i = impact::impact(net, &sc);
    let label = label_fn(&app);
    let report = if sc.is_empty() {
        vec![]
    } else {
        i.sentences(&sc, &label)
    };
    Ok(Json(json!({
        "generated": net.now,
        "sites": sc.sites,
        "links": sc.links,
        "from": i.from,
        "split": i.split,
        "lost": i.lost,
        "islands": i.islands,
        "already_cut_off": i.already,
        "report": report,
        "services": i.services.iter().map(|(p, r)| json!({
            "name": p.svc.name, "link": p.svc.link, "site": p.site,
            "reachable": *r == Reach::Reachable, "why": r.word(),
        })).collect::<Vec<_>>(),
        "replicated_chat": i.replicas.as_ref().map(|(before, after)| json!({
            "reachable_before": before, "reachable_after": after,
        })),
    })))
}

/// A candidate typed into the form, or why it can't be scored.
fn entered(q: &[(String, String)], configured: &[Candidate]) -> Option<Result<Candidate, String>> {
    let name = param(q, "name")?;
    let num = |k: &str, what: &str| -> Result<f64, String> {
        param(q, k)
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| v.is_finite())
            .ok_or_else(|| format!("The {what} must be a number in decimal degrees, like 35.33."))
    };
    Some((|| {
        let mut c = Candidate::at(
            name.to_string(),
            num("lat", "latitude")?,
            num("lon", "longitude")?,
            q.iter()
                .filter(|(k, _)| k == "link")
                .map(|(_, v)| v.clone())
                .collect(),
        );
        if let Some(h) = param(q, "height") {
            c.height_m = Some(
                h.parse::<f64>()
                    .map_err(|_| "The antenna height must be a number of metres.".to_string())?,
            );
        }
        let c = c.checked().map_err(|e| format!("{e}."))?;
        if configured.iter().any(|x| x.name == c.name) {
            return Err(format!(
                "A configured candidate is already called {:?}.",
                c.name
            ));
        }
        Ok(c)
    })())
}

/// The filter set in the query, from the `access`, `power`, `lease` and `owner` boxes.
fn filter(q: &[(String, String)]) -> Filter {
    Filter {
        access: param(q, "access").and_then(|v| match v {
            "road" => Some(Access::Road),
            "4wd" => Some(Access::Offroad),
            "foot" => Some(Access::Foot),
            _ => None,
        }),
        power: param(q, "power").and_then(|v| match v {
            "grid" => Some(Power::Grid),
            "solar" => Some(Power::Solar),
            _ => None,
        }),
        lease: param(q, "lease").and_then(|v| match v {
            "yes" => Some(Lease::Yes),
            "pending" => Some(Lease::Pending),
            _ => None,
        }),
        owner: param(q, "owner").unwrap_or("").chars().take(100).collect(),
    }
}

fn facts(c: &Candidate) -> Vec<String> {
    let mut f = vec![format!("{:.4}, {:.4}", c.lat, c.lon)];
    if let Some(h) = c.height_m {
        f.push(format!("antenna {h:.0} m up"));
    }
    f.push(c.access.map_or("access unknown", |a| a.word()).to_string());
    f.push(c.power.map_or("power unknown", |p| p.word()).to_string());
    f.push(c.lease.map_or("lease unknown", |l| l.word()).to_string());
    if !c.owner.is_empty() {
        f.push(format!("owner: {}", c.owner));
    }
    f
}

fn ranked_html(app: &App, i: usize, r: &Ranked) -> String {
    let l = |n: &str| app.cfg.label(n);
    let mut b = format!(
        "<h3>{}. {}{}</h3><p class=\"mute small\">{}</p>",
        i + 1,
        esc(&r.cand.name),
        if r.entered {
            " <span class=\"mute small\">(from the form, not saved)</span>"
        } else {
            ""
        },
        esc(&facts(&r.cand).join("; "))
    );
    if !r.cand.note.is_empty() {
        let _ = write!(b, "<p class=mute>{}</p>", esc(&r.cand.note));
    }
    let mut lines = vec![];
    lines.push(if r.protected.is_empty() {
        "Protects no site.".to_string()
    } else {
        format!(
            "Protects {}: no single failure could cut {} off any more.",
            names(app, &r.protected),
            if r.protected.len() == 1 { "it" } else { "them" }
        )
    });
    if !r.reconnects.is_empty() {
        lines.push(format!(
            "Would reconnect {}, cut off now.",
            names(app, &r.reconnects)
        ));
    }
    let removed: Vec<String> = r
        .removed_sites
        .iter()
        .map(|s| l(s))
        .chain(
            r.removed_links
                .iter()
                .map(|(a, c)| format!("the {} - {} link", l(a), l(c))),
        )
        .collect();
    lines.push(match removed.len() {
        0 => "Removes no single point of failure.".into(),
        1 => format!("Removes 1 single point of failure: {}.", list(&removed)),
        n => format!("Removes {n} single points of failure: {}.", list(&removed)),
    });
    if !r.still_exposed.is_empty() {
        lines.push(format!(
            "Still exposed to a single failure: {}.",
            names(app, &r.still_exposed)
        ));
    }
    if r.counted() == 1 {
        lines.push(
            "With one working link it would hang off that site, so it adds no redundancy.".into(),
        );
    }
    if !r.unknown.is_empty() {
        lines.push(format!(
            "Not in the mesh, so left out: {}.",
            list(&r.unknown)
        ));
    }
    for s in lines {
        let _ = write!(b, "<p>{}</p>", esc(&s));
    }
    if !r.links.is_empty() {
        b.push_str("<ul class=\"plain small\">");
        for link in &r.links {
            let km = link
                .km
                .map_or("position unknown".to_string(), |k| format!("{k:.1} km"));
            let pred = match &link.guess {
                Some(g) => format!("{}; {}", g.pred.mode_word(), g.words()),
                None if link.km.is_none() => "not predicted: the site's position is unknown".into(),
                None => "not predicted: no terrain for this path".into(),
            };
            let verdict = match (&link.guess, link.counted) {
                (Some(_), true) => pill(Color::Green, "should work"),
                (Some(_), false) => pill(Color::Red, "too weak, not counted"),
                (None, _) => pill(Color::Unknown, "assumed to work"),
            };
            let _ = write!(
                b,
                "<li>{verdict} to <b>{}</b>, {}: {}</li>",
                esc(&l(&link.site)),
                esc(&km),
                esc(&pred)
            );
        }
        b.push_str("</ul>");
    }
    b
}

fn select(name: &str, current: Option<&str>, options: &[(&str, &str)]) -> String {
    let mut s = format!("<select name={name}>");
    for (v, text) in options {
        let sel = if current == Some(*v) { " selected" } else { "" };
        let _ = write!(s, "<option value=\"{v}\"{sel}>{}</option>", esc(text));
    }
    s.push_str("</select>");
    s
}

fn filter_form(q: &[(String, String)]) -> String {
    format!(
        "<form method=get action=\"/candidates\"><fieldset><legend>Only show candidates with</legend>\
         <p><label>Access {}</label> <label>Power {}</label> <label>Lease {}</label> \
         <label>Owner contains <input name=owner size=14 value=\"{}\"></label> \
         <button type=submit>Filter</button> <a href=\"/candidates\">Clear</a></p>\
         <p class=\"mute small\">A candidate whose access, power or lease isn't known is left out by \
         that filter. Set them with <code>access</code>, <code>power</code>, <code>lease</code> and \
         <code>owner</code> in its <code>[[candidate]]</code>.</p></fieldset></form>",
        select(
            "access",
            param(q, "access"),
            &[
                ("", "any"),
                ("foot", "foot or better"),
                ("4wd", "4WD or road"),
                ("road", "road")
            ]
        ),
        select(
            "power",
            param(q, "power"),
            &[("", "any"), ("solar", "solar or grid"), ("grid", "grid")]
        ),
        select(
            "lease",
            param(q, "lease"),
            &[
                ("", "any"),
                ("pending", "pending or agreed"),
                ("yes", "agreed")
            ]
        ),
        esc(param(q, "owner").unwrap_or(""))
    )
}

fn candidate_form(net: &Net, app: &App, q: &[(String, String)]) -> String {
    let val = |k: &str| esc(param(q, k).unwrap_or(""));
    let picked: BTreeSet<String> = q
        .iter()
        .filter(|(k, _)| k == "link")
        .map(|(_, v)| norm_node(v))
        .collect();
    let mut b = format!(
        "<h2>Try a site</h2><form method=get action=\"/candidates\">\
         <p><label>Name <input name=name maxlength={MAX_NAME} value=\"{}\" required></label></p>\
         <p><label>Latitude <input name=lat inputmode=decimal size=10 value=\"{}\" required></label> \
         <label>Longitude <input name=lon inputmode=decimal size=10 value=\"{}\" required></label> \
         <label>Antenna height, m <input name=height inputmode=decimal size=5 value=\"{}\" placeholder=\"{:.0}\"></label></p>\
         <fieldset><legend>Sites it could link to</legend><div class=checks>",
        val("name"),
        val("lat"),
        val("lon"),
        val("height"),
        app.cfg.radio.height_m
    );
    for s in &net.sites {
        b.push_str(&checkbox("link", s, &app.cfg.label(s), picked.contains(s)));
    }
    b.push_str(
        "</div></fieldset><p><button type=submit>Score it</button> <a href=\"/candidates\">Clear</a></p>\
         <p class=\"mute small\">Nothing is saved. To keep a candidate, add a <code>[[candidate]]</code> \
         to the planner's config.</p></form>",
    );
    b
}

const SCORING: &str = "<p>Each candidate is scored on its own against the links working now: the \
    sites it protects (no single site or link failure could cut them off any more) and the single \
    points of failure it removes. This is judged against the largest part of the mesh, so it reads \
    the same at every site. Where there is terrain, each proposed link is predicted with ITM and \
    corrected by what the existing links measure; a link predicted below the workable SNR isn't \
    counted.</p>";

async fn candidates_page(State(app): State<Arc<App>>, Query(q): Q) -> Result<Html<String>, Fail> {
    let here = app.here().await;
    let v = crate::view(&app, &here).await?;
    let net = &v.net;
    let mut b = format!("{FORM_CSS}<h1>Candidates</h1>");
    if let Some(empty) = nothing_yet(&app, net) {
        b.push_str(&empty);
        return Ok(page(&app, &here, "/candidates", "Candidates", &b));
    }
    b.push_str(SCORING);
    let exposed = rank::Spofs::of(&net.topo.graph, None).exposed();
    let _ = write!(
        b,
        "<p>{}</p>",
        esc(&if exposed.is_empty() {
            "Right now no single failure would cut any site off.".to_string()
        } else {
            format!(
                "Right now a single failure could cut off {}.",
                names(&app, &exposed)
            )
        })
    );
    b.push_str(&partial_banner(&app, net));
    let f = filter(&q);
    let mut cands: Vec<(Candidate, bool)> = app
        .cfg
        .candidates
        .iter()
        .filter(|c| f.keeps(c))
        .map(|c| (c.clone(), false))
        .collect();
    let hidden = app.cfg.candidates.len() - cands.len();
    match entered(&q, &app.cfg.candidates) {
        Some(Ok(c)) => cands.push((c, true)),
        Some(Err(e)) => {
            let _ = write!(b, "<p class=banner>{}</p>", esc(&e));
        }
        None => {}
    }
    if !app.cfg.candidates.is_empty() {
        b.push_str(&filter_form(&q));
    }
    if hidden > 0 {
        let _ = write!(
            b,
            "<p class=mute>{hidden} configured candidate{} hidden by the filter.</p>",
            if hidden == 1 { " is" } else { "s are" }
        );
    }
    if cands.is_empty() {
        b.push_str(
            "<p class=mute>No candidates to rank. Try one with the form below, add \
             <code>[[candidate]]</code> entries to the planner's config, or let the \
             <a href=\"/hilltops\">hilltop search</a> find some.</p>",
        );
    } else {
        b.push_str("<h2>Ranked</h2>");
        let plain: Vec<Candidate> = cands.iter().map(|(c, _)| c.clone()).collect();
        let pred = predictor(&app, &v);
        if pred.is_none() {
            b.push_str(
                "<p class=\"mute small\">No terrain yet, so links are assumed to work. \
                 <a href=\"/terrain\">Add terrain</a> to predict them.</p>",
            );
        }
        for (i, r) in rank::rank(net, cands, pred.as_ref()).iter().enumerate() {
            b.push_str(&ranked_html(&app, i, r));
        }
        b.push_str(&map_html(
            &app,
            net,
            &Scenario::default(),
            &BTreeSet::new(),
            &plain,
        ));
    }
    b.push_str(&candidate_form(net, &app, &q));
    Ok(page(&app, &here, "/candidates", "Candidates", &b))
}

/// How many hilltops to try, and how many of the best to show.
const HILLS_TRIED: usize = 25;
const HILLS_SHOWN: usize = 10;

async fn hilltops_page(State(app): State<Arc<App>>, Query(q): Q) -> Result<Html<String>, Fail> {
    let here = app.here().await;
    let v = crate::view(&app, &here).await?;
    let mut b = format!(
        "{FORM_CSS}<h1>Hilltops</h1><p>The highest points in the terrain within {:.0} km of the \
         mesh, each tried as a new site linked to every site within {:.0} km. Links predicted \
         below {:.0} dB SNR are dropped, and the rest are ranked like any candidate: by the sites \
         they protect from a single failure. Whether a hill has road access, power or a willing \
         owner is unknown; that is for someone to go and find out.</p>",
        app.cfg.terrain.margin_km, app.cfg.radio.max_link_km, app.cfg.radio.min_snr_db
    );
    if let Some(empty) = nothing_yet(&app, &v.net) {
        b.push_str(&empty);
        return Ok(page(&app, &here, "/hilltops", "Hilltops", &b));
    }
    if predictor(&app, &v).is_none() {
        b.push_str(
            "<p class=banner>There is no terrain yet. <a href=\"/terrain\">Add terrain tiles</a> \
             covering the mesh first.</p>",
        );
        return Ok(page(&app, &here, "/hilltops", "Hilltops", &b));
    }
    if param(&q, "search").is_none() {
        b.push_str(
            "<form method=get action=\"/hilltops\"><p><button type=submit name=search value=1>Search \
             the terrain</button> <span class=\"mute small\">This predicts a few hundred paths and \
             can take a minute on a Pi.</span></p></form>",
        );
        return Ok(page(&app, &here, "/hilltops", "Hilltops", &b));
    }
    let a = app.clone();
    let view = v.clone();
    let ranked: Vec<String> = tokio::task::spawn_blocking(move || {
        let located: Vec<Pos> = view.net.pos.values().copied().collect();
        let found = hills::find(
            &a.terrain,
            &located,
            a.cfg.terrain.margin_km,
            a.cfg.radio.max_link_km,
            HILLS_TRIED,
        );
        let cands = hills::candidates(&found, &view.net, a.cfg.radio.max_link_km);
        let pred = predictor(&a, &view);
        rank::rank(
            &view.net,
            cands.into_iter().map(|c| (c, false)).collect(),
            pred.as_ref(),
        )
        .iter()
        .filter(|r| r.counted() >= 2)
        .take(HILLS_SHOWN)
        .enumerate()
        .map(|(i, r)| ranked_html(&a, i, r))
        .collect()
    })
    .await?;
    if ranked.is_empty() {
        b.push_str(
            "<p>None of the highest hilltops within reach of two or more sites predicts workable \
             links to two of them, so none would add redundancy.</p>",
        );
    } else {
        let _ = write!(
            b,
            "<h2>Best {} hilltops</h2><p class=\"mute small\">Only hills with at least two \
             workable links are shown, since one link adds no redundancy. Copy a promising one \
             into a <code>[[candidate]]</code> with what you learn about it, so the filters can \
             use it.</p>",
            ranked.len()
        );
        b.extend(ranked);
    }
    Ok(page(&app, &here, "/hilltops", "Hilltops", &b))
}

fn end_cells(m: &Measured, node: &str) -> String {
    match m.ends.iter().find(|e| e.node == node) {
        Some(e) => format!(
            "<td>{}</td><td>{}</td><td>{}</td>",
            opt(e.snr, " dB"),
            opt(e.signal, " dBm"),
            e.n
        ),
        None => "<td colspan=3 class=mute>not polled</td>".into(),
    }
}

fn calibration_html(app: &App, v: &View) -> String {
    let cal = &v.cal;
    let l = |n: &str| esc(&app.cfg.label(n));
    let mut b = String::from("<h2>Predicted and measured</h2>");
    let Some(mesh) = &cal.mesh else {
        b.push_str(
            "<p class=mute>No measured link has terrain under it yet, so there is nothing to \
             compare. <a href=\"/terrain\">Add terrain</a> covering the mesh.</p>",
        );
        return b;
    };
    let _ = write!(
        b,
        "<p>Each RF link is predicted with ITM over the terrain, using the transmit power, gains \
         and heights in the <code>[radio]</code> config, and compared with the signal level its \
         two ends measured over the last week. {}, over {} readings; that is the correction applied \
         to new paths, and the give or take is how far one link may still be off.</p>",
        esc(&correction_words(mesh)),
        mesh.samples
    );
    for r in &cal.regions {
        let _ = write!(b, "<p>{}.</p>", esc(&correction_words(r)));
    }
    match &cal.loo {
        Some(loo) => {
            let _ = write!(b, "<p><b>{}</b></p>", esc(&loo.words()));
        }
        None => b.push_str(
            "<p class=mute>With fewer than three links measured over terrain, the gain from \
             calibration can't be checked yet.</p>",
        ),
    }
    b.push_str(
        "<div class=wrap><table><tr><th>Link</th><th>Length</th><th>Path</th><th>End</th>\
         <th>Predicted</th><th>Measured</th><th>Off by</th></tr>",
    );
    for lc in &cal.links {
        for (i, e) in lc.ends.iter().enumerate() {
            let head = if i == 0 {
                format!(
                    "<td rowspan={n}>{} &harr; {}</td><td rowspan={n}>{:.1} km</td><td rowspan={n}>{}<br><span class=\"mute small\">antennas {:.0} and {:.0} m up</span></td>",
                    l(&lc.a),
                    l(&lc.b),
                    lc.pred.km,
                    esc(lc.pred.mode_word()),
                    lc.pred.h_a,
                    lc.pred.h_b,
                    n = lc.ends.len()
                )
            } else {
                String::new()
            };
            let _ = write!(
                b,
                "<tr>{head}<td>{} hears {}</td><td>{:.0} dBm</td><td>{:.0} dBm{}<br><span class=\"mute small\">{} readings</span></td><td>{:+.1} dB</td></tr>",
                l(&e.rx),
                l(&e.tx),
                e.predicted,
                e.measured,
                e.spread
                    .map_or(String::new(), |s| format!(" &plusmn; {s:.0}")),
                e.samples,
                e.residual
            );
        }
    }
    b.push_str("</table></div>");
    if !cal.skipped.is_empty() {
        let s: Vec<String> = cal
            .skipped
            .iter()
            .map(|(a, c, why)| format!("{} - {} ({why})", app.cfg.label(a), app.cfg.label(c)))
            .collect();
        let _ = write!(
            b,
            "<p class=\"mute small\">Not predicted: {}.</p>",
            esc(&list(&s))
        );
    }
    b.push_str(if app.can_write {
        "<p class=\"mute small\">Each link's predicted SNR, corrected by the other links only, \
         goes to Link Doctor's link pages as a <code>planner.prediction</code> event: again \
         when it moves by more than 1 dB, and once a day.</p>"
    } else {
        "<p class=\"mute small\">Not shared with Link Doctor: the planner can't read the core's \
         token (<code>token_file</code>), so it is read-only.</p>"
    });
    b
}

async fn links_page(State(app): State<Arc<App>>) -> Result<Html<String>, Fail> {
    let here = app.here().await;
    let v = crate::view(&app, &here).await?;
    let label = label_fn(&app);
    let mut b = String::from("<h1>Links</h1>");
    if v.measured.is_empty() {
        b.push_str("<p class=mute>No RF link samples in the last week yet.</p>");
        return Ok(page(&app, &here, "/links", "Links", &b));
    }
    b.push_str(&calibration_html(&app, &v));
    let _ = write!(
        b,
        "<h2>Measured against length</h2><p>What each RF link measured over the last {} days. \
         Free-space loss at {:.0} MHz is a baseline: what a perfectly clear path loses to \
         distance alone.</p>",
        record::WINDOW / ridgewarden_linkdoctor::history::DAY,
        app.cfg.radio.band_mhz
    );
    b.push_str(&record::scatter(&v.measured, &label));
    b.push_str(
        "<div class=wrap><table><tr><th>Link</th><th>Length</th><th>Free-space loss</th>\
         <th colspan=3>A hears B: SNR, signal, readings</th><th colspan=3>B hears A</th></tr>",
    );
    for m in &v.measured {
        let _ = write!(
            b,
            "<tr><td>{} (A) &harr; {} (B)</td><td>{}</td><td>{}</td>{}{}</tr>",
            esc(&label(&m.a)),
            esc(&label(&m.b)),
            m.km.map_or("position unknown".into(), |k| format!("{k:.1} km")),
            opt(m.fspl, " dB"),
            end_cells(m, &m.a),
            end_cells(m, &m.b)
        );
    }
    b.push_str(
        "</table></div><p class=\"mute small\">Medians over the window. A node only reports \
         signal and SNR for links it hears over RF; DtD and tunnel links are left out.</p>",
    );
    Ok(page(&app, &here, "/links", "Links", &b))
}

async fn terrain_page(State(app): State<Arc<App>>) -> Result<Html<String>, Fail> {
    let here = app.here().await;
    let v = crate::view(&app, &here).await?;
    let have = app.terrain.list()?;
    let need = share::needed(&app, &v.net);
    let shared = share::shared(&app.db())?;
    let mut b = format!(
        "<h1>Terrain</h1><p>Predictions need elevation tiles covering the mesh: 1 by 1 degree \
         <code>.hgt</code> files (SRTM, or Copernicus GLO-30 converted with \
         <code>gdal_translate -of SRTMHGT -outsize 3601 3601</code>), named for their south-west corner like \
         <code>N35W121.hgt</code>, gzipped or not. Import them at any one site with \
         <code>ridgewarden-planner terrain import FILE...</code>; {}</p>",
        if app.cfg.terrain.share && app.can_write {
            "that site's core then shares them over the mesh, and every planner that needs a tile \
             fetches it, so the internet is needed only once."
        } else if !app.can_write {
            "sharing over the mesh is off here because the core's token isn't readable \
             (<code>token_file</code>)."
        } else {
            "sharing over the mesh is turned off here (<code>[terrain] share</code>)."
        }
    );
    b.push_str(
        "<p class=\"mute small\">Both datasets are surface models: heights include trees and \
         roofs, so set antenna heights above those. Heights are above the geoid; a GPS altitude \
         (<code>alt_m</code>) is converted with <code>radio.geoid_m</code>.</p>",
    );
    if need.is_empty() {
        b.push_str(
            "<p class=mute>No site has reported a position, so no tiles are needed yet.</p>",
        );
    } else {
        b.push_str(
            "<h2>Tiles this mesh needs</h2><div class=wrap><table><tr><th>Tile</th><th>State</th></tr>",
        );
        for (lat, lon) in &need {
            let state = if have.contains(&(*lat, *lon)) {
                pill(Color::Green, "here")
            } else if let Some(t) = shared.get(&(*lat, *lon)) {
                format!(
                    "{} shared by {}, {:.0} MB; on its way",
                    pill(Color::Yellow, "arriving"),
                    esc(&t.call),
                    t.size as f64 / 1e6
                )
            } else {
                pill(Color::Red, "missing")
            };
            let _ = write!(
                b,
                "<tr><td><code>{}</code></td><td>{state}</td></tr>",
                tile_name(*lat, *lon)
            );
        }
        b.push_str("</table></div>");
    }
    let extra: Vec<String> = have
        .iter()
        .filter(|t| !need.contains(t))
        .map(|(a, o)| tile_name(*a, *o))
        .collect();
    if !extra.is_empty() {
        let _ = write!(
            b,
            "<p class=\"mute small\">Also on disk, outside the mesh's area: {}.</p>",
            esc(&extra.join(", "))
        );
    }
    Ok(page(&app, &here, "/terrain", "Terrain", &b))
}

async fn report_pdf(State(app): State<Arc<App>>) -> Result<Response, Fail> {
    let here = app.here().await;
    let v = crate::view(&app, &here).await?;
    let a = app.clone();
    let body = tokio::task::spawn_blocking(move || {
        let pred = predictor(&a, &v);
        let ranked = rank::rank(
            &v.net,
            a.cfg
                .candidates
                .iter()
                .map(|c| (c.clone(), false))
                .collect(),
            pred.as_ref(),
        );
        report::build(&v, &a.cfg, &ranked)
    })
    .await?;
    Ok((
        [
            (header::CONTENT_TYPE, "application/pdf"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"mesh-plan.pdf\"",
            ),
        ],
        body,
    )
        .into_response())
}

async fn geojson(State(app): State<Arc<App>>) -> Result<Response, Fail> {
    let here = app.here().await;
    let v = crate::view(&app, &here).await?;
    let body = geo::geojson(&v.net, &app.cfg, &v.cal).to_string();
    Ok((
        [
            (header::CONTENT_TYPE, "application/geo+json"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"mesh.geojson\"",
            ),
        ],
        body,
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn entered_candidates_are_checked() {
        assert!(entered(&q(&[]), &[]).is_none(), "no form, no candidate");
        let c = entered(
            &q(&[
                ("name", " Hill "),
                ("lat", "35.4"),
                ("lon", "-120.6"),
                ("height", "12"),
                ("link", "TST-Cuesta"),
            ]),
            &[],
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            (c.name.as_str(), c.links[0].as_str(), c.height_m),
            ("Hill", "tst-cuesta", Some(12.0))
        );
        let err = entered(&q(&[("name", "Hill"), ("lat", "north"), ("lon", "1")]), &[])
            .unwrap()
            .unwrap_err();
        assert!(err.contains("latitude"), "{err}");
        let err = entered(&q(&[("name", "Hill"), ("lat", "35"), ("lon", "-120")]), &[])
            .unwrap()
            .unwrap_err();
        assert!(err.contains("at least one site"), "{err}");
        let err = entered(
            &q(&[
                ("name", "Hill"),
                ("lat", "35"),
                ("lon", "-120"),
                ("link", "a"),
            ]),
            &[c],
        )
        .unwrap()
        .unwrap_err();
        assert!(err.contains("already called"), "{err}");
    }

    #[test]
    fn filters_come_from_the_query() {
        let f = filter(&q(&[
            ("access", "4wd"),
            ("power", ""),
            ("lease", "yes"),
            ("owner", " parks "),
        ]));
        assert_eq!(f.access, Some(Access::Offroad));
        assert_eq!(f.power, None);
        assert_eq!(f.lease, Some(Lease::Yes));
        assert_eq!(f.owner, "parks");
        assert!(filter(&q(&[("access", "<script>")])).is_empty());
    }
}
