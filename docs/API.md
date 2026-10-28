# Sokoban Online API Format Documentation & Data Profile

> Reverse-engineered + empirically measured data profile. Data source: `https://www.sokobanonline.com`  
> Profile sample: all 459 levels by author `soko7734` (scraped on 2026-10-23, 0 failures).  
> ⚠️ The site's robots.txt declares `Disallow: /api/` (Crawl-delay: 5) — low-frequency, personal use only.

## 1. Overview

| Item           | Value                                                                                                        |
| -------------- | ------------------------------------------------------------------------------------------------------------ |
| Architecture   | Vue 3 SPA (built with Vite); the frontend fetches all data from the REST API                                 |
| API Base       | `https://www.sokobanonline.com/api/v1`                                                                       |
| Authentication | Public read endpoints require no login; write operations (submitting solutions, etc.) require session + CSRF |
| Content type   | `application/json`                                                                                           |
| Server         | Health check exposes database + redis; version `0.1.0`                                                       |

Discovery method: the main JS bundle (`/assets/index-B4hV3n8a.js`) contains only relative paths (e.g. `` `/puzzles/${e}` ``);  
the base prefix was confirmed via the browser Network panel (`performance.getEntriesByType('resource')`).

## 2. Common Envelope (Identical Across All Endpoints)

```jsonc
// Success
{
  "status": "success",
  "meta": { "timestamp": "2026-10-23T18:53:26.955Z", "requestid": "…",
            /* list endpoints additionally have: */ "page": 1, "perpage": 30, "total": 459 },
  "data": { /* business data */ }
}

// Failure (the HTTP status code also reflects the error category)
{
  "status": "error",
  "meta": { "timestamp": "…", "requestid": "…" },
  "data": { "error": { "code": "NOT_FOUND", "message": "Puzzle not found." } }
}
```

Error model as observed:

| HTTP | Trigger                                                 | `code`      |
| ---- | ------------------------------------------------------- | ----------- |
| 404  | Nonexistent resource (e.g. `/api/v1/puzzles/999999999`) | `NOT_FOUND` |
| 429  | Rate limit triggered (response carries `Retry-After`)   | —           |
| 401  | Accessing `/me/*` without login                         | —           |

Pagination: list endpoints use `?page=N`; `perpage` is fixed server-side (passing `perpage=100` was observed to be ignored — still 30).  
Out-of-range pages (`page=999`) return 200 with `items: []`; `meta` echoes the requested page number as-is.  
Some other endpoints (leaderboards etc.) use cursor pagination instead (`?cursor=…&limit=…`; see the `$i()` helper in the frontend source).

## 3. Endpoints

### 3.1 `GET /api/v1/health` ✅ Verified

```jsonc
// data:
{ "version": "0.1.0",
  "checks": { "database": { "ok": true, "latencyms": 2.3 },
              "redis":    { "ok": true, "latencyms": 0.3 } } }
```

### 3.2 `GET /api/v1/packs/by-path/{fullslug}?page=N` ✅ Verified

Fetch a pack's level list by its path (an author page is itself a pack: `community/{author-slug}`).

- `fullslug` = `community/soko7734`
- `meta.total` in the response is the pack's total level count; `data.items` holds 30 entries per page
- `data.kind` is decided **by whether the pack owns child packs**, not by its `packtype`:  
  `"packs"` (items are child-pack summaries with `packid`/`name`/`fullslug`/`path`/`numpuzzles`)  
  when `numpacks > 0`, otherwise `"puzzles"` (items are level summaries with `puzzleid`).  
  A `membercontainer` author with **no** child packs therefore returns `"puzzles"`  
  (observed for `soko7734`: `packtype=membercontainer`, `numpacks=0`, `kind=puzzles`).  
  The Rust CLI scrapes each child pack of a `"packs"` page (one level of nesting).
- ⚠️ A pack can own **both** child packs and its own puzzles, but `kind="packs"` lists only the  
  child packs. See §3.2.1 — this makes such puzzles unreachable from the UI and invisible to the CLI.

`data.pack` fields:

| Field                    | Type    | Description / Observed values                                              |
| ------------------------ | ------- | -------------------------------------------------------------------------- |
| `packid`                 | int     | 1846                                                                       |
| `name`                   | string  | "soko7734"                                                                 |
| `fullslug`               | string  | "community/soko7734"                                                       |
| `path`                   | string  | "/play/community/soko7734"                                                 |
| `teaser`                 | string? | null (empty in all observations)                                           |
| `packtype`               | string  | `"membercontainer"`                                                        |
| `numpuzzles`             | int     | 459 (= `meta.total`)                                                       |
| `numpacks`               | int     | 0                                                                          |
| `cover`                  | object  | `{kind:"puzzle", base:"/thumbs/v7/m/2d/<hash>"}` thumbnail prefix          |
| `art`                    | object  | `{kind:"avatar", avatar:"4.10.18.3.8.0.4.1.18.2.4"}` avatar pixel encoding |
| `lock`                   | object? | null (paid lock; never observed non-empty)                                 |
| `awardstrophies`         | bool    | true                                                                       |
| `description`            | string? | null                                                                       |
| `creditsname/creditsurl` | string? | null                                                                       |
| `listed`                 | bool    | true                                                                       |
| `owner`                  | object  | `{slug:"soko7734"}`                                                        |
| `ancestors`              | array   | Chain of parent packs (Community → …)                                      |

`data.items[]` (metadata per level, **board not included**):

| Field            | Type   | Description / Observations across 459 levels                                      |
| ---------------- | ------ | --------------------------------------------------------------------------------- |
| `puzzleid`       | int    | Globally unique, 131697–136005                                                    |
| `name`           | string | Level name; the author reuses names (series names like class01 appear repeatedly) |
| `path`           | string | `/play/community/soko7734/{id}_{name}`                                            |
| `puzzletype`     | string | `"modern"` ×392 / `"classic"` ×67                                                 |
| `thumb`          | object | Thumbnail (path derived from designhash)                                          |
| `width`/`height` | int    | 8–43 / 6–42, both with median 15                                                  |
| `pack`           | object | Redundant pack summary                                                            |
| `author`         | object | `{displayname, slug, avatar, folder}`                                             |
| `numsolve`       | int    | 2–74                                                                              |
| `numuniquesolve` | int    | ≤ numsolve                                                                        |
| `numlike`        | int    | Can be negative (-3…8), net upvotes minus downvotes                               |
| `numlikevotes`   | int    | Total like/dislike votes                                                          |
| `isjustforfun`   | bool   | false (excluded from rankings)                                                    |
| `awardsplacings` | bool   | true                                                                              |
| `bests`          | int[3] | Three integers (gold/silver/bronze tiers, presumably move-count thresholds)       |
| `publisheddate`  | string | ISO 8601 UTC, 2023-08-18 → 2026-10-20, no nulls                                   |
| `position`       | int    | Position within pack 1..N, no duplicates                                          |
| `guestlocked`    | bool   | false (no guest locks in this pack)                                               |

### 3.2.1 Container packs that own puzzles directly ⚠️ Verified 2026-10-28

A puzzle always belongs to exactly one pack, but that pack does not have to be a leaf group —  
it can be the author's root `membercontainer` pack. Such a container holds **two kinds of  
children at once**: its sub-packs *and* its own puzzles, and the API only ever exposes one of them  
per response (`kind="packs"` → sub-packs only).

Consequences:

- The author page silently hides those puzzles: `/play/community/obyzz011` reports  
  `Puzzles: 503 in 16 groups`, yet the 16 child packs add up to 499. The remaining **4** puzzles sit  
  directly in pack `2110` (`packtotal=4`, `position` 1–4) and appear nowhere on the page.
- They are still `listed=true` and still show up in the global feeds (`/play/new?type=classic`),  
  because those feeds are pack-independent. They are reachable only by direct URL  
  (e.g. `/play/community/obyzz011/136063_46-steps`) or by `prev`/`next` from a sibling.
- Guests have no UI path to them at all. Logged-in users can get a flat puzzle list for a pack from  
  `GET /api/v1/me/packs/{packid}/puzzles?filter={unsolved|all-solved|gold|silver|bronze|no-trophy-solved}`  
  (401 without a session; this is what the "Filter puzzles" chips call when `kind="packs"`).
- A `"packs"` page's `numpuzzles` therefore counts `sum(child packs) + own puzzles`. This explains the  
  parent/child count mismatch noted earlier for `t1024` (2026-10-28: parent 103, 9 child packs summing 99).

Observed instances (2026-10-28):

| Author   | Container pack | `numpuzzles` | Child packs | Sum | Orphans                          |
| -------- | -------------- | ------------ | ----------- | --- | -------------------------------- |
| obyzz011 | 2110           | 503          | 16          | 499 | 4                                |
| t1024    | 2187           | 103          | 9           | 99  | 4                                |
| aymdup   | —              | 94           | 4           | 94  | 0                                |
| soko7734 | 1846           | 461          | 0           | —   | 0 (no sub-packs, `kind=puzzles`) |


**Impact on this CLI**: scraping a `"packs"` author page misses exactly these orphans — they are not in  
any child pack and the pack endpoints cannot return them. They *can* be enumerated from the site's  
sitemap (§3.5), which is what `--ungrouped` does; the pack counter makes the miss detectable  
(`Collection::hidden` in `src/api.rs`).

### 3.3 `GET /api/v1/puzzles/{puzzleid}` ✅ Verified

Complete data for a single level. Additional fields beyond §3.2:

| Field                                        | Type    | Description                                                                             |
| -------------------------------------------- | ------- | --------------------------------------------------------------------------------------- |
| `design`                                     | string  | **The board itself** (encoding in §4), two formats depending on `puzzletype`            |
| `designhash`                                 | string  | Content hash (= og:image thumbnail path segment)                                        |
| `description` / `creditsname` / `creditsurl` | string? | null (empty in all observations)                                                        |
| `author`                                     | object  | Here includes `userid` (40644) + `displayname` + `slug`                                 |
| `pack`                                       | object  | Includes `parent` and `ancestors`, full hierarchy                                       |
| `numplay`                                    | int     | Play count (27…, ≥ numsolve)                                                            |
| `numfavorite`                                | int     | 0                                                                                       |
| `listed`                                     | bool    | true                                                                                    |
| `position` / `packtotal` / `perpage`         | int     | 1 / 459 / 30 (server-side redundancy)                                                   |
| `prev` / `next`                              | object? | Adjacent levels within the pack `{puzzleid,name,path}`; `prev=null` for the first level |
| `lock` / `recommended`                       | object? | null (empty in all observations)                                                        |
| `bests`                                      | int[3]  | Same as in the list endpoint                                                            |

### 3.4 Endpoints Observed but Not Individually Verified (from the frontend source)

```
GET  /api/v1/auth/session                    Login state (called for guests too)
GET  /api/v1/announcements/active            Active site announcements
GET  /api/v1/puzzles/{id}/solutions?after=…  Solution feed (cursor pagination)
GET  /api/v1/puzzles/{id}/ranks/{steps}/players?cursor=&limit=&before=
GET  /api/v1/tournaments/{id}                Tournament
GET  /api/v1/timetrial                       Weekly time-trial leaderboard
GET  /api/v1/me/*                            Requires login (friends, drafts, favorites…)
POST /api/v1/puzzles/{id}/attempts           Requires login (start attempt, issues attempttoken)
POST /api/v1/puzzles/{id}/solves             Requires login (submit solution + anti-cheat telemetry)
```

### 3.5 Sitemaps ✅ Verified 2026-10-28

`robots.txt` disallows `/api/` but advertises `Sitemap: https://www.sokobanonline.com/sitemap.xml`,  
so the sitemaps — not the API — are the crawl path the site itself sanctions.

| URL                             | Contents                                                                      |
| ------------------------------- | ----------------------------------------------------------------------------- |
| `/sitemap.xml`                  | `<sitemapindex>`: `pages.xml`, `packs.xml`, `puzzles-1.xml` … `puzzles-3.xml` |
| `/sitemaps/puzzles-{1,2,3}.xml` | 45 000 / 45 000 / 41 424 `<loc>` entries — 131 424 total, ~16 MB combined     |
| `/sitemaps/packs.xml`           | Pack pages (not needed here)                                                  |

Puzzle URLs come in three shapes:

```
/play/community/{author}/{id}_{slug}              <- directly on the author page (no sub-pack)
/play/community/{author}/{pack}/{id}_{slug}       <- inside a sub-pack
/play/web-archive/{author}/{pack}/{id}_{slug}     <- imported web archives
```

The number of segments after the author slug is what identifies an ungrouped level: exactly one means  
the level hangs off the author's root container pack, i.e. it is one of the levels §3.2.1 describes.  
Matching `<loc>` against `play/{collection}/` and keeping single-segment remainders reproduces the  
hidden set exactly (2026-10-28: obyzz011 4 of 4, t1024 4 of 4 — both equal to  
`numpuzzles − Σ sub-packs`).

Cost: one request for the index plus one per shard; `sokofetch --ungrouped` pays it once per run.

## 4. `design` Board Encoding Specification

The encoding logic was aligned line-by-line with the parse/serialize functions of the site's bundled  
level editor (`/assets/builder-Ch-_DB4f.js`):

```js
// Site parser (excerpt, modern branch)
if (row.length % 4) throw "row is not a whole number of cells";
tile = row.slice(i*4, i*4+2);      // terrain layer
obj  = row.slice(i*4+2, i*4+4);    // object layer; "__" = none
```

### 4.1 classic type (1 char per cell)

Standard Sokoban text format, i.e. the XSB alphabet: `#` wall, `_`/space floor, `$` box, `.` goal,  
`@` player, `*` box on goal, `+` player on goal. Line width = `width`.

### 4.2 modern type (4 chars per cell = 2 terrain chars + 2 object chars)

Each line is exactly `4 × width` characters long, with `height` lines in total.

Terrain layer (tile) token table (with frequencies measured across the 459 levels):

| token                    | Meaning                                        | Frequency               |
| ------------------------ | ---------------------------------------------- | ----------------------- |
| `__`                     | Plain floor                                    | 128824                  |
| `##`                     | Wall                                           | 32382                   |
| `--` `\|\|`              | Rail tracks (horizontal/vertical)              | 5682 / 4333             |
| `v>` `<v` `<^` `^>`      | Rail corners                                   | 634–723                 |
| `-^` `-v` `<\|` `\|>`    | Rail T-junctions                               | 144–165                 |
| `~~`                     | Ice (sliding)                                  | 3528                    |
| `00`                     | Breakable floor                                | 1275                    |
| `OO`                     | Hole                                           | 1763                    |
| `.H` `.C` `.D` `.S` `.A` | Suit goals (hearts/clubs/diamonds/spades/star) | 2044/1621/1264/1017/712 |
| `c^` `c>` `cv` `c<`      | Magnet (four orientations)                     | 98–143                  |
| `dH…dA`                  | Suit door (closed)                             | 90–192                  |
| `eH…eA`                  | Suit door (open)                               | 6–20                    |
| `tR` `tG` `tB` `tP` `tT` | Portal (red/green/blue/purple/cyan)            | 52–222                  |

Object layer (obj) token table:

| token            | Meaning                                                           | Frequency                                                                 |
| ---------------- | ----------------------------------------------------------------- | ------------------------------------------------------------------------- |
| `__`             | None                                                              | (the vast majority)                                                       |
| `$X`             | Wildcard box (may enter any goal; also serves as decoy boxes)     | 5036                                                                      |
| `$H/$C/$D/$S/$A` | Suit box (must enter the matching suit goal)                      | Exactly equal to the frequency of the corresponding goal token            |
| `@1` / `@2`      | Player 1 / Player 2                                               | 392 (exactly one @1 in every modern level) / 45 (two-player co-op levels) |
| `bH…bA`          | Suit button (controls the door of the same suit)                  | 92–226                                                                    |
| `p^`             | Magnetic switch (must be embedded in a wall; at most 1 per level) | 31                                                                        |

### 4.3 modern → XSB Degradation Rules

```
terrain ##       → '#'        goal + player   → '+'
box $* on goal   → '*'        player @1/@2    → '@'
box $*           → '$'        everything else → ' ' (floor)
suit goal .X     → '.'
```

Suit boxes/goals degrade to ordinary boxes/goals (**losing the "must match suit" constraint**);  
the wildcard box `$X` is semantically an ordinary box, so no loss there.  
Levels containing mechanisms such as rails, ice, holes, doors, magnets, or portals cannot be  
expressed in XSB — any occurrence excludes the entire level (in this pack, 31 levels containing  
the `p^` magnetic switch were excluded; some levels additionally combine ice/rails/doors and were  
excluded in the same pass).

> ⚠️ **Correction from real-world testing (2026-10-23)**: mapping all tile-layer mechanisms  
> (rails/ice/holes/doors/portals/magnets) to plain floor per the table above **silently changes  
> how a level plays** — e.g. zigzag06's only mechanism is ice `~~`, and after degradation it  
> becomes an ordinary level without ice, which no longer matches the original; moreover, such  
> levels cannot be detected from obj-layer tokens alone (buttons `b*`, magnetic switch `p^`).  
> Therefore modern levels **should not be lossily degraded**; the entire level should be excluded  
> instead (the Rust CLI in this repo does not degrade; it skips all modern levels).

## 5. Data Profile (author soko7734, measured across 459 levels)

- **Type distribution**: modern 392 (85.4%) / classic 67 (14.6%)
- **Sizes**: 8×6 to 43×42 (area 64–1806 cells), median 15×15
- **Difficulty & popularity**: numsolve 2–74; numlike -3…8 (can be negative)
- **Publication dates**: 2023-08-18 → 2026-10-20, spanning 3 years, no nulls
- **Naming**: serial naming (catchy01…, zigzag17…), with duplicates across pages (e.g. class01–class15  
  and a later republished series)
- **Mechanism preferences** (total token counts in modern levels): rails 11k+, ice 3528, holes 1763,  
  breakable floors 1275, doors/buttons 1800+, portals 710, magnets 472 + magnetic switches 31 —  
  this author favors mechanism-driven gameplay
- **Box/goal pairing**: suit boxes and suit goals match 1:1 exactly; the `$X` wildcard boxes (5036)  
  are extra decoy boxes, so 227 levels have more boxes than goals ("fill goals with surplus boxes" gameplay)
- **Consistency checks**: puzzleid globally unique; position 1..459 with no duplicates; all modern  
  line lengths = 4×width; all classic line lengths = width; no guestlocked levels
- **XSB-convertible**: two-stage filter — ① representable (no rails/ice/holes/doors/magnets etc.) →  
  428/459 (67 native classic + 361 degraded modern); ② then validated against the XSB loader's hard  
  constraints (exactly 1 player, box count == goal count) → **206 levels** ultimately entered  
  `traditional_levels.txt`.  
  Among the excluded modern levels: 31 contain magnetic switches, 45 are two-player co-op (`@1`+`@2`),  
  and 177 have more boxes than goals (`$X` decoy boxes, see above).
  > ⚠️ The 428/206 figures above are under the lossy-degradation-allowed criterion (tile-layer  
  > mechanisms silently degraded; see the §4.3 correction). Historically the Rust CLI did degrade  
  > modern levels; after strictly excluding mechanisms it produced 98/459 (343 with mechanisms +  
  > 17 decoy-box + 1 two-player). The current criterion **keeps only classic**: **67/459**  
  > (all classic levels pass soukoban validation, 0 rejections); all 392 modern levels are skipped.

## 6. Tools

Rust CLI (`cargo run -- fetch <slug>`): a single command does the scraping + filtering and directly  
outputs XSB-format `<slug>.txt` (progress bar + resumable after Ctrl-C interruption); each level =  
map + trailing `Title:`/`Author:`/`URL:` metadata lines; supports membercontainer child packs;  
**keeps only classic levels, skips all modern ones**.

`cargo run -- info <slug>` prints the same structure without fetching levels: the sub-packs with  
their counts, plus how many levels belong to none of them (§3.2.1). `--ungrouped` extends either  
subcommand to those levels, recovering their ids from the sitemap (§3.5); `fetch --only-ungrouped`
fetches *only* them and skips the sub-packs (one listing request instead of one per sub-pack).

## 7. Compliance

robots.txt: `Disallow: /api/`, `Crawl-delay: 5`. Scraping is one-time, low-frequency, and for  
personal use only; level copyrights belong to their respective authors — do not redistribute.

The sitemap route (§3.5) is the path `robots.txt` explicitly advertises, but it is still ~16 MB of  
XML for 3 requests: run it once, not in a loop.
