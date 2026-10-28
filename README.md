# sokofetch

> [!WARNING]  
> The site's `robots.txt` disallows `/api/`: the API is not meant to be crawled,
> so keep fetches one-off, low-volume, and personal-use. Do not redistribute the fetched levels;
> the copyright belongs to their authors.

A toolchain for fetching levels of an author (community pack) from [Sokoban Online]
and exporting them as an [XSB] level collection.

## Features

- **Fetch**: Downloads the pack listing and every puzzle detail, with request pacing
  and 429/5xx retry. `membercontainer` authors are fetched pack by pack.
- **XSB output**: An XSB collection with per-level metadata, validated by [soukoban].
- **Classic only**: Skips modern puzzles and boards the parser rejects
  (co-op, decoy boxes, ...).
- **Ungrouped levels**: `info` reports — and with `--ungrouped` both subcommands handle — the
  levels that sit directly on an author page instead of inside one of its sub-packs. The site
  itself never links to them (see [Levels outside every sub-pack](#levels-outside-every-sub-pack)).
- **Interruptible**: Ctrl+C still writes the levels fetched so far (exit code 130).

## Usage

```sh
sokofetch info  <username>              # what a collection holds, without fetching levels
sokofetch info  <username> --ungrouped  # ... and the levels outside every sub-pack
sokofetch fetch <username>              # whole author (sub-packs merged) -> <username>.txt
sokofetch fetch <username>/<pack>       # one sub-pack -> <username>-<pack>.txt
sokofetch fetch <username> --ungrouped  # also fetch levels that belong to no sub-pack
sokofetch fetch <username> --only-ungrouped  # fetch *only* those levels
sokofetch fetch <username> --limit 10   # small trial run first
sokofetch fetch <username> --delay 5    # more polite request pacing
sokofetch fetch <username> --verbose    # log every skipped level
```

`--delay`, `--timeout` and `--ungrouped` are accepted by both subcommands; `--output`,
`--limit`, `--verbose` and `--only-ungrouped` only make sense for `fetch`, so `info`
rejects them. `--only-ungrouped` implies `--ungrouped` and also skips listing the
sub-packs, so it is the cheapest way to top up a collection with the levels that were
missed.

## Levels outside every sub-pack

A Sokoban Online author page can hold sub-packs *and* its own levels at the same time, but
the API only ever returns one of the two: once a page has sub-packs, its listing is
sub-packs only. Levels sitting directly on the page therefore show up in `/play/new`,
open by direct URL, and are counted by the site ("503 puzzles in 16 groups" where the 16
groups only add up to 499) — but no page links to them.

`info` always reports the discrepancy, and `--ungrouped` recovers those levels from the
site's sitemap (the one enumeration path `robots.txt` advertises):

```console
$ sokofetch info obyzz011
obyzz011 · 503 puzzles in 16 sub-packs
  https://www.sokobanonline.com/play/community/obyzz011

  obyzz011/bridge             14  Bridge
  obyzz011/classic-9-boxes   195  Classic 9 boxes
  ...
  obyzz011/trees               8  Trees

  outside every sub-pack: 4 (not linked from the page; --ungrouped lists them)

$ sokofetch info obyzz011 --ungrouped
  ...
  outside every sub-pack: 4 (not linked from the page)
    135462  136063  136072  136074
```

`--ungrouped` costs 4 extra requests and ~16 MB of sitemap XML per run.

To download *only* those levels — e.g. to top up a collection fetched before they existed —
use `--only-ungrouped`. It skips the sub-packs entirely, so it needs one listing request
plus the sitemap instead of one per sub-pack:

```console
$ sokofetch fetch obyzz011 --only-ungrouped
done: kept 3, skipped 1 (1 modern), failed 0 of 4 fetched, only the 4 outside the sub-packs
```

A page without sub-packs is rejected: there every level is already on the page, so
"only the ungrouped ones" would silently mean "the whole collection".

## AI Disclosure

This project is developed entirely with LLM assistance. Reliability is verified by unit
tests only, without human code review.

## License

Licensed under either of

- [Apache License, Version 2.0](LICENSE-APACHE)
- [MIT license](LICENSE-MIT)

at your option.

[Sokoban Online]: https://www.sokofetch.com
[soukoban]: https://github.com/ShenMian/soukoban
[XSB]: https://sokobano.de/wiki/index.php?title=Level_format
