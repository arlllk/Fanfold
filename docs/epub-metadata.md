# EPUB metadata written by Fanfold

After every download, Fanfold adds what it knows about the work to the book's package
document (`content.opf`). This page describes those additions. EPUB 3 books declare the
`ao3:` prefix as `https://github.com/arlllk/Fanfold/blob/main/docs/epub-metadata.md#`.

## Tags (`dc:subject`)

The downloader's subject list is replaced with labelled tags, so a reader's tag list groups
them by kind:

| Subject | From |
|---|---|
| `Rating: Explicit` | the work's rating |
| `Warning: …` | archive warnings |
| `Category: F/M` | categories |
| `Fandom: …` | fandoms |
| `Relationship: …` | relationships |
| `Character: …` | characters |
| `Tag: …` | additional (freeform) tags |
| `Status: Complete` or `Status: In progress` | every planned chapter posted, or not |

## Series

- `<meta name="calibre:series" content="…"/>` and `calibre:series_index`: one series, the
  followed series the library files the book under, otherwise the first the work's page lists.
- EPUB 3 only: every series as `belongs-to-collection`, refined by `collection-type` (`series`),
  `group-position` (the part number) and `dcterms:identifier` (the series URL).

## `ao3:` properties

In EPUB 3 as `<meta property="ao3:words">120000</meta>`; in EPUB 2 as
`<meta name="ao3:words" content="120000"/>`. Properties marked *repeated* appear once per value.

| Property | Value |
|---|---|
| `ao3:work-id` | AO3's work number |
| `ao3:rating` | e.g. `Teen And Up Audiences` |
| `ao3:warning`, `ao3:category`, `ao3:fandom`, `ao3:relationship`, `ao3:character`, `ao3:freeform` | one tag each (*repeated*) |
| `ao3:author` | `pseud (user)`, or `user` when they are the same (*repeated*) |
| `ao3:status` | `complete` or `in-progress` |
| `ao3:chapters-posted` | chapters posted |
| `ao3:chapters-total` | planned chapters; absent when AO3 shows `?` |
| `ao3:words` | word count |
| `ao3:published`, `ao3:updated` | dates, `YYYY-MM-DD` |
| `ao3:restricted` | `true` when only logged-in users can read the work |
| `ao3:kudos`, `ao3:hits`, `ao3:bookmarks`, `ao3:comments` | counters at the download |
| `ao3:stats-date` | the day of that download |
| `ao3:first-downloaded` | the day Fanfold first saw the work |

## Pages and cover

- `OEBPS/fanfold_cover.png` (1000 × 1500), the cover (`<meta name="cover">`, and in EPUB 3
  `properties="cover-image"`), shown by `OEBPS/fanfold_cover.xhtml`, the first page.
- Nothing after the story, so an update never moves the reader's place. Books finished
  before finishing version 2 had `OEBPS/fanfold_about.xhtml` ("About this work") there, and
  FanFicFare's update log (`log_page`); finishing them again removes both, with their
  table-of-contents entries.

## Updates

The downloader rebuilds a book when it adds chapters and keeps none of this, so Fanfold adds it
again after every download. Everything Fanfold writes is recognisable (a `fanfold-` id, an
`ao3:` property, a `fanfold_` file name, marker comments in the tables of contents) and is
replaced, not duplicated. `<meta name="fanfold:finish" content="N"/>` records which version
of these additions a book has; a book with an older one (or none) gets the current additions
at its next check, even when the work has no new chapters.
