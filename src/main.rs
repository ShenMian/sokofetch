//! Inspect and fetch author collections from sokobanonline.com.
//!
//! A collection is a pack plus every level below it: the levels of its
//! sub-packs and the ones sitting directly on its page, which the site itself
//! never links to. `info` describes both kinds and `fetch` downloads both, so
//! neither needs a flag to be complete. The collection is addressed by the path
//! of its page relative to `sokobanonline.com/play/community/`: `<author>`,
//! `<author>/<pack>`, or `<author>/` for the levels on the page itself — the one
//! collection that has no name. Levels that an XSB loader cannot represent
//! (mechanisms, co-op, decoy boxes) are skipped; docs/API.md documents the API
//! and the board encoding.

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

use anyhow::{Context, bail, ensure};
use clap::{Args, Parser, Subcommand};
use indicatif::{ProgressBar, ProgressStyle};

use crate::api::{Client, Collection, Pack, Puzzle};

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
    /// directly on its page
    Info(InfoArgs),
    /// Fetch a collection's levels as an XSB level collection (.txt)
    Fetch(FetchArgs),
}

/// What every request-making subcommand shares.
#[derive(Debug, Args)]
struct Connection {
    /// Collection path: `<author>`, `<author>/<pack>`, or `<author>/` for the
    /// levels sitting directly on the author's page. `<author>` alone means the
    /// whole author (sub-packs merged); the trailing slash names the one
    /// collection that has no name of its own
    #[arg(value_name = "COLLECTION")]
    collection: String,

    /// Seconds to sleep between requests (robots.txt asks for 5)
    #[arg(long, value_name = "SECONDS", default_value_t = 1.0)]
    delay: f64,

    /// Seconds until a single request times out
    #[arg(long, value_name = "SECONDS", default_value_t = 30.0)]
    timeout: f64,
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

    /// Keep the levels already in the output file and fetch only the missing
    /// ones, appending them
    #[arg(long)]
    resume: bool,
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

/// What a collection path addresses.
///
/// An author page is a container holding named sub-collections (its sub-packs)
/// *and* one that has no name at all: the levels sitting directly on the page,
/// which no sub-pack contains (docs/API.md §3.2.1). The path mirrors that, so
/// the empty name after the slash is the unnamed collection rather than a
/// separate mode.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Target {
    /// `<author>`: the author and every collection below it.
    Author(String),
    /// `<author>/<pack>`: one named sub-collection.
    Pack(String),
    /// `<author>/`: the collection that has no name.
    Page(String),
}

impl Target {
    /// Path after `community/`, i.e. what the API is asked about.
    fn path(&self) -> &str {
        match self {
            Self::Author(author) | Self::Page(author) => author,
            Self::Pack(pack) => pack,
        }
    }
}

/// Validate the collection path and say what it addresses: `<author>`,
/// `<author>/<pack>`, or `<author>/` for the one collection that has no name.
/// Allowed characters per segment are `[a-zA-Z0-9_-]`.
fn parse_collection(input: &str) -> anyhow::Result<Target> {
    let trimmed = input.trim();
    ensure!(!trimmed.is_empty(), "empty collection path");
    // A trailing slash names the empty collection, i.e. the levels sitting on
    // the page itself. A pack holds no sub-packs, so there the empty name
    // resolves to the pack's own levels — the same thing.
    let (path, unnamed) = match trimmed.strip_suffix('/') {
        Some(rest) => (rest, true),
        None => (trimmed, false),
    };
    ensure!(
        path.split('/').all(|segment| {
            !segment.is_empty()
                && segment
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        }),
        "invalid collection path {trimmed:?} (expected <author>[/<pack>], \
         allowed characters [a-zA-Z0-9_-])"
    );
    Ok(match path.split_once('/') {
        None if unnamed => Target::Page(path.to_owned()),
        None => Target::Author(path.to_owned()),
        Some(_) => Target::Pack(path.to_owned()),
    })
}

/// Default output name: path segments joined with `-`, so collections of
/// different authors cannot silently overwrite each other. The unnamed
/// collection gets a `-page` suffix rather than a dangling `-`.
fn default_output(target: &Target) -> PathBuf {
    match target {
        Target::Author(author) => PathBuf::from(format!("{author}.txt")),
        Target::Pack(pack) => PathBuf::from(format!("{}.txt", pack.replace('/', "-"))),
        Target::Page(author) => PathBuf::from(format!("{author}-page.txt")),
    }
}

fn run(command: Command, client: &Client, stop: &Arc<AtomicBool>) -> anyhow::Result<ExitCode> {
    validate(command.connection())?;
    let target = parse_collection(&command.connection().collection)?;

    match command {
        Command::Info(_) => {
            info(client, &target)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Fetch(args) => {
            let out_path = args
                .output
                .clone()
                .unwrap_or_else(|| default_output(&target));
            let interrupted = fetch_and_write(client, &target, &out_path, &args, stop)?;
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

/// Print what a collection path addresses.
fn info(client: &Client, target: &Target) -> anyhow::Result<()> {
    match target {
        Target::Page(author) => info_unnamed(client, author),
        _ => info_container(client, target),
    }
}

/// Describe the collection that has no name: the levels sitting directly on an
/// author's page, outside every sub-pack.
fn info_unnamed(client: &Client, author: &str) -> anyhow::Result<()> {
    let spinner = ProgressBar::new_spinner();
    spinner.enable_steady_tick(Duration::from_millis(100));
    let (pack, listing) = client.list_pack(author, &spinner)?;
    // Nothing is named on a flat page, so its unnamed collection is all of it.
    // Elsewhere the sitemap is the only way to know which levels these are, so
    // it is read even when the counters claim there are none: they are the
    // site's own tally and can be stale, while this is the whole answer.
    let levels = if listing.packs.is_empty() {
        listing.puzzles
    } else {
        client.fetch_page_levels(author, &spinner)?
    };
    spinner.finish_and_clear();

    if levels.is_empty() {
        anstream::println!("{} · no levels directly on the page", pack.name);
        anstream::println!("  https://www.sokobanonline.com{}", pack.path);
        return Ok(());
    }
    anstream::println!(
        "{} · {} levels directly on the page",
        pack.name,
        levels.len()
    );
    anstream::println!("  https://www.sokobanonline.com{}", pack.path);
    anstream::println!();
    for chunk in levels.chunks(6) {
        let ids: Vec<String> = chunk
            .iter()
            .map(|item| item.puzzle_id.to_string())
            .collect();
        anstream::println!("  {}", ids.join("  "));
    }
    Ok(())
}

/// Print what a container holds: its sub-packs, plus the levels that sit
/// directly on the page rather than inside one of them (docs/API.md §3.2.1).
/// Both kinds are rendered the same way — as rows of one table.
fn info_container(client: &Client, target: &Target) -> anyhow::Result<()> {
    let collection = target.path();
    let spinner = ProgressBar::new_spinner();
    spinner.enable_steady_tick(Duration::from_millis(100));
    let (pack, listing) = client.list_pack(collection, &spinner)?;
    spinner.finish_and_clear();

    let total = pack
        .num_puzzles
        .map_or_else(|| "?".to_owned(), |n| n.to_string());
    if listing.packs.is_empty() {
        anstream::println!(
            "{} · {} puzzles{}",
            pack.name,
            listing.puzzles.len(),
            match target {
                Target::Author(_) => ", all directly on the page",
                _ => "",
            }
        );
        anstream::println!("  https://www.sokobanonline.com{}", pack.path);
        return Ok(());
    }

    // The counters only say *how many* levels sit on the page, never which
    // ones; naming them costs the sitemap scan, so it is skipped whenever the
    // arithmetic proves there are none.
    let grouped: Option<u64> = listing.packs.iter().map(|sub| sub.num_puzzles).sum();
    let hidden = api::hidden_estimate(&pack, grouped, listing.puzzles.len() as u64);
    let on_page = if hidden == Some(0) {
        Vec::new()
    } else {
        let spinner = ProgressBar::new_spinner();
        spinner.enable_steady_tick(Duration::from_millis(100));
        let levels = client.fetch_page_levels(collection, &spinner);
        spinner.finish_and_clear();
        levels?
    };

    let suffix = if on_page.is_empty() {
        String::new()
    } else {
        format!(" · {} on the page", on_page.len())
    };
    anstream::println!(
        "{} · {} puzzles in {} sub-packs{suffix}",
        pack.name,
        total,
        listing.packs.len()
    );
    anstream::println!("  https://www.sokobanonline.com{}", pack.path);

    let width = listing
        .packs
        .iter()
        .map(|sub| api::display_path(&sub.full_slug).chars().count())
        .chain(std::iter::once("(page)".len()))
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
    if !on_page.is_empty() {
        anstream::println!(
            "  {:<width$}  {:>4}  levels directly on the page",
            "(page)",
            on_page.len(),
            width = width
        );
        // The sitemap carries ids, not names; they are what `fetch` works from.
        let indent = " ".repeat(width + 10);
        for chunk in on_page.chunks(6) {
            let ids: Vec<String> = chunk
                .iter()
                .map(|item| item.puzzle_id.to_string())
                .collect();
            anstream::println!("{indent}{}", ids.join("  "));
        }
    }
    Ok(())
}

/// Fetch one collection and write its XSB collection file.
/// Returns `true` when the user interrupted the fetch.
fn fetch_and_write(
    client: &Client,
    target: &Target,
    out_path: &Path,
    args: &FetchArgs,
    stop: &Arc<AtomicBool>,
) -> anyhow::Result<bool> {
    // Phase 1: learn the puzzle ids — the collection listing, its sub-packs,
    // and, only when the counters show levels no sub-pack holds, the sitemap.
    let spinner = ProgressBar::new_spinner();
    spinner.enable_steady_tick(Duration::from_millis(100));
    spinner.set_style(
        ProgressStyle::with_template("{spinner:.green} {msg}").expect("template is valid"),
    );
    spinner.set_message(format!("listing {} ...", target.path()));
    let collected = match target {
        // The unnamed collection: one listing to confirm the author exists and
        // to know whether anything is named, then only the levels on the page.
        Target::Page(author) => client
            .list_pack(author, &spinner)
            .and_then(|(pack, listing)| {
                // The counters only hint at how many there are, and this
                // collection is exactly what was asked for: read the sitemap.
                let items = if listing.packs.is_empty() {
                    // nothing is named, so the unnamed collection is the page
                    listing.puzzles
                } else {
                    client.fetch_page_levels(author, &spinner)?
                };
                Ok(Collection {
                    pack,
                    items,
                    hidden: Some(0),
                })
            }),
        _ => client.fetch_pack(target.path(), &spinner),
    };
    let collected = match collected {
        Ok(collected) => collected,
        Err(err) => {
            spinner.finish_and_clear();
            if stop.load(Ordering::Relaxed) {
                anstream::eprintln!("interrupted");
                return Ok(true);
            }
            return Err(err);
        }
    };
    let on_page = if collected.hidden == Some(0) {
        Vec::new()
    } else {
        let levels = client.fetch_page_levels(target.path(), &spinner)?;
        let mut seen: HashSet<i64> = collected.items.iter().map(|item| item.puzzle_id).collect();
        levels
            .into_iter()
            .filter(|item| seen.insert(item.puzzle_id))
            .collect()
    };
    spinner.finish_and_clear();
    let pack = collected.pack;
    let mut items = collected.items;
    items.extend(on_page);

    // Everything already in the output file is left alone when resuming, so a
    // re-run only pays for the levels that appeared since the last one.
    let present = if args.resume {
        existing_ids(out_path)?
    } else {
        HashSet::new()
    };
    let already = items
        .iter()
        .filter(|item| present.contains(&item.puzzle_id))
        .count();
    items.retain(|item| !present.contains(&item.puzzle_id));

    let items: Vec<_> = match args.limit {
        Some(limit) => items.into_iter().take(limit).collect(),
        None => items,
    };
    if items.is_empty() {
        if already > 0 {
            anstream::println!(
                "{}: nothing new in {} ({} levels already in {})",
                paint(anstyle::AnsiColor::Green, "done"),
                api::display_path(&pack.full_slug),
                paint(anstyle::AnsiColor::Green, already),
                out_path.display(),
            );
            return Ok(false);
        }
        bail!(
            "{}",
            match target {
                Target::Page(_) => format!(
                    "no levels sit directly on the page of {}",
                    api::display_path(&pack.full_slug)
                ),
                _ => format!(
                    "pack {} holds no puzzles",
                    api::display_path(&pack.full_slug)
                ),
            }
        );
    }

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
    let mut fetched = 0usize; // successful detail fetches, reported in the summary
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
                note(&progress, &format!("  failed {}: {err:#}", item.label()));
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

    write_output(out_path, &pack, &kept, args.resume)?;

    anstream::println!(
        "{}: kept {}, skipped {}{}, failed {} of {fetched} fetched{} -> {}",
        paint(anstyle::AnsiColor::Green, "done"),
        paint(anstyle::AnsiColor::Green, kept.len()),
        paint(anstyle::AnsiColor::Yellow, skipped),
        skipped_by.breakdown(),
        paint(anstyle::AnsiColor::Red, failed),
        if already > 0 {
            format!(
                ", {} already present",
                paint(anstyle::AnsiColor::Green, already)
            )
        } else {
            String::new()
        },
        out_path.display(),
    );
    Ok(interrupted)
}

/// Write the collection, appending to an existing file when resuming. The
/// header is rewritten either way, so its level count always describes the
/// file as a whole rather than the levels this run happened to fetch.
fn write_output(
    path: &Path,
    pack: &Pack,
    kept: &[(Puzzle, String)],
    append: bool,
) -> anyhow::Result<()> {
    let blocks = render_blocks(pack, kept);
    let body = if append && path.exists() {
        let existing =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let mut body = strip_header(&existing).trim_end().to_owned();
        if !body.is_empty() && !blocks.is_empty() {
            body.push_str("\n\n");
        }
        body.push_str(&blocks);
        body
    } else {
        blocks
    };
    let levels = body
        .lines()
        .filter(|line| line.starts_with("; URL:"))
        .count();
    let text = format!("{}{body}", render_header(pack, levels));
    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))
}

/// Drop the leading `;` comment block (and the blank line after it), keeping
/// the level blocks byte for byte.
fn strip_header(content: &str) -> &str {
    let mut offset = 0;
    for line in content.split_inclusive('\n') {
        let trimmed = line.trim();
        if !(trimmed.is_empty() || trimmed.starts_with(';')) {
            return &content[offset..];
        }
        offset += line.len();
    }
    ""
}

/// Puzzle ids already recorded in an output file, read back from its `; URL:`
/// lines. A missing file simply has none.
fn existing_ids(path: &Path) -> anyhow::Result<HashSet<i64>> {
    if !path.exists() {
        return Ok(HashSet::new());
    }
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(text
        .lines()
        .filter_map(|line| line.strip_prefix("; URL: "))
        .filter_map(|url| url.rsplit('/').next())
        .filter_map(|tail| match tail.split_once('_') {
            Some((id, _)) => id.parse().ok(),
            None => tail.parse().ok(),
        })
        .collect())
}

/// The three `;` header lines, followed by the blank line that separates them
/// from the first level block.
fn render_header(pack: &Pack, levels: usize) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "; Sokoban Online - {} ({})", pack.name, pack.path);
    let _ = writeln!(out, "; {levels} XSB levels");
    let _ = writeln!(
        out,
        "; classic puzzles only - modern puzzles are excluded\n"
    );
    out
}

/// Render the level blocks: each is the XSB map followed by `Title:` /
/// `Author:` / `URL:` metadata lines, which soukoban parses as level metadata.
fn render_blocks(pack: &Pack, kept: &[(Puzzle, String)]) -> String {
    let mut out = String::new();
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
        let out = format!(
            "{}{}",
            render_header(&pack, 1),
            render_blocks(&pack, &[(puzzle, "####".into())])
        );
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
        let out = format!(
            "{}{}",
            render_header(&pack, 1),
            render_blocks(&pack, &[(puzzle, "#####\n#@$.#\n#####".into())])
        );
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
        let author = |s: &str| Target::Author(s.to_owned());
        let pack = |s: &str| Target::Pack(s.to_owned());
        let page = |s: &str| Target::Page(s.to_owned());

        assert_eq!(parse_collection("a").unwrap(), author("a"));
        assert_eq!(parse_collection(" a/b ").unwrap(), pack("a/b"));
        // the empty name after the slash is the collection that has none
        assert_eq!(parse_collection("a/").unwrap(), page("a"));
        assert_eq!(parse_collection(" a/ ").unwrap(), page("a"));
        // a pack holds no sub-packs, so its empty name resolves to itself
        assert_eq!(parse_collection("a/b/").unwrap(), pack("a/b"));

        assert!(parse_collection("a//").is_err()); // empty segment
        assert!(parse_collection("").is_err());
        assert!(parse_collection(" ").is_err());
        assert!(parse_collection("/").is_err()); // no author at all
        assert!(parse_collection("https://example.com").is_err()); // ':' invalid
        assert!(parse_collection("a b").is_err()); // space invalid
    }

    #[test]
    fn default_output_joins_path_segments() {
        assert_eq!(
            default_output(&Target::Author("t1024".into())),
            PathBuf::from("t1024.txt")
        );
        assert_eq!(
            default_output(&Target::Pack("t1024/corner-set".into())),
            PathBuf::from("t1024-corner-set.txt")
        );
        assert_eq!(
            default_output(&Target::Pack("a/b/c".into())),
            PathBuf::from("a-b-c.txt")
        );
        // the unnamed collection never shares a file with the whole author
        assert_eq!(
            default_output(&Target::Page("t1024".into())),
            PathBuf::from("t1024-page.txt")
        );
    }

    #[test]
    fn subcommands_keep_their_own_options() {
        let cli = Cli::parse_from(["sokofetch", "info", "obyzz011"]);
        match cli.command {
            Command::Info(args) => assert_eq!(args.connection.collection, "obyzz011"),
            Command::Fetch(_) => panic!("expected the info subcommand"),
        }

        let cli = Cli::parse_from([
            "sokofetch",
            "fetch",
            "a/b",
            "-o",
            "x.txt",
            "--limit",
            "5",
            "--resume",
        ]);
        match cli.command {
            Command::Fetch(args) => {
                assert_eq!(args.output.as_deref(), Some(Path::new("x.txt")));
                assert_eq!(args.limit, Some(5));
                assert!(args.resume);
            }
            Command::Info(_) => panic!("expected the fetch subcommand"),
        }

        // output options belong to `fetch` only, and a subcommand is required
        assert!(Cli::try_parse_from(["sokofetch", "info", "x", "-o", "y"]).is_err());
        assert!(Cli::try_parse_from(["sokofetch", "x"]).is_err());
        // both subcommands cover the whole collection: there is nothing to opt in
        assert!(Cli::try_parse_from(["sokofetch", "info", "x", "--ungrouped"]).is_err());
        assert!(Cli::try_parse_from(["sokofetch", "fetch", "x", "--only-ungrouped"]).is_err());
    }

    // ---- end-to-end tests against a local mock of the API ----

    use crate::test_support::{
        Action, MockServer, Response, wire_detail, wire_listing, wire_pack_counted, wire_pack_item,
        wire_pack_item_counted, wire_puzzle_item,
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
        }
    }

    /// `fetch` arguments pointed at `out`, with every option at its default.
    fn fetch_args(collection: &str, out: &Path) -> FetchArgs {
        FetchArgs {
            connection: connection(collection),
            output: Some(out.to_path_buf()),
            limit: None,
            verbose: false,
            resume: false,
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
        assert!(text.contains("; 2 XSB levels"));
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
        assert!(text.contains("; 1 XSB levels"));
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
        assert!(text.contains("; 1 XSB levels"));
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
        assert!(text.contains("; 0 XSB levels"));
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
        assert!(text.contains("; 1 XSB levels"));
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
        assert!(text.contains("; 0 XSB levels"));
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
                    // the counter accounts for both sub-packs (3 + 3), so no
                    // level sits on the page and no sitemap scan is needed
                    Some(wire_pack_counted("t1024", 6)),
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

    // ---- levels that sit directly on the page (docs/API.md §3.2.1) ----

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
    fn page_levels_handler() -> impl Fn(usize, &str) -> Action + Send + Sync + 'static {
        |_call, target| match target {
            "/sitemap.xml" => Action::Respond(Response::plain(200, SITEMAP_INDEX)),
            "/sitemaps/puzzles-1.xml" => Action::Respond(Response::plain(200, SITEMAP_SHARD)),
            "/packs/by-path/community/t1024?page=1" => Action::Respond(Response::json(
                200,
                wire_listing(
                    Some(wire_pack_counted("t1024", 2)),
                    // the counter matches the one level the sub-pack holds, so
                    // the arithmetic alone says one level sits on the page
                    vec![wire_pack_item_counted("community/t1024/corner-set", 1)],
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
    fn run_fetches_the_levels_that_live_on_the_page() {
        let dir = scratch_dir("page-levels");
        let out = dir.join("out.txt");
        let (server, client, stop) = mock(page_levels_handler());
        assert_eq!(
            run(fetch_cmd("t1024", &out), &client, &stop).unwrap(),
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
        assert!(text.contains("; 2 XSB levels"));
        assert!(text.contains("Title: p11\n"));
        assert!(text.contains("Title: p99\n"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_skips_the_sitemap_when_the_counters_prove_nothing_is_hidden() {
        let dir = scratch_dir("no-page-levels");
        let out = dir.join("out.txt");
        let (server, client, stop) = mock(|_call, target| match target {
            "/packs/by-path/community/t1024?page=1" => Action::Respond(Response::json(
                200,
                wire_listing(
                    Some(wire_pack_counted("t1024", 1)),
                    vec![wire_pack_item_counted("community/t1024/corner-set", 1)],
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
            t if t.starts_with("/puzzles/") => detail_action(11),
            _ => Action::Respond(Response::plain(500, "unexpected target")),
        });
        assert_eq!(
            run(fetch_cmd("t1024", &out), &client, &stop).unwrap(),
            ExitCode::SUCCESS
        );
        assert!(
            !server.calls().iter().any(|t| t.contains("sitemap")),
            "the counters account for every level: {:?}",
            server.calls()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_unnamed_collection_fetches_only_the_levels_on_the_page() {
        let dir = scratch_dir("unnamed");
        let out = dir.join("out.txt");
        let (server, client, stop) = mock(page_levels_handler());
        assert_eq!(
            run(fetch_cmd("t1024/", &out), &client, &stop).unwrap(),
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
        assert!(text.contains("; 1 XSB levels"));
        assert!(text.contains("Title: p99\n"));
        assert!(!text.contains("Title: p11"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_a_pack_with_a_trailing_slash_is_still_that_pack() {
        let dir = scratch_dir("pack-slash");
        let out = dir.join("out.txt");
        let (server, client, stop) = mock(fetch_handler(&[11, 12]));
        assert_eq!(
            run(fetch_cmd("t1024/corner-set/", &out), &client, &stop).unwrap(),
            ExitCode::SUCCESS
        );
        assert_eq!(
            server.calls(),
            [
                "/packs/by-path/community/t1024/corner-set?page=1",
                "/puzzles/11",
                "/puzzles/12"
            ],
            "a pack holds no sub-packs, so the empty name adds nothing"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn info_unnamed_collection_lists_only_the_levels_on_the_page() {
        let (server, client, stop) = mock(page_levels_handler());
        assert_eq!(
            run(info_cmd("t1024/"), &client, &stop).unwrap(),
            ExitCode::SUCCESS
        );
        assert_eq!(
            server.calls(),
            [
                "/packs/by-path/community/t1024?page=1",
                "/sitemap.xml",
                "/sitemaps/puzzles-1.xml"
            ],
            "no sub-pack listing and no puzzle details"
        );
    }

    /// A file holding just level `11`, as an earlier run would have left it.
    const RESUME_FILE: &str = "; Sokoban Online - t1024 (/play/community/t1024)
; 1 XSB levels
; classic puzzles only - modern puzzles are excluded

#####
#@$.#
#####
Title: p11
Author: T1024
; URL: https://www.sokobanonline.com/play/community/t1024/11_p11
";

    #[test]
    fn run_resume_only_fetches_the_levels_missing_from_the_file() {
        let dir = scratch_dir("resume");
        let out = dir.join("out.txt");
        std::fs::write(&out, RESUME_FILE).expect("seed output file");
        let (server, client, stop) = mock(page_levels_handler());
        let mut args = fetch_args("t1024", &out);
        args.resume = true;
        assert_eq!(
            run(Command::Fetch(args), &client, &stop).unwrap(),
            ExitCode::SUCCESS
        );
        assert_eq!(
            server
                .calls()
                .iter()
                .filter(|t| t.starts_with("/puzzles/"))
                .count(),
            1,
            "only the missing level is fetched: {:?}",
            server.calls()
        );
        let text = std::fs::read_to_string(&out).expect("output file");
        assert_eq!(
            text.matches("Title: p11\n").count(),
            1,
            "the level already in the file is not duplicated"
        );
        assert!(text.contains("Title: p99\n"));
        // the header describes the file, not the levels this run happened to add
        assert!(text.contains("; 2 XSB levels"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_resume_reports_when_the_file_is_already_complete() {
        let dir = scratch_dir("resume-complete");
        let out = dir.join("out.txt");
        let (server, client, stop) = mock(page_levels_handler());
        assert_eq!(
            run(fetch_cmd("t1024", &out), &client, &stop).unwrap(),
            ExitCode::SUCCESS
        );
        let first = std::fs::read_to_string(&out).expect("output file");

        let calls_before = server.calls().len();
        let mut args = fetch_args("t1024", &out);
        args.resume = true;
        assert_eq!(
            run(Command::Fetch(args), &client, &stop).unwrap(),
            ExitCode::SUCCESS
        );
        assert_eq!(
            server.calls().len() - calls_before,
            4,
            "the listings and the sitemap, no detail fetch: {:?}",
            &server.calls()[calls_before..]
        );
        assert_eq!(
            std::fs::read_to_string(&out).expect("output file"),
            first,
            "a complete file is left untouched"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_resume_without_an_existing_file_fetches_everything() {
        let dir = scratch_dir("resume-fresh");
        let out = dir.join("out.txt");
        let (server, client, stop) = mock(page_levels_handler());
        let mut args = fetch_args("t1024", &out);
        args.resume = true;
        assert_eq!(
            run(Command::Fetch(args), &client, &stop).unwrap(),
            ExitCode::SUCCESS
        );
        assert_eq!(
            server
                .calls()
                .iter()
                .filter(|t| t.starts_with("/puzzles/"))
                .count(),
            2
        );
        let text = std::fs::read_to_string(&out).expect("output file");
        assert!(text.contains("; 2 XSB levels"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
