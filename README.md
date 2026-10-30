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
- **Whole collection**: A collection is its sub-packs *and* the levels sitting directly
  on its page. Both are always included; the site never links to the second kind, so
  they are recovered from its sitemap.
- **Resumable**: `--resume` keeps what an earlier run already wrote and fetches only
  the levels that appeared since.
- **Interruptible**: Ctrl+C still writes the levels fetched so far (exit code 130).

## Usage

```sh
sokofetch info  <username>              # what a collection holds, without fetching levels
sokofetch fetch <username>              # whole author (sub-packs merged) -> <username>.txt
sokofetch fetch <username>/<pack>       # one sub-pack -> <username>-<pack>.txt
sokofetch fetch <username> --resume     # only the levels missing from the existing file
sokofetch fetch <username> --limit 10   # small trial run first
sokofetch fetch <username> --delay 5    # more polite request pacing
sokofetch fetch <username> --verbose    # log every skipped level
```

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
