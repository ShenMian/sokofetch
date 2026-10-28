//! Inspect and fetch author collections from sokobanonline.com.
//!
//! `info` describes a collection (its sub-packs and the levels that sit
//! outside them); `fetch` writes its levels as an XSB level collection (.txt).
//! The collection is addressed by the path of its page relative to
//! `sokobanonline.com/play/community/`: `<username>` or `<username>/<pack>`.
//! Levels that an XSB loader cannot represent (mechanisms, co-op, decoy
//! boxes) are skipped; docs/API.md documents the API and the board encoding.

mod api;
mod convert;
#[cfg(test)]
mod test_support;

use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, ensure};
use clap::{Args, Parser, Subcommand};
use indicatif::{ProgressBar, ProgressStyle};

use crate::api::{Client, Pack, Puzzle};

/// Fetch levels of an author collection from sokobanonline.com and write an
/// XSB level collection (.txt).
#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Show what a collection holds: its sub-packs and the levels that sit
    /// outside them
    Info(InfoArgs),
    /// Fetch a collection's levels as an XSB level collection (.txt)
    Fetch(FetchArgs),
}

/// What every request-making subcommand shares.
#[derive(Debug, Args)]
struct Connection {
    /// Collection path: `<username>[/<pack>]` — the path segments of the
    /// sokobanonline.com/play/community/... URL. `<username>` alone means the
    /// whole author (sub-packs merged).
    #[arg(value_name = "COLLECTION")]
    collection: String,

    /// Seconds to sleep between requests (robots.txt asks for 5)
    #[arg(long, value_name = "SECONDS", default_value_t = 1.0)]
    delay: f64,

    /// Seconds until a single request times out
    #[arg(long, value_name = "SECONDS", default_value_t = 30.0)]
    timeout: f64,

    /// Also look at levels that sit directly on the collection page instead of
    /// inside one of its sub-packs. The site never links to them, so they are
    /// read from its sitemap (4 extra requests, ~16 MB).
    #[arg(long)]
    ungrouped: bool,
}

#[derive(Debug, Args)]
struct InfoArgs {
    #[command(flatten)]
    connection: Connection,
}

#[derive(Debug, Args)]
struct FetchArgs {
    #[command(flatten)]
    connection: Connection,

    /// Output file [default: <COLLECTION> with '/' replaced by '-'>.txt]
    #[arg(short, long, value_name = "FILE")]
    output: Option<PathBuf>,

    /// Only fetch the first N puzzles of the collection
    #[arg(long, value_name = "N")]
    limit: Option<usize>,

    /// Print a line for every skipped level (default: a summary breakdown only)
    #[arg(short, long)]
    verbose: bool,

    /// Fetch only the levels that sit directly on the collection page, and
    /// none of its sub-packs. Implies --ungrouped.
    #[arg(long)]
    only_ungrouped: bool,
}

impl Command {
    fn connection(&self) -> &Connection {
        match self {
            Self::Info(args) => &args.connection,
            Self::Fetch(args) => &args.connection,
        }
    }
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
    let connection = cli.command.connection();
    validate(connection)?;
    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = stop.clone();
        ctrlc::set_handler(move || stop.store(true, Ordering::SeqCst))
            .context("installing the Ctrl+C handler")?;
    }
    let client = Client::new(
        Duration::from_secs_f64(connection.delay),
        Duration::from_secs_f64(connection.timeout),
        stop.clone(),
    );
    run(cli.command, &client, &stop)
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

fn run(command: Command, client: &Client, stop: &Arc<AtomicBool>) -> anyhow::Result<ExitCode> {
    validate(command.connection())?;
    let collection = parse_collection(&command.connection().collection)?;

    match command {
        Command::Info(args) => {
            info(client, &collection, args.connection.ungrouped)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Fetch(args) => {
            let out_path = args
                .output
                .clone()
                .unwrap_or_else(|| default_output(&collection));
            let interrupted = fetch_and_write(client, &collection, &out_path, &args, stop)?;
            Ok(if interrupted {
                ExitCode::from(130)
            } else {
                ExitCode::SUCCESS
            })
        }
    }
}

/// Reject nonsensical pacing before any request (and before the client, which
/// cannot be built from a negative timeout).
fn validate(connection: &Connection) -> anyhow::Result<()> {
    ensure!(connection.delay >= 0.0, "--delay must not be negative");
    ensure!(connection.timeout > 0.0, "--timeout must be positive");
    Ok(())
}

/// Print what a collection holds: its sub-packs, plus the levels that belong
/// to none of them (docs/API.md §3.2.1).
fn info(client: &Client, collection: &str, ungrouped: bool) -> anyhow::Result<()> {
    let spinner = ProgressBar::new_spinner();
    spinner.enable_steady_tick(Duration::from_millis(100));
    let (pack, listing) = client.list_pack(collection, &spinner)?;
    spinner.finish_and_clear();

    let total = pack
        .num_puzzles
        .map_or_else(|| "?".to_owned(), |n| n.to_string());
    if listing.packs.is_empty() {
        anstream::println!(
            "{} · {} puzzles, all directly on the page",
            pack.name,
            listing.puzzles.len()
        );
        anstream::println!("  https://www.sokobanonline.com{}", pack.path);
        return Ok(());
    }
    anstream::println!(
        "{} · {} puzzles in {} sub-packs",
        pack.name,
        total,
        listing.packs.len()
    );
    anstream::println!("  https://www.sokobanonline.com{}", pack.path);

    let width = listing
        .packs
        .iter()
        .map(|sub| api::display_path(&sub.full_slug).chars().count())
        .max()
        .unwrap_or(0);
    anstream::println!();
    for sub in &listing.packs {
        let count = sub
            .num_puzzles
            .map_or_else(|| "?".to_owned(), |n| n.to_string());
        anstream::println!(
            "  {:<width$}  {:>4}  {}",
            api::display_path(&sub.full_slug),
            count,
            sub.name,
            width = width
        );
    }
    anstream::println!();

    // The page's own counter includes levels that belong to no sub-pack; the
    // listing above can never reach them (docs/API.md §3.2.1).
    let counts: Option<u64> = listing.packs.iter().map(|sub| sub.num_puzzles).sum();
    let hidden = match (pack.num_puzzles, counts) {
        (Some(total), Some(grouped)) => total.saturating_sub(grouped),
        _ => 0,
    };
    if !ungrouped {
        if hidden > 0 {
            anstream::println!(
                "  {}",
                paint(
                    anstyle::AnsiColor::Yellow,
                    format!(
                        "outside every sub-pack: {hidden} \
                         (not linked from the page; --ungrouped lists them)"
                    )
                )
            );
        }
        return Ok(());
    }
    let spinner = ProgressBar::new_spinner();
    spinner.enable_steady_tick(Duration::from_millis(100));
    let extra = client.fetch_ungrouped(collection, &spinner);
    spinner.finish_and_clear();
    let extra = extra?;
    if extra.is_empty() {
        anstream::println!("  outside every sub-pack: none");
        return Ok(());
    }
    anstream::println!(
        "  outside every sub-pack: {} (not linked from the page)",
        extra.len()
    );
    for chunk in extra.chunks(6) {
        let ids: Vec<String> = chunk
            .iter()
            .map(|item| item.puzzle_id.to_string())
            .collect();
        anstream::println!("    {}", ids.join("  "));
    }
    Ok(())
}

/// Fetch one collection and write its XSB collection file.
/// Returns `true` when the user interrupted the fetch.
fn fetch_and_write(
    client: &Client,
    collection: &str,
    out_path: &Path,
    args: &FetchArgs,
    stop: &Arc<AtomicBool>,
) -> anyhow::Result<bool> {
    // Phase 1: learn the puzzle ids. `--only-ungrouped` skips the sub-packs
    // entirely, so it needs nothing but the container page and the sitemap.
    let spinner = ProgressBar::new_spinner();
    spinner.enable_steady_tick(Duration::from_millis(100));
    spinner.set_style(
        ProgressStyle::with_template("{spinner:.green} {msg}").expect("template is valid"),
    );
    spinner.set_message(format!("listing {collection} ..."));
    let listing = if args.only_ungrouped {
        client
            .list_pack(collection, &spinner)
            .and_then(|(pack, listing)| {
                ensure!(
                    !listing.packs.is_empty(),
                    "{} has no sub-packs, so every level is already on its page - drop \
                     --only-ungrouped",
                    api::display_path(&pack.full_slug)
                );
                Ok((pack, Vec::new(), listing.packs.len(), 0, 0))
            })
    } else {
        client.fetch_pack(collection, &spinner).map(|collection| {
            (
                collection.pack,
                collection.items,
                collection.groups,
                collection.grouped,
                collection.hidden,
            )
        })
    };
    let (pack, mut items, groups, grouped, hidden) = match listing {
        Ok(listing) => listing,
        Err(err) => {
            if stop.load(Ordering::Relaxed) {
                anstream::eprintln!("interrupted");
                return Ok(true);
            }
            return Err(err);
        }
    };
    spinner.finish_and_clear();

    // Levels on the container page itself are missing from the listing: the
    // API only ever returns one of "sub-packs" / "own puzzles" (§3.2.1).
    let mut ungrouped = 0usize;
    if args.connection.ungrouped || args.only_ungrouped {
        let spinner = ProgressBar::new_spinner();
        spinner.enable_steady_tick(Duration::from_millis(100));
        spinner.set_style(
            ProgressStyle::with_template("{spinner:.green} {msg}").expect("template is valid"),
        );
        let extra = client.fetch_ungrouped(collection, &spinner);
        spinner.finish_and_clear();
        let extra = extra?;
        let mut seen: HashSet<i64> = items.iter().map(|item| item.puzzle_id).collect();
        for item in extra {
            if seen.insert(item.puzzle_id) {
                items.push(item);
                ungrouped += 1;
            }
        }
    } else if hidden > 0 {
        let total = pack.num_puzzles.unwrap_or(grouped + hidden);
        anstream::eprintln!(
            "{} {} of {total} levels sit directly on {}, outside its {groups} sub-packs; the \
             page never links to them. Re-run with --ungrouped to include them.",
            paint(anstyle::AnsiColor::Yellow, "warning:"),
            hidden,
            api::display_path(&pack.full_slug),
        );
    }

    let items: Vec<_> = match args.limit {
        Some(limit) => items.into_iter().take(limit).collect(),
        None => items,
    };
    let empty_message = if args.only_ungrouped {
        format!(
            "no levels sit outside the sub-packs of {}",
            api::display_path(&pack.full_slug)
        )
    } else {
        format!(
            "pack {} holds no puzzles",
            api::display_path(&pack.full_slug)
        )
    };
    ensure!(!items.is_empty(), "{empty_message}");

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
            "{} - kept {} · skipped {} · failed {}",
            item.label(),
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
                    if args.verbose {
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
                        if args.verbose {
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
                                if args.verbose {
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
        "{}: kept {}, skipped {}{}, failed {} of {fetched} fetched{} -> {}",
        paint(anstyle::AnsiColor::Green, "done"),
        paint(anstyle::AnsiColor::Green, kept.len()),
        paint(anstyle::AnsiColor::Yellow, skipped),
        skipped_by.breakdown(),
        paint(anstyle::AnsiColor::Red, failed),
        match (ungrouped, args.only_ungrouped) {
            (0, _) => String::new(),
            (n, true) => format!(
                ", only the {} outside the sub-packs",
                paint(anstyle::AnsiColor::Green, n)
            ),
            (n, false) => format!(
                ", {} included from outside the sub-packs",
                paint(anstyle::AnsiColor::Green, n)
            ),
        },
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
                num_puzzles: Some(1),
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

    #[test]
    fn subcommands_keep_their_own_options() {
        let cli = Cli::parse_from(["sokofetch", "info", "obyzz011", "--ungrouped"]);
        match cli.command {
            Command::Info(args) => {
                assert_eq!(args.connection.collection, "obyzz011");
                assert!(args.connection.ungrouped);
            }
            Command::Fetch(_) => panic!("expected the info subcommand"),
        }

        let cli = Cli::parse_from(["sokofetch", "fetch", "a/b", "-o", "x.txt", "--limit", "5"]);
        match cli.command {
            Command::Fetch(args) => {
                assert_eq!(args.output.as_deref(), Some(Path::new("x.txt")));
                assert_eq!(args.limit, Some(5));
            }
            Command::Info(_) => panic!("expected the fetch subcommand"),
        }

        // output options belong to `fetch` only, and a subcommand is required
        assert!(Cli::try_parse_from(["sokofetch", "info", "x", "-o", "y"]).is_err());
        assert!(Cli::try_parse_from(["sokofetch", "x"]).is_err());
    }

    // ---- end-to-end tests against a local mock of the API ----

    use crate::test_support::{
        Action, MockServer, Response, wire_detail, wire_listing, wire_pack, wire_pack_counted,
        wire_pack_item, wire_puzzle_item,
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

    fn connection(collection: &str) -> Connection {
        Connection {
            collection: collection.to_owned(),
            delay: 0.0,
            timeout: 5.0,
            ungrouped: false,
        }
    }

    /// `fetch` arguments pointed at `out`, with every option at its default.
    fn fetch_args(collection: &str, out: &Path) -> FetchArgs {
        FetchArgs {
            connection: connection(collection),
            output: Some(out.to_path_buf()),
            limit: None,
            verbose: false,
            only_ungrouped: false,
        }
    }

    fn fetch_cmd(collection: &str, out: &Path) -> Command {
        Command::Fetch(fetch_args(collection, out))
    }

    fn info_cmd(collection: &str) -> Command {
        Command::Info(InfoArgs {
            connection: connection(collection),
        })
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
                Some(wire_pack_counted("t1024", ids.len() as u64)),
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
        let code = run(fetch_cmd("t1024", &out), &client, &stop).unwrap();
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
        let mut args = fetch_args("a/b", Path::new("unused.txt"));
        args.output = None;
        assert_eq!(
            run(Command::Fetch(args), &client, &stop).unwrap(),
            ExitCode::SUCCESS
        );
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
        let code = run(fetch_cmd("t1024", &out), &client, &stop).unwrap();
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
        let mut args = fetch_args("t1024", &out);
        args.verbose = true;
        assert_eq!(
            run(Command::Fetch(args), &client, &stop).unwrap(),
            ExitCode::SUCCESS
        );
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
        let code = run(fetch_cmd("t1024", &out), &client, &stop).unwrap();
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
        let mut args = fetch_args("t1024", &out);
        args.limit = Some(1);
        assert_eq!(
            run(Command::Fetch(args), &client, &stop).unwrap(),
            ExitCode::SUCCESS
        );
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
        let code = run(fetch_cmd("t1024", Path::new("unused.txt")), &client, &stop).unwrap();
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
        let code = run(fetch_cmd("t1024", &out), &client, &stop).unwrap();
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
        let code = run(fetch_cmd("t1024", &out), &client, &stop).unwrap();
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
        let err = run(fetch_cmd("ghost", &out), &client, &stop).unwrap_err();
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
        assert_eq!(
            run(info_cmd("t1024"), &client, &stop).unwrap(),
            ExitCode::SUCCESS
        );
        assert_eq!(server.calls().len(), 1, "no puzzle details are fetched");
    }

    #[test]
    fn run_list_mode_reports_direct_puzzle_counts() {
        let (server, client, stop) = mock(|_call, _target| listing_of(&[11]));
        assert_eq!(
            run(info_cmd("t1024"), &client, &stop).unwrap(),
            ExitCode::SUCCESS
        );
        assert_eq!(server.calls().len(), 1);
    }

    #[test]
    fn run_validates_delay_timeout_collection_and_empty_packs() {
        let dir = scratch_dir("validation");
        let out = dir.join("out.txt");
        let (server, client, stop) = mock(|_call, _target| listing_of(&[]));

        let mut args = fetch_args("t1024", &out);
        args.connection.delay = -1.0;
        assert!(
            run(Command::Fetch(args), &client, &stop)
                .unwrap_err()
                .to_string()
                .contains("--delay must not be negative")
        );

        let mut args = fetch_args("t1024", &out);
        args.connection.timeout = 0.0;
        assert!(
            run(Command::Fetch(args), &client, &stop)
                .unwrap_err()
                .to_string()
                .contains("--timeout must be positive")
        );

        assert!(
            run(fetch_cmd("no such", &out), &client, &stop)
                .unwrap_err()
                .to_string()
                .contains("invalid collection path")
        );

        // an empty listing surfaces as "the pack holds no puzzles"
        assert!(
            run(fetch_cmd("t1024", &out), &client, &stop)
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

    // ---- levels outside every sub-pack (docs/API.md §3.2.1) ----

    const SITEMAP_INDEX: &str = r#"<sitemapindex>
<sitemap><loc>https://www.sokobanonline.com/sitemaps/packs.xml</loc></sitemap>
<sitemap><loc>https://www.sokobanonline.com/sitemaps/puzzles-1.xml</loc></sitemap>
</sitemapindex>"#;

    /// `99` sits directly on the author page, `11` is already listed through
    /// the sub-pack and must not be fetched twice.
    const SITEMAP_SHARD: &str = r#"<urlset>
<url><loc>https://www.sokobanonline.com/play/community/t1024/99_p99</loc></url>
<url><loc>https://www.sokobanonline.com/play/community/t1024/corner-set/11_p11</loc></url>
</urlset>"#;

    /// A container page whose sub-pack holds `11`, while the pack counter says
    /// the collection has one more level — the one that lives on the page.
    fn ungrouped_handler() -> impl Fn(usize, &str) -> Action + Send + Sync + 'static {
        |_call, target| match target {
            "/sitemap.xml" => Action::Respond(Response::plain(200, SITEMAP_INDEX)),
            "/sitemaps/puzzles-1.xml" => Action::Respond(Response::plain(200, SITEMAP_SHARD)),
            "/packs/by-path/community/t1024?page=1" => Action::Respond(Response::json(
                200,
                wire_listing(
                    Some(wire_pack_counted("t1024", 2)),
                    vec![wire_pack_item("community/t1024/corner-set")],
                    1,
                ),
            )),
            "/packs/by-path/community/t1024/corner-set?page=1" => Action::Respond(Response::json(
                200,
                wire_listing(
                    Some(wire_pack_counted("corner-set", 1)),
                    vec![wire_puzzle_item(11)],
                    1,
                ),
            )),
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
    fn run_ungrouped_adds_the_levels_that_live_on_the_page() {
        let dir = scratch_dir("ungrouped");
        let out = dir.join("out.txt");
        let (server, client, stop) = mock(ungrouped_handler());
        let mut args = fetch_args("t1024", &out);
        args.connection.ungrouped = true;
        assert_eq!(
            run(Command::Fetch(args), &client, &stop).unwrap(),
            ExitCode::SUCCESS
        );
        assert_eq!(
            server.calls(),
            [
                "/packs/by-path/community/t1024?page=1",
                "/packs/by-path/community/t1024/corner-set?page=1",
                "/sitemap.xml",
                "/sitemaps/puzzles-1.xml",
                "/puzzles/11",
                "/puzzles/99"
            ]
        );
        let text = std::fs::read_to_string(&out).expect("output file");
        assert!(text.contains("; 2 XSB levels of 2 fetched"));
        assert!(text.contains("Title: p11\n"));
        assert!(text.contains("Title: p99\n"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_only_ungrouped_skips_every_sub_pack() {
        let dir = scratch_dir("only-ungrouped");
        let out = dir.join("out.txt");
        let (server, client, stop) = mock(ungrouped_handler());
        let mut args = fetch_args("t1024", &out);
        args.only_ungrouped = true;
        assert_eq!(
            run(Command::Fetch(args), &client, &stop).unwrap(),
            ExitCode::SUCCESS
        );
        assert_eq!(
            server.calls(),
            [
                "/packs/by-path/community/t1024?page=1",
                "/sitemap.xml",
                "/sitemaps/puzzles-1.xml",
                "/puzzles/99"
            ],
            "the sub-pack is neither listed nor fetched"
        );
        let text = std::fs::read_to_string(&out).expect("output file");
        assert!(text.contains("; 1 XSB levels of 1 fetched"));
        assert!(text.contains("Title: p99\n"));
        assert!(!text.contains("Title: p11"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_only_ungrouped_rejects_a_page_without_sub_packs() {
        let dir = scratch_dir("only-ungrouped-flat");
        let out = dir.join("out.txt");
        let (_server, client, stop) = mock(|_call, _target| listing_of(&[11]));
        let mut args = fetch_args("t1024", &out);
        args.only_ungrouped = true;
        let err = run(Command::Fetch(args), &client, &stop).unwrap_err();
        assert!(err.to_string().contains("has no sub-packs"), "{err:#}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_only_ungrouped_fails_when_nothing_lives_on_the_page() {
        let dir = scratch_dir("only-ungrouped-empty");
        let out = dir.join("out.txt");
        let (_server, client, stop) = mock(|_call, target| match target {
            "/sitemap.xml" => Action::Respond(Response::plain(200, SITEMAP_INDEX)),
            "/sitemaps/puzzles-1.xml" => Action::Respond(Response::plain(
                200,
                "<urlset><url><loc>https://www.sokobanonline.com/play/community/t1024/corner-set/11_p11</loc></url></urlset>",
            )),
            "/packs/by-path/community/t1024?page=1" => Action::Respond(Response::json(
                200,
                wire_listing(
                    Some(wire_pack_counted("t1024", 1)),
                    vec![wire_pack_item("community/t1024/corner-set")],
                    1,
                ),
            )),
            _ => Action::Respond(Response::plain(500, "unexpected target")),
        });
        let mut args = fetch_args("t1024", &out);
        args.only_ungrouped = true;
        let err = run(Command::Fetch(args), &client, &stop).unwrap_err();
        assert!(err.to_string().contains("no levels sit outside"), "{err:#}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_without_ungrouped_never_touches_the_sitemap() {
        let dir = scratch_dir("grouped-only");
        let out = dir.join("out.txt");
        let (server, client, stop) = mock(ungrouped_handler());
        assert_eq!(
            run(fetch_cmd("t1024", &out), &client, &stop).unwrap(),
            ExitCode::SUCCESS
        );
        assert!(
            !server.calls().iter().any(|t| t.contains("sitemap")),
            "{:?}",
            server.calls()
        );
        let text = std::fs::read_to_string(&out).expect("output file");
        assert!(text.contains("; 1 XSB levels of 1 fetched"));
        assert!(!text.contains("Title: p99"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
