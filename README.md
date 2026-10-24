# sokofetch

!!! warning Compliance
    The site's `robots.txt` disallows `/api/`: the API is not meant to be crawled, so keep
    fetches one-off, low-volume, and personal-use. Do not redistribute the fetched levels;
    the copyright belongs to their authors.

A toolchain for fetching levels of an author (community pack) from [Sokoban Online]
and exporting them as an [XSB] level collection.

## Features

- **Fetch**: Downloads the pack listing and every puzzle detail, with request pacing
  and 429/5xx retry. `membercontainer` authors are fetched pack by pack.
- **XSB output**: An XSB collection with per-level metadata, validated by [soukoban].
- **Classic only**: Skips modern puzzles and boards the parser rejects
  (co-op, decoy boxes, ...).
- **Interruptible**: Ctrl+C still writes the levels fetched so far (exit code 130).

## Usage

```sh
sokofetch <username>                 # whole author (sub-packs merged)
sokofetch <username>/<pack>          # one sub-pack -> <username>-<pack>.txt
sokofetch <username> --list          # show the sub-packs without fetching
sokofetch <username> --limit 10      # small trial run first
sokofetch <username> --delay 5       # more polite request pacing
sokofetch <username> --verbose       # log every skipped level
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
