//! Fetch levels of an author collection from sokobanonline.com and write them
//! as an XSB level collection (.txt).
//!
//! The collection is addressed by the path of its page relative to
//! `sokobanonline.com/play/community/`: `<username>` or `<username>/<pack>`.
//! Levels that an XSB loader cannot represent (mechanisms, co-op, decoy
//! boxes) are skipped; docs/API.md documents the API and the board encoding.

mod api;
mod convert;
#[cfg(test)]
mod test_support;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, ensure};
use clap::Parser;
use indicatif::{ProgressBar, ProgressStyle};

use crate::api::{Client, Pack, Puzzle};

/// Fetch levels of an author collection from sokobanonline.com and write an
/// XSB level collection (.txt).
#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    /// Collection path: `<username>[/<pack>]` — the path segments of the
    /// sokobanonline.com/play/community/... URL. `<username>` alone fetches
    /// the whole author (sub-packs merged).
    #[arg(value_name = "COLLECTION")]
    collection: String,

    /// Output file [default: <COLLECTION> with '/' replaced by '-'>.txt]
    #[arg(short, long, value_name = "FILE")]
    output: Option<PathBuf>,

    /// List the sub-packs of the collection instead of fetching
    #[arg(long)]
    list: bool,

    /// Only fetch the first N puzzles of the collection
    #[arg(long, value_name = "N")]
    limit: Option<usize>,

    /// Seconds to sleep between requests (robots.txt asks for 5)
    #[arg(long, value_name = "SECONDS", default_value_t = 1.0)]
    delay: f64,

    /// Seconds until a single request times out
    #[arg(long, value_name = "SECONDS", default_value_t = 30.0)]
    timeout: f64,

    /// Print a line for every skipped level (default: a summary breakdown only)
    #[arg(short, long)]
    verbose: bool,
}

fn main() -> ExitCode {
    match try_main() {
        Ok(code) => code,
        Err(err) => {
            let mut chain = err.chain();
            if let Some(top) = chain.next() {
                anstream::eprintln!("{} {top}", paint(anstyle::AnsiColor::Red, "error:"));
            }
            for cause in chain {
                anstream::eprintln!("  caused by: {cause}");
            }
            ExitCode::FAILURE
        }
    }
}

/// Everything `main` does that can fail: parse the args, install the Ctrl+C
/// handler, build the client, then run.
fn try_main() -> anyhow::Result<ExitCode> {
    let cli = Cli::parse();
    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = stop.clone();
        ctrlc::set_handler(move || stop.store(true, Ordering::SeqCst))
            .context("installing the Ctrl+C handler")?;
    }
    let client = Client::new(
        Duration::from_secs_f64(cli.delay),
        Duration::from_secs_f64(cli.timeout),
        stop.clone(),
    );
    run(cli, &client, &stop)
}

/// Color `text` for the terminal; anstream strips the codes when not a TTY.
fn paint(color: anstyle::AnsiColor, text: impl std::fmt::Display) -> String {
    let style = anstyle::Style::new().fg_color(Some(anstyle::Color::Ansi(color)));
    format!("{}{text}{}", style.render(), anstyle::Reset.render())
}

/// Print a line above the progress bar, or plainly when stderr is not a TTY
/// (indicatif drops `println` on hidden draw targets).
fn note(progress: &ProgressBar, line: &str) {
    if console::Term::stderr().is_term() {
        progress.println(line);
    } else {
        eprintln!("{line}");
    }
}

/// Skip counts by reason category, rendered as a compact summary suffix.
#[derive(Default)]
struct SkipBreakdown {
    modern: usize,
    guest_locked: usize,
    rejected: usize,
}

impl SkipBreakdown {
    fn breakdown(&self) -> String {
        let mut parts = Vec::new();
        if self.modern > 0 {
            parts.push(format!("{} modern", self.modern));
        }
        if self.guest_locked > 0 {
            parts.push(format!("{} guest-locked", self.guest_locked));
        }
        if self.rejected > 0 {
            parts.push(format!("{} rejected by the XSB parser", self.rejected));
        }
        if parts.is_empty() {
            String::new()
        } else {
            format!(" ({})", parts.join(", "))
        }
    }
}

/// Validate the collection path: `<username>[/<pack>...]`, with the allowed
/// characters (`[a-zA-Z0-9_-]`) per path segment.
fn parse_collection(input: &str) -> anyhow::Result<String> {
    let collection = input.trim();
    ensure!(!collection.is_empty(), "empty collection path");
    ensure!(
        collection.split('/').all(|segment| {
            !segment.is_empty()
                && segment
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        }),
        "invalid collection path {collection:?} (expected <username>[/<pack>...], \
         allowed characters [a-zA-Z0-9_-])"
    );
    Ok(collection.to_owned())
}

/// Default output name: path segments joined with `-`, so collections of
/// different authors cannot silently overwrite each other.
fn default_output(collection: &str) -> PathBuf {
    PathBuf::from(format!("{}.txt", collection.replace('/', "-")))
}

fn run(cli: Cli, client: &Client, stop: &Arc<AtomicBool>) -> anyhow::Result<ExitCode> {
    ensure!(cli.delay >= 0.0, "--delay must not be negative");
    ensure!(cli.timeout > 0.0, "--timeout must be positive");

    let collection = parse_collection(&cli.collection)?;

    if cli.list {
        list_collection(client, &collection)?;
        return Ok(ExitCode::SUCCESS);
    }
    let out_path = cli
        .output
        .clone()
        .unwrap_or_else(|| default_output(&collection));
    let interrupted = fetch_and_write(client, &collection, &out_path, &cli, stop)?;
    Ok(if interrupted {
        ExitCode::from(130)
    } else {
        ExitCode::SUCCESS
    })
}

/// List the sub-packs (or the direct puzzle count) of one collection.
fn list_collection(client: &Client, collection: &str) -> anyhow::Result<()> {
    let spinner = ProgressBar::new_spinner();
    spinner.enable_steady_tick(Duration::from_millis(100));
    let (pack, listing) = client.list_pack(collection, &spinner)?;
    spinner.finish_and_clear();

    if listing.packs.is_empty() {
        anstream::println!(
            "{}: {} puzzles directly on the page",
            pack.name,
            listing.puzzles.len()
        );
        return Ok(());
    }
    anstream::println!("{} - {} sub-packs:", pack.name, listing.packs.len());
    for sub in &listing.packs {
        let count = sub.num_puzzles.map_or("?".to_owned(), |n| n.to_string());
        anstream::println!(
            "  {} - {} ({} puzzles)",
            api::display_path(&sub.full_slug),
            sub.name,
            count
        );
    }
    Ok(())
}

/// Fetch one collection and write its XSB collection file.
/// Returns `true` when the user interrupted the fetch.
fn fetch_and_write(
    client: &Client,
    collection: &str,
    out_path: &Path,
    cli: &Cli,
    stop: &Arc<AtomicBool>,
) -> anyhow::Result<bool> {
    // Phase 1: paginate the pack listing to learn the puzzle ids.
    let spinner = ProgressBar::new_spinner();
    spinner.enable_steady_tick(Duration::from_millis(100));
    spinner.set_style(
        ProgressStyle::with_template("{spinner:.green} {msg}").expect("template is valid"),
    );
    spinner.set_message(format!("listing {collection} ..."));
    let (pack, items) = match client.fetch_pack(collection, &spinner) {
        Ok(result) => result,
        Err(err) => {
            if stop.load(Ordering::Relaxed) {
                anstream::eprintln!("interrupted");
                return Ok(true);
            }
            return Err(err);
        }
    };
    spinner.finish_and_clear();

    let items: Vec<_> = match cli.limit {
        Some(limit) => items.into_iter().take(limit).collect(),
        None => items,
    };
    ensure!(
        !items.is_empty(),
        "pack {} holds no puzzles",
        api::display_path(&pack.full_slug)
    );

    // Phase 2: fetch every puzzle and convert/filter it for the XSB format.
    let progress = ProgressBar::new(items.len() as u64);
    progress.enable_steady_tick(Duration::from_millis(100));
    progress.set_style(
        ProgressStyle::with_template(
            "{spinner:.green} [{elapsed_precise}] {bar:30.cyan/blue} {pos}/{len} ({percent}%) {msg}",
        )
        .expect("template is valid"),
    );

    let mut kept: Vec<(Puzzle, String)> = Vec::new();
    let mut fetched = 0usize; // successful detail fetches; the count the header reports
    let mut skipped = 0usize;
    let mut skipped_by = SkipBreakdown::default();
    let mut failed = 0usize;
    let mut interrupted = false;

    for item in &items {
        if stop.load(Ordering::Relaxed) {
            interrupted = true;
            break;
        }
        progress.set_message(format!(
            "{} ({}) - kept {} · skipped {} · failed {}",
            item.name,
            item.puzzle_id,
            kept.len(),
            skipped,
            failed,
        ));
        match client.fetch_puzzle(item.puzzle_id) {
            Err(err) if !stop.load(Ordering::Relaxed) => {
                failed += 1;
                note(
                    &progress,
                    &format!("  failed {} {}: {err:#}", item.puzzle_id, item.name),
                );
            }
            Err(_) => {
                interrupted = true;
                break;
            }
            Ok(puzzle) => {
                if puzzle.guest_locked {
                    skipped += 1;
                    skipped_by.guest_locked += 1;
                    if cli.verbose {
                        note(
                            &progress,
                            &format!(
                                "  skipped {} {}: guest-locked",
                                puzzle.puzzle_id, puzzle.name
                            ),
                        );
                    }
                } else {
                    fetched += 1;
                    if puzzle.puzzle_type != "classic" {
                        // modern designs use a 4-chars-per-cell encoding XSB
                        // cannot express; docs/API.md §4.
                        skipped += 1;
                        skipped_by.modern += 1;
                        if cli.verbose {
                            note(
                                &progress,
                                &format!(
                                    "  skipped {} {}: {} level",
                                    puzzle.puzzle_id, puzzle.name, puzzle.puzzle_type
                                ),
                            );
                        }
                    } else {
                        match convert::to_xsb(&puzzle.design) {
                            Ok(board) => kept.push((puzzle, board)),
                            Err(reason) => {
                                skipped += 1;
                                skipped_by.rejected += 1;
                                if cli.verbose {
                                    note(
                                        &progress,
                                        &format!(
                                            "  skipped {} {}: {reason}",
                                            puzzle.puzzle_id, puzzle.name
                                        ),
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
        progress.inc(1);
    }
    progress.finish_and_clear();

    if interrupted {
        anstream::eprintln!(
            "{}",
            paint(
                anstyle::AnsiColor::Yellow,
                "interrupted - writing the levels fetched so far",
            )
        );
    }

    std::fs::write(out_path, render_output(&pack, fetched, &kept))
        .with_context(|| format!("writing {}", out_path.display()))?;

    anstream::println!(
        "{}: kept {}, skipped {}{}, failed {} of {fetched} fetched -> {}",
        paint(anstyle::AnsiColor::Green, "done"),
        paint(anstyle::AnsiColor::Green, kept.len()),
        paint(anstyle::AnsiColor::Yellow, skipped),
        skipped_by.breakdown(),
        paint(anstyle::AnsiColor::Red, failed),
        out_path.display(),
    );
    Ok(interrupted)
}

/// Render the collection: each block is the XSB map followed by `Title:` /
/// `Author:` / `URL:` metadata lines, which soukoban parses as level metadata.
fn render_output(pack: &Pack, fetched: usize, kept: &[(Puzzle, String)]) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "; Sokoban Online - {} ({})", pack.name, pack.path);
    let _ = writeln!(out, "; {} XSB levels of {fetched} fetched", kept.len());
    let _ = writeln!(
        out,
        "; classic puzzles only - modern puzzles are excluded\n"
    );
    for (index, (puzzle, board)) in kept.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        out.push_str(board);
        out.push('\n');
        let _ = writeln!(out, "Title: {}", puzzle.name);
        let _ = writeln!(out, "Author: {}", puzzle.author_name(pack));
        if let Some(date) = puzzle.publish_date() {
            let _ = writeln!(out, "; Published: {date}");
        }
        let _ = writeln!(out, "; URL: https://www.sokobanonline.com{}", puzzle.path);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (Pack, Puzzle) {
        (
            Pack {
                name: "t1024".into(),
                full_slug: "community/t1024".into(),
                path: "/play/community/t1024".into(),
            },
            Puzzle {
                puzzle_id: 1,
                name: "corner01".into(),
                path: "/play/community/t1024/1_corner01".into(),
                puzzle_type: "classic".into(),
                design: String::new(),
                published_date: "2023-08-18T12:34:56Z".into(),
                guest_locked: false,
                author: Some(api::Author {
                    display_name: Some("T1024".into()),
                }),
            },
        )
    }

    #[test]
    fn output_blocks_carry_metadata() {
        let (pack, puzzle) = fixture();
        let out = render_output(&pack, 1, &[(puzzle, "####".into())]);
        // map first, metadata after
        assert!(out.contains("\n####\nTitle: corner01\n"));
        assert!(out.contains("Author: T1024\n"));
        // publish date as a comment, before the URL comment
        let published = out.find("; Published: 2023-08-18\n").expect("publish date");
        let url = out.find("; URL:").expect("url comment");
        assert!(published < url);
        assert!(
            out.contains("; URL: https://www.sokobanonline.com/play/community/t1024/1_corner01\n")
        );
        // the old provenance comment is gone
        assert!(!out.contains(" | by "));
    }

    #[test]
    fn emitted_block_parses_as_soukoban_level() {
        let (pack, puzzle) = fixture();
        let out = render_output(&pack, 1, &[(puzzle, "#####\n#@$.#\n#####".into())]);
        let block = out
            .split("\n\n")
            .nth(1)
            .expect("one block after the header");
        let level: soukoban::level::Level = block.parse().expect("block parses as a Level");
        assert_eq!(
            level.metadata().get("title").map(String::as_str),
            Some("corner01")
        );
        assert_eq!(
            level.metadata().get("author").map(String::as_str),
            Some("T1024")
        );
    }

    #[test]
    fn author_falls_back_to_pack_name() {
        let (pack, mut puzzle) = fixture();
        puzzle.author = None;
        assert_eq!(puzzle.author_name(&pack), "t1024");
    }

    #[test]
    fn collection_paths_are_parsed_and_validated() {
        assert_eq!(parse_collection("a").unwrap(), "a");
        assert_eq!(parse_collection(" a/b ").unwrap(), "a/b");
        assert!(parse_collection("a//b").is_err()); // empty segment
        assert!(parse_collection("").is_err());
        assert!(parse_collection(" ").is_err());
        assert!(parse_collection("https://example.com").is_err()); // ':' invalid
        assert!(parse_collection("a b").is_err()); // space invalid
    }

    #[test]
    fn default_output_joins_path_segments() {
        assert_eq!(default_output("t1024"), PathBuf::from("t1024.txt"));
        assert_eq!(
            default_output("t1024/corner-set"),
            PathBuf::from("t1024-corner-set.txt")
        );
        assert_eq!(default_output("a/b/c"), PathBuf::from("a-b-c.txt"));
    }

    // ---- end-to-end tests against a local mock of the API ----

    use crate::test_support::{
        Action, MockServer, Response, wire_detail, wire_listing, wire_pack, wire_pack_item,
        wire_puzzle_item,
    };
    use std::sync::atomic::AtomicUsize;

    /// A unique temp directory per test, for output files.
    fn scratch_dir(tag: &str) -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "sokobanonline-test-{}-{}-{tag}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    fn cli_for(collection: &str, out: &Path) -> Cli {
        Cli {
            collection: collection.to_owned(),
            output: Some(out.to_path_buf()),
            list: false,
            limit: None,
            delay: 0.0,
            timeout: 5.0,
            verbose: false,
        }
    }

    /// A mock server + a client pointed at it + a fresh stop flag.
    fn mock(
        handler: impl Fn(usize, &str) -> Action + Send + Sync + 'static,
    ) -> (Arc<MockServer>, Client, Arc<AtomicBool>) {
        let server = MockServer::start();
        server.set_handler(handler);
        let stop = Arc::new(AtomicBool::new(false));
        let client = Client::test_client(&server.base, Arc::clone(&stop));
        (server, client, stop)
    }

    /// The listing response for a pack holding exactly `ids` as puzzles.
    fn listing_of(ids: &[i64]) -> Action {
        Action::Respond(Response::json(
            200,
            wire_listing(
                Some(wire_pack("t1024")),
                ids.iter().map(|id| wire_puzzle_item(*id)).collect(),
                ids.len() as u64,
            ),
        ))
    }

    fn detail_action(id: i64) -> Action {
        Action::Respond(Response::json(
            200,
            wire_detail(id, "classic", "#####\n#@$.#\n#####", false),
        ))
    }

    /// Route `/puzzles/<id>` to the detail of that id, mocking the listing
    /// endpoint to hold exactly `ids`.
    fn fetch_handler(
        ids: &'static [i64],
    ) -> impl Fn(usize, &str) -> Action + Send + Sync + 'static {
        move |_call, target| match target {
            t if t.starts_with("/packs/by-path/") => listing_of(ids),
            t if t.starts_with("/puzzles/") => {
                let id: i64 = t
                    .rsplit('/')
                    .next()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                detail_action(id)
            }
            _ => Action::Respond(Response::plain(500, "unexpected target")),
        }
    }

    #[test]
    fn run_fetches_details_and_writes_the_collection() {
        let dir = scratch_dir("fetch");
        let out = dir.join("out.txt");
        let (server, client, stop) = mock(fetch_handler(&[11, 12]));
        let code = run(cli_for("t1024", &out), &client, &stop).unwrap();
        assert_eq!(code, ExitCode::SUCCESS);
        assert_eq!(
            server.calls(),
            [
                "/packs/by-path/community/t1024?page=1",
                "/puzzles/11",
                "/puzzles/12"
            ]
        );
        let text = std::fs::read_to_string(&out).expect("output file");
        assert!(text.contains("; Sokoban Online - t1024 (/play/community/t1024)"));
        assert!(text.contains("; 2 XSB levels of 2 fetched"));
        assert!(text.contains("Title: p11\n"));
        assert!(text.contains("Title: p12\n"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_defaults_the_output_path_from_the_collection() {
        let (_server, client, stop) = mock(fetch_handler(&[11]));
        let cli = Cli {
            output: None,
            ..cli_for("a/b", Path::new("unused.txt"))
        };
        assert_eq!(run(cli, &client, &stop).unwrap(), ExitCode::SUCCESS);
        let text = std::fs::read_to_string("a-b.txt").expect("default output file");
        assert!(text.contains("Title: p11\n"));
        let _ = std::fs::remove_file("a-b.txt");
    }

    /// One valid, one modern, one guest-locked and one unparseable puzzle.
    fn skip_fixture_handler() -> impl Fn(usize, &str) -> Action + Send + Sync + 'static {
        let details = [
            (1, "classic", "#####\n#@$.#\n#####", false),
            (2, "modern", "##", false),
            (3, "classic", "#####\n#@$.#\n#####", true),
            (4, "classic", "#####\n#$$.#\n# @ #\n#####", false),
        ];
        move |_call, target| match target {
            t if t.starts_with("/packs/by-path/") => listing_of(&[1, 2, 3, 4]),
            t if t.starts_with("/puzzles/") => {
                let id: i64 = t
                    .rsplit('/')
                    .next()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                match details.iter().find(|(key, ..)| *key == id) {
                    Some((id, kind, design, locked)) => Action::Respond(Response::json(
                        200,
                        wire_detail(*id, kind, design, *locked),
                    )),
                    None => Action::Respond(Response::plain(500, "unexpected id")),
                }
            }
            _ => Action::Respond(Response::plain(500, "unexpected target")),
        }
    }

    #[test]
    fn run_skips_modern_guest_locked_and_unparseable_boards() {
        let dir = scratch_dir("skips");
        let out = dir.join("out.txt");
        let (_server, client, stop) = mock(skip_fixture_handler());
        let code = run(cli_for("t1024", &out), &client, &stop).unwrap();
        assert_eq!(code, ExitCode::SUCCESS);
        let text = std::fs::read_to_string(&out).expect("output file");
        assert!(text.contains("; 1 XSB levels of 3 fetched"));
        assert!(text.contains("Title: p1\n"));
        assert!(!text.contains("Title: p2\n"));
        assert!(!text.contains("Title: p3\n"));
        assert!(!text.contains("Title: p4\n"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_verbose_mode_reports_each_skip() {
        let dir = scratch_dir("verbose");
        let out = dir.join("out.txt");
        let (_server, client, stop) = mock(skip_fixture_handler());
        let cli = Cli {
            verbose: true,
            ..cli_for("t1024", &out)
        };
        assert_eq!(run(cli, &client, &stop).unwrap(), ExitCode::SUCCESS);
        let text = std::fs::read_to_string(&out).expect("output file");
        assert!(text.contains("; 1 XSB levels of 3 fetched"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_reports_failed_detail_fetches_and_still_writes_the_file() {
        let dir = scratch_dir("failed");
        let out = dir.join("out.txt");
        let (server, client, stop) = mock(|_call, target| match target {
            t if t.starts_with("/packs/by-path/") => listing_of(&[1]),
            _ => Action::Respond(Response::json(
                503,
                r#"{"data":{"error":{"message":"upstream exploded"}}}"#,
            )),
        });
        let code = run(cli_for("t1024", &out), &client, &stop).unwrap();
        assert_eq!(code, ExitCode::SUCCESS);
        let detail_calls = server
            .calls()
            .iter()
            .filter(|t| t.starts_with("/puzzles/"))
            .count();
        assert_eq!(detail_calls, 4, "503 is retried across all attempts");
        let text = std::fs::read_to_string(&out).expect("output file");
        assert!(text.contains("; 0 XSB levels of 0 fetched"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_limit_only_fetches_the_first_n_puzzles() {
        let dir = scratch_dir("limit");
        let out = dir.join("out.txt");
        let (server, client, stop) = mock(fetch_handler(&[11, 12]));
        let cli = Cli {
            limit: Some(1),
            ..cli_for("t1024", &out)
        };
        assert_eq!(run(cli, &client, &stop).unwrap(), ExitCode::SUCCESS);
        assert_eq!(
            server.calls(),
            ["/packs/by-path/community/t1024?page=1", "/puzzles/11"]
        );
        let text = std::fs::read_to_string(&out).expect("output file");
        assert!(text.contains("Title: p11\n"));
        assert!(!text.contains("Title: p12"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_interrupted_while_listing_exits_130() {
        let (server, client, stop) = mock(|_call, _target| listing_of(&[1]));
        stop.store(true, Ordering::Relaxed);
        let code = run(cli_for("t1024", Path::new("unused.txt")), &client, &stop).unwrap();
        assert_eq!(code, ExitCode::from(130));
        assert!(server.calls().is_empty(), "no request must be made");
    }

    #[test]
    fn run_interrupted_between_details_writes_the_partial_collection() {
        let dir = scratch_dir("interrupted");
        let out = dir.join("out.txt");
        let server = MockServer::start();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        server.set_handler(move |_call, target| match target {
            t if t.starts_with("/packs/by-path/") => listing_of(&[1, 2]),
            _ => {
                // the first detail fetch succeeds but pulls the brake
                flag.store(true, Ordering::Relaxed);
                detail_action(1)
            }
        });
        // delay zero: the flag set while serving item 1 must not trip the
        // post-response sleep, so the loop-top check for item 2 is what stops
        let client = Client::with_base(
            &server.base,
            Duration::ZERO,
            Duration::from_secs(10),
            Duration::ZERO,
            Arc::clone(&stop),
        );
        let code = run(cli_for("t1024", &out), &client, &stop).unwrap();
        assert_eq!(code, ExitCode::from(130));
        let detail_calls = server
            .calls()
            .iter()
            .filter(|t| t.starts_with("/puzzles/"))
            .count();
        assert_eq!(detail_calls, 1, "the stop flag halts after item 1");
        let text = std::fs::read_to_string(&out).expect("output file");
        assert!(text.contains("; 1 XSB levels of 1 fetched"));
        assert!(text.contains("Title: p1\n"));
        assert!(!text.contains("Title: p2"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_interrupted_by_a_failing_detail_fetch_writes_nothing_kept() {
        let dir = scratch_dir("interrupted-failed");
        let out = dir.join("out.txt");
        let server = MockServer::start();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        server.set_handler(move |_call, target| match target {
            t if t.starts_with("/packs/by-path/") => listing_of(&[1, 2]),
            _ => {
                // fail the first detail fetch and pull the brake with it
                flag.store(true, Ordering::Relaxed);
                Action::Respond(Response::plain(500, "boom"))
            }
        });
        let client = Client::test_client(&server.base, Arc::clone(&stop));
        let code = run(cli_for("t1024", &out), &client, &stop).unwrap();
        assert_eq!(code, ExitCode::from(130));
        let detail_calls = server
            .calls()
            .iter()
            .filter(|t| t.starts_with("/puzzles/"))
            .count();
        assert_eq!(detail_calls, 1, "the stop flag halts the retry loop");
        let text = std::fs::read_to_string(&out).expect("output file");
        assert!(text.contains("; 0 XSB levels of 0 fetched"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_propagates_fetch_errors_when_not_interrupted() {
        let dir = scratch_dir("propagate");
        let out = dir.join("out.txt");
        let (server, client, stop) =
            mock(|_call, _target| Action::Respond(Response::json(404, "{}")));
        let err = run(cli_for("ghost", &out), &client, &stop).unwrap_err();
        assert!(err.to_string().contains("author/pack not found"), "{err:#}");
        assert_eq!(server.calls().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_list_mode_prints_sub_packs_without_fetching_details() {
        let (server, client, stop) = mock(|_call, _target| {
            Action::Respond(Response::json(
                200,
                wire_listing(
                    Some(wire_pack("t1024")),
                    vec![
                        wire_pack_item("community/t1024/corner-set"),
                        wire_pack_item("community/t1024/box-world"),
                    ],
                    2,
                ),
            ))
        });
        let cli = Cli {
            list: true,
            ..cli_for("t1024", Path::new("unused.txt"))
        };
        assert_eq!(run(cli, &client, &stop).unwrap(), ExitCode::SUCCESS);
        assert_eq!(server.calls().len(), 1, "no puzzle details are fetched");
    }

    #[test]
    fn run_list_mode_reports_direct_puzzle_counts() {
        let (server, client, stop) = mock(|_call, _target| listing_of(&[11]));
        let cli = Cli {
            list: true,
            ..cli_for("t1024", Path::new("unused.txt"))
        };
        assert_eq!(run(cli, &client, &stop).unwrap(), ExitCode::SUCCESS);
        assert_eq!(server.calls().len(), 1);
    }

    #[test]
    fn run_validates_delay_timeout_collection_and_empty_packs() {
        let dir = scratch_dir("validation");
        let out = dir.join("out.txt");
        let (server, client, stop) = mock(|_call, _target| listing_of(&[]));

        let mut cli = cli_for("t1024", &out);
        cli.delay = -1.0;
        assert!(
            run(cli, &client, &stop)
                .unwrap_err()
                .to_string()
                .contains("--delay must not be negative")
        );

        let mut cli = cli_for("t1024", &out);
        cli.timeout = 0.0;
        assert!(
            run(cli, &client, &stop)
                .unwrap_err()
                .to_string()
                .contains("--timeout must be positive")
        );

        assert!(
            run(cli_for("no such", &out), &client, &stop)
                .unwrap_err()
                .to_string()
                .contains("invalid collection path")
        );

        // an empty listing surfaces as "the pack holds no puzzles"
        assert!(
            run(cli_for("t1024", &out), &client, &stop)
                .unwrap_err()
                .to_string()
                .contains("holds no puzzles")
        );

        assert_eq!(
            server.calls().len(),
            1,
            "only the empty-listing case reaches the network"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn skip_breakdown_renders_only_nonzero_counts() {
        assert_eq!(SkipBreakdown::default().breakdown(), "");
        let mut breakdown = SkipBreakdown {
            modern: 2,
            ..SkipBreakdown::default()
        };
        assert_eq!(breakdown.breakdown(), " (2 modern)");
        breakdown.guest_locked = 3;
        breakdown.rejected = 4;
        assert_eq!(
            breakdown.breakdown(),
            " (2 modern, 3 guest-locked, 4 rejected by the XSB parser)"
        );
    }
}
