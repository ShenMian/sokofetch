//! Minimal client for the public sokobanonline.com REST API (see docs/API.md).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, bail};
use indicatif::ProgressBar;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use ureq::Agent;

const BASE: &str = "https://www.sokobanonline.com/api/v1";
const USER_AGENT: &str = "Mozilla/5.0 (personal level archiver; one-off fetch)";
const MAX_ATTEMPTS: u32 = 4;

/// The requested pack/author page does not exist.
#[derive(Debug)]
pub struct NotFound(pub String);

impl std::fmt::Display for NotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "not found: {}", self.0)
    }
}

impl std::error::Error for NotFound {}

#[derive(Debug, Deserialize)]
struct Meta {
    total: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct Envelope<D> {
    #[serde(default)]
    status: String,
    meta: Option<Meta>,
    data: D,
}

/// Pack metadata from the listing endpoint.
#[derive(Debug, Clone, Deserialize)]
pub struct Pack {
    pub name: String,
    #[serde(rename = "fullslug")]
    pub full_slug: String,
    pub path: String,
}

/// Author info embedded in a puzzle detail.
#[derive(Debug, Clone, Deserialize)]
pub struct Author {
    #[serde(default, rename = "displayname")]
    pub display_name: Option<String>,
}

/// One puzzle entry of a pack listing (no board data).
#[derive(Debug, Clone, Deserialize)]
pub struct PuzzleItem {
    #[serde(rename = "puzzleid")]
    pub puzzle_id: i64,
    pub name: String,
}

/// One sub-pack entry of a container listing.
#[derive(Debug, Clone, Deserialize)]
pub struct PackRef {
    pub name: String,
    #[serde(rename = "fullslug")]
    pub full_slug: String,
    /// The site's own puzzle count for this sub-pack; may be stale or missing.
    #[serde(rename = "numpuzzles")]
    pub num_puzzles: Option<u64>,
}

/// What one listing hierarchy yielded, split by entry kind.
#[derive(Debug, Default)]
pub struct Listing {
    pub puzzles: Vec<PuzzleItem>,
    pub packs: Vec<PackRef>,
}

/// Listing entries come in two shapes: puzzles (`puzzle_id`) and sub-packs
/// (`packid`/`full_slug`).
#[derive(Debug, Deserialize)]
struct ListItem {
    #[serde(default, rename = "puzzleid")]
    puzzle_id: Option<i64>,
    #[serde(default)]
    name: String,
    #[serde(default, rename = "fullslug")]
    full_slug: Option<String>,
    #[serde(default)]
    path: String,
    #[serde(default, rename = "numpuzzles")]
    num_puzzles: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct ListData {
    pack: Option<Pack>,
    #[serde(default)]
    items: Vec<ListItem>,
}

/// Full puzzle as returned by `/puzzles/{id}`, including the board `design`.
#[derive(Debug, Deserialize)]
pub struct Puzzle {
    #[serde(rename = "puzzleid")]
    pub puzzle_id: i64,
    pub name: String,
    pub path: String,
    #[serde(rename = "puzzletype")]
    pub puzzle_type: String,
    pub design: String,
    /// ISO 8601 UTC timestamp.
    #[serde(default, rename = "publisheddate")]
    pub published_date: String,
    #[serde(default, rename = "guestlocked")]
    pub guest_locked: bool,
    #[serde(default)]
    pub author: Option<Author>,
}

impl Puzzle {
    /// Display name of the level author, falling back to the pack name.
    pub fn author_name<'a>(&'a self, pack: &'a Pack) -> &'a str {
        self.author
            .as_ref()
            .and_then(|author| author.display_name.as_deref())
            .filter(|name| !name.trim().is_empty())
            .unwrap_or(&pack.name)
    }

    /// The date part of `published_date`, e.g. `2023-08-18`.
    pub fn publish_date(&self) -> Option<&str> {
        let date = self.published_date.split('T').next()?;
        (!date.is_empty()).then_some(date)
    }
}

/// Split raw listing entries by kind: items with a `puzzle_id` are puzzles,
/// the rest are sub-packs (a sub-pack must carry a path).
fn split_items(items: Vec<ListItem>) -> Listing {
    let mut listing = Listing::default();
    for item in items {
        match item.puzzle_id {
            Some(puzzle_id) => listing.puzzles.push(PuzzleItem {
                puzzle_id,
                name: item.name,
            }),
            None => {
                let full_slug = item
                    .full_slug
                    .unwrap_or_else(|| path_to_fullslug(&item.path));
                if !full_slug.is_empty() {
                    listing.packs.push(PackRef {
                        name: item.name,
                        full_slug,
                        num_puzzles: item.num_puzzles,
                    });
                }
            }
        }
    }
    listing
}

/// `community/<path>` -> `<path>`. Everything this tool fetches lives under
/// `community/`, so the prefix is redundant in user-facing output.
pub fn display_path(full_slug: &str) -> &str {
    full_slug.strip_prefix("community/").unwrap_or(full_slug)
}

/// Sequential, rate-limited API client shared across the fetch phases.
pub struct Client {
    base: String,
    agent: Agent,
    delay: Duration,
    retry_backoff: Duration,
    stop: Arc<AtomicBool>,
}

impl Client {
    pub fn new(delay: Duration, timeout: Duration, stop: Arc<AtomicBool>) -> Self {
        Self::with_base(BASE, delay, timeout, Duration::from_secs(2), stop)
    }

    /// Alternate base URL and retry backoff: tests point `base` at a local
    /// mock server and collapse the backoff to zero.
    pub(crate) fn with_base(
        base: &str,
        delay: Duration,
        timeout: Duration,
        retry_backoff: Duration,
        stop: Arc<AtomicBool>,
    ) -> Self {
        Self {
            base: base.to_owned(),
            agent: Agent::config_builder()
                .timeout_connect(Some(Duration::from_secs(15)))
                .timeout_global(Some(timeout))
                .user_agent(USER_AGENT)
                // 4xx/5xx must come back as Ok so the Retry-After header and
                // the error body remain accessible.
                .http_status_as_error(false)
                .build()
                .new_agent(),
            delay,
            retry_backoff,
            stop,
        }
    }

    /// A client aimed at a local mock server: the rate-limit delay stays
    /// small but real, retries happen instantly.
    #[cfg(test)]
    pub(crate) fn test_client(base: &str, stop: Arc<AtomicBool>) -> Self {
        Self::with_base(
            base,
            Duration::from_millis(30),
            Duration::from_secs(10),
            Duration::ZERO,
            stop,
        )
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    /// Sleep in short slices so Ctrl+C takes effect promptly.
    fn sleep(&self, duration: Duration) -> anyhow::Result<()> {
        let mut left = duration;
        while left > Duration::ZERO {
            if self.stopped() {
                bail!("interrupted by user");
            }
            let step = left.min(Duration::from_millis(100));
            std::thread::sleep(step);
            left -= step;
        }
        Ok(())
    }

    fn get_json<D: DeserializeOwned>(&self, path: &str) -> anyhow::Result<Envelope<D>> {
        let url = format!("{}{path}", self.base);
        for attempt in 1..=MAX_ATTEMPTS {
            if self.stopped() {
                bail!("interrupted by user");
            }
            match self.agent.get(&url).call() {
                Ok(response) => {
                    // http_status_as_error is disabled: 4xx/5xx arrive as Ok.
                    let code = response.status().as_u16();
                    if code == 429 {
                        let wait = response
                            .headers()
                            .get("Retry-After")
                            .and_then(|value| value.to_str().ok())
                            .and_then(|value| value.trim().parse::<u64>().ok())
                            .unwrap_or(30);
                        eprintln!("  rate limited, waiting {wait}s ...");
                        self.sleep(Duration::from_secs(wait))?;
                        continue;
                    }
                    if code >= 500 && attempt < MAX_ATTEMPTS {
                        self.sleep(self.retry_backoff * attempt)?;
                        continue;
                    }
                    if code == 404 {
                        return Err(NotFound(url).into());
                    }
                    if !(200..300).contains(&code) {
                        bail!(
                            "HTTP {code} for {url}: {}",
                            error_excerpt(response.into_body())
                        );
                    }
                    let envelope: Envelope<D> = response
                        .into_body()
                        .read_json()
                        .with_context(|| format!("decoding response of {url}"))?;
                    self.sleep(self.delay)?;
                    if envelope.status != "success" {
                        bail!("API reported status {:?} for {url}", envelope.status);
                    }
                    return Ok(envelope);
                }
                Err(err) => {
                    if attempt < MAX_ATTEMPTS {
                        eprintln!("  request failed ({err}), retrying ...");
                        self.sleep(self.retry_backoff * attempt)?;
                        continue;
                    }
                    return Err(err).with_context(|| format!("requesting {url}"));
                }
            }
        }
        bail!("giving up on {url} after {MAX_ATTEMPTS} attempts")
    }

    /// Fetch every puzzle of one author: either the author page lists puzzles
    /// directly, or it is a container whose sub-packs are fetched in turn.
    pub fn fetch_pack(
        &self,
        username: &str,
        progress: &ProgressBar,
    ) -> anyhow::Result<(Pack, Vec<PuzzleItem>)> {
        match self.fetch_author(username, progress) {
            Err(err) if err.downcast_ref::<NotFound>().is_some() => bail!(
                "author/pack not found: {username} (https://www.sokobanonline.com/play/community/{username})"
            ),
            other => other,
        }
    }

    /// List one collection: the pack plus its direct puzzles or
    /// sub-packs, without fetching puzzle details.
    pub fn list_pack(
        &self,
        username: &str,
        progress: &ProgressBar,
    ) -> anyhow::Result<(Pack, Listing)> {
        let fullslug = format!("community/{username}");
        match self.fetch_listing(&fullslug, username, progress) {
            Err(err) if err.downcast_ref::<NotFound>().is_some() => bail!(
                "author/pack not found: {username} (https://www.sokobanonline.com/play/community/{username})"
            ),
            other => other,
        }
    }

    fn fetch_author(
        &self,
        username: &str,
        progress: &ProgressBar,
    ) -> anyhow::Result<(Pack, Vec<PuzzleItem>)> {
        let (pack, listing) =
            self.fetch_listing(&format!("community/{username}"), username, progress)?;
        if listing.packs.is_empty() {
            return Ok((pack, listing.puzzles));
        }
        // Container page: collect the puzzles of every sub-pack (one level deep).
        let mut puzzles = listing.puzzles;
        for sub in &listing.packs {
            let label = format!("{} ({})", display_path(&sub.full_slug), sub.name);
            let (_, sub_listing) = self.fetch_listing(&sub.full_slug, &label, progress)?;
            if !sub_listing.packs.is_empty() {
                bail!(
                    "pack {} holds sub-packs more than one level deep, which is not supported",
                    sub.full_slug
                );
            }
            puzzles.extend(sub_listing.puzzles);
        }
        Ok((pack, puzzles))
    }

    /// Paginate `/packs/by-path/{fullslug}`; entries may be puzzles or sub-packs.
    /// `label` is only for the progress display (fullslug, optionally with a name).
    fn fetch_listing(
        &self,
        fullslug: &str,
        label: &str,
        progress: &ProgressBar,
    ) -> anyhow::Result<(Pack, Listing)> {
        let mut listing = Listing::default();
        let mut page: u64 = 1;
        loop {
            let envelope =
                self.get_json::<ListData>(&format!("/packs/by-path/{fullslug}?page={page}"))?;
            let data = envelope.data;
            let pack = match data.pack {
                Some(pack) => pack,
                None if page == 1 => return Err(NotFound(fullslug.to_owned()).into()),
                None => bail!("pack metadata missing on page {page}"),
            };
            let total = envelope.meta.and_then(|meta| meta.total).unwrap_or(0);
            let batch_len = data.items.len();
            let split = split_items(data.items);
            listing.puzzles.extend(split.puzzles);
            listing.packs.extend(split.packs);
            let seen = (listing.puzzles.len() + listing.packs.len()) as u64;
            progress.set_message(format!(
                "listing {label}: page {page}, {}/{} entries",
                seen,
                total.max(seen),
            ));
            if seen >= total || batch_len == 0 {
                return Ok((pack, listing));
            }
            page += 1;
        }
    }

    /// Fetch one puzzle with its board `design`.
    pub fn fetch_puzzle(&self, puzzleid: i64) -> anyhow::Result<Puzzle> {
        let envelope = self.get_json::<Puzzle>(&format!("/puzzles/{puzzleid}"))?;
        Ok(envelope.data)
    }
}

/// `/play/community/<slug>` -> `community/<slug>`.
fn path_to_fullslug(path: &str) -> String {
    path.strip_prefix("/play/")
        .unwrap_or(path)
        .trim_matches('/')
        .to_owned()
}

/// Pull a human-readable message out of an API error envelope, falling back to raw text.
fn error_excerpt(mut body: ureq::Body) -> String {
    let text = body.read_to_string().unwrap_or_default();
    let message = serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|value| {
            value
                .pointer("/data/error/message")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        });
    let joined = message.unwrap_or(text);
    let line = joined.lines().next().unwrap_or_default();
    if line.chars().count() <= 200 {
        line.to_owned()
    } else {
        line.chars().take(200).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Puzzle detail with the site's wire field names, plus extra fields the
    /// real payloads carry (they must be ignored).
    const DETAIL_JSON: &str = r#"{
        "puzzleid": 1,
        "name": "corner01",
        "path": "/play/community/t1024/1_corner01",
        "puzzletype": "classic",
        "width": 8,
        "height": 6,
        "design": "  #####",
        "publisheddate": "2023-08-18T12:34:56Z",
        "guestlocked": false,
        "author": { "userid": 1, "displayname": "T1024", "slug": "t1024" },
        "numsolve": 7,
        "position": 1
    }"#;

    /// Listing envelope mixing a puzzle entry and a sub-pack entry, again with
    /// extra wire fields.
    const LISTING_JSON: &str = r#"{
        "status": "success",
        "meta": { "timestamp": "2026-10-23T18:53:26.955Z", "page": 1, "perpage": 30, "total": 2 },
        "data": {
            "kind": "puzzles",
            "pack": {
                "packid": 1,
                "name": "T1024",
                "fullslug": "community/t1024",
                "path": "/play/community/t1024",
                "numpuzzles": 2
            },
            "items": [
                { "puzzleid": 11, "name": "p1", "position": 1 },
                {
                    "packid": 2,
                    "name": "Corner Set",
                    "fullslug": "community/t1024/corner-set",
                    "path": "/play/community/t1024/corner-set",
                    "numpuzzles": 15
                }
            ]
        }
    }"#;

    #[test]
    fn puzzle_deserializes_from_wire_json() {
        let puzzle: Puzzle = serde_json::from_str(DETAIL_JSON).unwrap();
        assert_eq!(puzzle.puzzle_id, 1);
        assert_eq!(puzzle.puzzle_type, "classic");
        assert_eq!(puzzle.publish_date(), Some("2023-08-18"));
        assert!(!puzzle.guest_locked);
        assert_eq!(
            puzzle.author.as_ref().unwrap().display_name.as_deref(),
            Some("T1024")
        );
    }

    #[test]
    fn puzzle_defaults_tolerate_missing_fields() {
        let puzzle: Puzzle = serde_json::from_str(
            r#"{ "puzzleid": 2, "name": "x", "path": "/p", "puzzletype": "modern", "design": "" }"#,
        )
        .unwrap();
        assert!(puzzle.publish_date().is_none());
        assert!(!puzzle.guest_locked);
        assert!(puzzle.author.is_none());
    }

    #[test]
    fn listing_deserializes_from_wire_json() {
        let envelope: Envelope<ListData> = serde_json::from_str(LISTING_JSON).unwrap();
        assert_eq!(envelope.status, "success");
        assert_eq!(envelope.meta.unwrap().total, Some(2));
        assert_eq!(envelope.data.pack.unwrap().full_slug, "community/t1024");

        let listing = split_items(envelope.data.items);
        assert_eq!(listing.puzzles.len(), 1);
        assert_eq!(listing.puzzles[0].puzzle_id, 11);

        assert_eq!(listing.packs.len(), 1);
        let sub = &listing.packs[0];
        assert_eq!(sub.num_puzzles, Some(15));
        assert_eq!(display_path(&sub.full_slug), "t1024/corner-set");
    }

    // ---- client behaviour against a local mock server ----

    use crate::test_support::{
        Action, MockServer, Response, wire_detail, wire_listing, wire_pack, wire_pack_item,
        wire_puzzle_item,
    };

    /// A mock server + a client aimed at it (short rate-limit delay, no
    /// retry backoff).
    fn server_with(
        handler: impl Fn(usize, &str) -> Action + Send + Sync + 'static,
    ) -> (Arc<MockServer>, Client) {
        let server = MockServer::start();
        server.set_handler(handler);
        let client = Client::test_client(&server.base, Arc::new(AtomicBool::new(false)));
        (server, client)
    }

    #[test]
    fn fetch_puzzle_parses_the_detail_envelope() {
        let (server, client) = server_with(|_call, target| match target {
            "/puzzles/11" => Action::Respond(Response::json(
                200,
                wire_detail(11, "classic", "#####\n#@$.#\n#####", false),
            )),
            _ => Action::Respond(Response::plain(500, "unexpected target")),
        });
        let puzzle = client.fetch_puzzle(11).unwrap();
        assert_eq!(puzzle.name, "p11");
        assert_eq!(puzzle.publish_date(), Some("2023-08-18"));
        assert_eq!(puzzle.design, "#####\n#@$.#\n#####");
        assert_eq!(server.calls(), ["/puzzles/11"]);
    }

    #[test]
    fn rate_limited_requests_wait_and_retry() {
        let (server, client) = server_with(|call, _target| {
            if call == 0 {
                Action::Respond(Response::json(429, "{}").with_header("Retry-After", "0"))
            } else {
                Action::Respond(Response::json(200, wire_detail(11, "classic", "#", false)))
            }
        });
        assert!(client.fetch_puzzle(11).is_ok());
        assert_eq!(server.calls().len(), 2);
    }

    #[test]
    fn rate_limit_without_retry_after_can_be_interrupted() {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let server = MockServer::start();
        server.set_handler(move |_call, _target| {
            flag.store(true, Ordering::Relaxed);
            Action::Respond(Response::json(429, "{}"))
        });
        let client = Client::test_client(&server.base, stop);
        // the 30s default wait notices the stop flag immediately
        let err = client.fetch_puzzle(11).unwrap_err();
        assert!(err.to_string().contains("interrupted"), "{err:#}");
    }

    #[test]
    fn server_errors_are_retried_then_reported_with_the_api_message() {
        let (server, client) = server_with(|_call, _target| {
            Action::Respond(Response::json(
                503,
                r#"{"data":{"error":{"message":"upstream exploded"}}}"#,
            ))
        });
        let message = client.fetch_puzzle(11).unwrap_err().to_string();
        assert!(message.contains("HTTP 503"), "{message}");
        assert!(message.contains("upstream exploded"), "{message}");
        assert_eq!(server.calls().len(), 4, "every attempt is retried");
    }

    #[test]
    fn plain_text_errors_keep_one_truncated_line() {
        let (_server, client) = server_with(|_call, _target| {
            Action::Respond(Response::plain(
                403,
                format!("{}\nsecond line", "a".repeat(250)),
            ))
        });
        let message = client.fetch_puzzle(11).unwrap_err().to_string();
        assert!(message.contains("HTTP 403"), "{message}");
        assert!(message.contains(&"a".repeat(200)));
        assert!(!message.contains(&"a".repeat(201)));
        assert!(!message.contains("second line"));
    }

    #[test]
    fn non_utf8_error_bodies_degrade_to_an_empty_excerpt() {
        let (_server, client) =
            server_with(|_call, _target| Action::Respond(Response::plain(400, vec![0xff, 0xfe])));
        let message = client.fetch_puzzle(11).unwrap_err().to_string();
        assert!(message.contains("HTTP 400"), "{message}");
    }

    #[test]
    fn connection_failures_are_retried() {
        let (server, client) = server_with(|call, _target| {
            if call == 0 {
                Action::Close
            } else {
                Action::Respond(Response::json(200, wire_detail(11, "classic", "#", false)))
            }
        });
        assert!(client.fetch_puzzle(11).is_ok());
        assert_eq!(server.calls().len(), 2);
    }

    #[test]
    fn transport_errors_name_the_url_after_max_attempts() {
        let (server, client) = server_with(|_call, _target| Action::Close);
        let err = client.fetch_puzzle(11).unwrap_err();
        assert!(format!("{err:#}").contains("requesting"), "{err:#}");
        assert_eq!(server.calls().len(), 4);
    }

    #[test]
    fn rate_limited_requests_give_up_after_max_attempts() {
        let (server, client) = server_with(|_call, _target| {
            Action::Respond(Response::json(429, "{}").with_header("Retry-After", "0"))
        });
        let err = client.fetch_puzzle(11).unwrap_err();
        assert!(err.to_string().contains("giving up"), "{err:#}");
        assert_eq!(server.calls().len(), 4);
    }

    #[test]
    fn error_envelopes_are_rejected_even_with_http_200() {
        let (_server, client) = server_with(|_call, _target| {
            let envelope = serde_json::json!({
                "status": "error",
                "meta": { "total": 0 },
                "data": {
                    "puzzleid": 11,
                    "name": "p11",
                    "path": "/p",
                    "puzzletype": "classic",
                    "design": "#"
                }
            });
            Action::Respond(Response::json(200, envelope.to_string()))
        });
        let err = client.fetch_puzzle(11).unwrap_err();
        assert!(
            err.to_string().contains(r#"API reported status "error""#),
            "{err:#}"
        );
    }

    #[test]
    fn non_json_success_bodies_fail_with_a_decode_context() {
        let (_server, client) = server_with(|_call, _target| {
            Action::Respond(Response::json(200, "<html>not json</html>"))
        });
        let err = client.fetch_puzzle(11).unwrap_err();
        assert!(format!("{err:#}").contains("decoding response"), "{err:#}");
    }

    #[test]
    fn unknown_collections_report_a_friendly_not_found() {
        let (server, client) =
            server_with(|_call, _target| Action::Respond(Response::json(404, "{}")));
        let err = client
            .fetch_pack("ghost", &ProgressBar::hidden())
            .unwrap_err();
        assert!(
            err.to_string().contains("author/pack not found: ghost"),
            "{err:#}"
        );
        let err = client
            .list_pack("ghost", &ProgressBar::hidden())
            .unwrap_err();
        assert!(
            err.to_string().contains("author/pack not found: ghost"),
            "{err:#}"
        );
        assert_eq!(server.calls().len(), 2);
    }

    #[test]
    fn not_found_displays_as_not_found() {
        assert_eq!(
            NotFound("https://example.com/x".to_owned()).to_string(),
            "not found: https://example.com/x"
        );
    }

    #[test]
    fn listings_paginate_until_the_reported_total() {
        let (server, client) = server_with(|call, _target| {
            let items = if call == 0 {
                vec![wire_puzzle_item(11)]
            } else {
                vec![wire_puzzle_item(12), wire_puzzle_item(13)]
            };
            Action::Respond(Response::json(
                200,
                wire_listing(Some(wire_pack("t1024")), items, 3),
            ))
        });
        let (pack, puzzles) = client.fetch_pack("t1024", &ProgressBar::hidden()).unwrap();
        assert_eq!(pack.name, "t1024");
        let ids: Vec<i64> = puzzles.into_iter().map(|p| p.puzzle_id).collect();
        assert_eq!(ids, [11, 12, 13]);
        let calls = server.calls();
        assert!(calls[0].ends_with("page=1"), "{calls:?}");
        assert!(calls[1].ends_with("page=2"), "{calls:?}");
    }

    #[test]
    fn listings_without_pack_metadata_are_not_found() {
        let (_server, client) = server_with(|_call, _target| {
            Action::Respond(Response::json(
                200,
                wire_listing(None, vec![wire_puzzle_item(11)], 1),
            ))
        });
        let err = client
            .list_pack("t1024", &ProgressBar::hidden())
            .unwrap_err();
        assert!(
            err.to_string().contains("author/pack not found: t1024"),
            "{err:#}"
        );
    }

    #[test]
    fn missing_pack_metadata_on_later_pages_fails() {
        let (server, client) = server_with(|call, _target| {
            let data = if call == 0 {
                wire_listing(Some(wire_pack("t1024")), vec![wire_puzzle_item(11)], 2)
            } else {
                wire_listing(None, vec![wire_puzzle_item(12)], 2)
            };
            Action::Respond(Response::json(200, data))
        });
        let err = client
            .fetch_pack("t1024", &ProgressBar::hidden())
            .unwrap_err();
        assert!(
            err.to_string().contains("pack metadata missing on page 2"),
            "{err:#}"
        );
        assert_eq!(server.calls().len(), 2);
    }

    #[test]
    fn container_packs_merge_their_sub_pack_puzzles() {
        let (server, client) = server_with(|call, _target| {
            let data = if call == 0 {
                wire_listing(
                    Some(wire_pack("t1024")),
                    vec![
                        wire_puzzle_item(11),
                        wire_pack_item("community/t1024/corner-set"),
                    ],
                    2,
                )
            } else {
                wire_listing(
                    Some(wire_pack("corner-set")),
                    vec![wire_puzzle_item(21), wire_puzzle_item(22)],
                    2,
                )
            };
            Action::Respond(Response::json(200, data))
        });
        let (_pack, puzzles) = client.fetch_pack("t1024", &ProgressBar::hidden()).unwrap();
        let ids: Vec<i64> = puzzles.into_iter().map(|p| p.puzzle_id).collect();
        assert_eq!(ids, [11, 21, 22]);
        assert!(
            server.calls()[1].starts_with("/packs/by-path/community/t1024/corner-set"),
            "{:?}",
            server.calls()
        );
    }

    #[test]
    fn sub_packs_beyond_one_level_are_rejected() {
        let (server, client) = server_with(|call, _target| {
            let data = if call == 0 {
                wire_listing(
                    Some(wire_pack("t1024")),
                    vec![wire_pack_item("community/t1024/corner-set")],
                    1,
                )
            } else {
                wire_listing(
                    Some(wire_pack("corner-set")),
                    vec![wire_pack_item("community/t1024/corner-set/inner")],
                    1,
                )
            };
            Action::Respond(Response::json(200, data))
        });
        let err = client
            .fetch_pack("t1024", &ProgressBar::hidden())
            .unwrap_err();
        assert!(
            err.to_string().contains("more than one level deep"),
            "{err:#}"
        );
        assert_eq!(server.calls().len(), 2);
    }

    #[test]
    fn items_without_a_fullslug_fall_back_to_their_path() {
        let listing = split_items(vec![
            ListItem {
                puzzle_id: None,
                name: "corner-set".into(),
                full_slug: None,
                path: "/play/community/t1024/corner-set".into(),
                num_puzzles: Some(3),
            },
            ListItem {
                puzzle_id: None,
                name: "broken".into(),
                full_slug: None,
                path: String::new(),
                num_puzzles: None,
            },
        ]);
        assert_eq!(listing.packs.len(), 1);
        assert_eq!(listing.packs[0].full_slug, "community/t1024/corner-set");
        assert_eq!(listing.packs[0].num_puzzles, Some(3));
    }
}
