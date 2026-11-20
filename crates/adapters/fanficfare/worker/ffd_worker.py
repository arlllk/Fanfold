"""One-shot FanFicFare worker.

Protocol (one story or series per process):
  stdin : a JSON object  {"op", "url", "existing", "workdir", "personal_ini",
                          "cookies", "user_agent", "clearance"}   (the last three are optional)
          `op` is "download" (the default) or "list_series".
          `cookies` is a Netscape cookies.txt that FanFicFare loads, and the worker writes
          it back (cookies rotate during a run) so the caller can keep them.
          `clearance` {"flaresolverr", "timeout_ms", "url"} asks the worker to have FlareSolverr
          pass Cloudflare once first (see apply_clearance); FanFicFare then fetches by itself.
  stdout: FanFicFare's own chatter is captured; the LAST line printed is a JSON
          object, either
            {"status": "ok", "path", "title", "changed", "metadata", "log"}
          or, for "list_series",
            {"status": "listed", "name", "summary_html", "complete", "works", "raw", "log"}
          or
            {"status": "error", "kind", "message", "log"}
          A download's "metadata" (see work_metadata) is what the site said about the work,
          read from FanFicFare's own story object, with lists kept as lists.
          `kind` is one of KINDS (fanfold-domain's FailureKind).

The Rust side never parses FanFicFare's text; this script is the only place that
knows how FanFicFare reports problems.

Every page FanFicFare fetches passes the page guard (install_page_guard) first: an error
page that arrives looking like a successful response (FlareSolverr reports HTTP 200 for
whatever its browser ended up showing) is turned into the HTTP error it stands for, before
FanFicFare can parse it as the work, the chapter list or the series it asked for.

Runs as `python -I -c <this file>`, so it is one file that imports only the standard library
and FanFicFare. The pure parts are unit tested in test_ffd_worker.py.
"""

from __future__ import annotations

import contextlib
import dataclasses
import datetime
import hashlib
import html
import io
import json
import logging
import os
import re
import sys
import time
import traceback
import urllib.parse
import xml.etree.ElementTree as ET
import zipfile
from collections.abc import Callable
from typing import TYPE_CHECKING, cast

if TYPE_CHECKING:  # FanFicFare's own classes, so Pyrefly checks every use of them
    from optparse import Values

    from fanficfare.adapters.base_adapter import BaseSiteAdapter

LOG_TAIL_CHARS = 20000

# ----------------------------------------------------------------------------- failure kinds

SITE_UNAVAILABLE = "site_unavailable"
RATE_LIMITED = "rate_limited"
BLOCKED = "blocked"
AUTH_FAILED = "auth_failed"
NOT_FOUND = "not_found"
TRANSIENT = "transient"
PERMANENT = "permanent"
KINDS = (SITE_UNAVAILABLE, RATE_LIMITED, BLOCKED, AUTH_FAILED, NOT_FOUND, TRANSIENT, PERMANENT)


class WorkerError(Exception):
    """A failure already classified: becomes {"status": "error", "kind", "message"}."""

    def __init__(self, kind: str, message: str) -> None:
        assert kind in KINDS, kind
        super().__init__(message)
        self.kind = kind
        self.message = message


EXCEPTION_KINDS = {
    "StoryDoesNotExist": NOT_FOUND,
    "FailedToLogin": AUTH_FAILED,
    "NeedTimedOneTimePassword": AUTH_FAILED,
    "AccessDenied": AUTH_FAILED,
    "AdultCheckRequired": PERMANENT,
    "InvalidStoryURL": PERMANENT,
    "UnknownSite": PERMANENT,
    "PersonalIniFailed": PERMANENT,
    "RegularExpresssionFailed": PERMANENT,  # sic: FanFicFare's spelling
    "ConflictingOptions": PERMANENT,
}

# FanFicFare reports many problems by printing and returning; match on its text. The first
# rules are the page guard's own messages, which FanFicFare may pass on as text.
TEXT_RULES = (
    ("bot protection", RATE_LIMITED),
    ("browser check", BLOCKED),
    ("not an ao3 page", SITE_UNAVAILABLE),
    ("error code: 1010", BLOCKED),
    ("retry later", RATE_LIMITED),
    ("too many requests", RATE_LIMITED),
    ("login failed", AUTH_FAILED),
    ("only available to registered users", AUTH_FAILED),
    # FanFicFare's login could not even start (no usable token): only another way in helps.
    ("failed getting login token", AUTH_FAILED),
    ("adult check required", PERMANENT),
    ("does not exist", NOT_FOUND),
    ("downtime", SITE_UNAVAILABLE),
    ("maintenance", SITE_UNAVAILABLE),
    ("timed out", SITE_UNAVAILABLE),
    ("connection", SITE_UNAVAILABLE),
)

# Lines FanFicFare's CLI prints before giving up without raising or calling its failure callback.
FATAL_PRINTS = ("Login Failed on non-interactive process",)


def classify_text(text: str) -> str | None:
    lowered = text.lower()
    for needle, kind in TEXT_RULES:
        if needle in lowered:
            return kind
    return None


def classify_status(status: int) -> str:
    if status == 429:
        return RATE_LIMITED
    if status == 403:
        return BLOCKED
    if status in (404, 410):
        return NOT_FOUND
    if status >= 500:  # includes Cloudflare 520-527
        return SITE_UNAVAILABLE
    return TRANSIENT


def chained_status(exc: BaseException) -> int | None:
    """The HTTP status of the error that caused `exc`, following __cause__/__context__.

    FanFicFare often wraps a failed request in a generic error ("Failed getting login token
    from token_dispenser.json" around a 525): the wrapped status says what really happened.
    """
    seen = set()
    current: BaseException | None = exc
    while current is not None and id(current) not in seen:
        seen.add(id(current))
        status = getattr(current, "status_code", None)
        if isinstance(status, int):
            return status
        current = current.__cause__ or current.__context__
    return None


def classify_exception(exc: BaseException) -> str:
    status = getattr(exc, "status_code", None)
    if isinstance(status, int):
        return classify_status(status)
    name = type(exc).__name__
    if name in EXCEPTION_KINDS:
        return EXCEPTION_KINDS[name]
    status = chained_status(exc)
    if status is not None:
        return classify_status(status)
    lowered = name.lower()
    if "timeout" in lowered or "connection" in lowered:
        return SITE_UNAVAILABLE
    return classify_text(str(exc)) or TRANSIENT


# ------------------------------------------------------------------------------ page guard

# Hosts FanFicFare's AO3 adapter accepts (adapter_archiveofourownorg.getAcceptDomains).
AO3_DOMAINS = (
    "archiveofourown.org",
    "archiveofourown.com",
    "archiveofourown.net",
    "archiveofourown.gay",
    "ao3.org",
)
# Every page AO3 itself renders (works, chapters, series, login, its own error pages) has
# its layout's main region; pages from anything in front of it (Cloudflare, AO3's bot
# filter, a proxy) do not.
AO3_LAYOUT = re.compile(r'<div\b[^>]*\bid="main"')
# Error pages are short and the layout marker is near the top: never scan a whole book.
INSPECT_CHARS = 64 * 1024
SNIPPET_CHARS = 200

# Cloudflare's own error page (5xx origin errors, 1xxx blocks).
CLOUDFLARE_ERROR_PAGE = re.compile(r'id="cf-error-details"')
CLOUDFLARE_ERROR_CODE = re.compile(r"Error code:?\s*(\d{3,4})", re.IGNORECASE)

# Cloudflare could not get an answer from the origin (AO3): 520-527. These are retried at
# once, a few times: during AO3's partial 525 outages roughly half of all fresh connections
# fail and a retry is a fresh coin toss, while the request never reached AO3, so retrying is
# safe even for a login form. Nothing else is retried here: a block or a rate limit only
# gets worse for asking again at once; the job queue backs off instead.
CLOUDFLARE_ORIGIN_ERRORS = range(520, 528)
CLOUDFLARE_RETRIES = 3
CLOUDFLARE_RETRY_DELAY_S = 1.0


def cloudflare_error(text: object) -> int | None:
    """The error code if `text` is a Cloudflare error page, else None."""
    if not isinstance(text, str) or not CLOUDFLARE_ERROR_PAGE.search(text):
        return None
    match = CLOUDFLARE_ERROR_CODE.search(text)
    return int(match.group(1)) if match else 520


def status_for_cloudflare(code: int) -> int:
    """The HTTP status that classifies a Cloudflare error code like the real response would."""
    if code < 1000:
        return code  # 520-530: the origin (AO3) is unreachable
    return 429 if code == 1015 else 403  # 1015 is Cloudflare's rate limit, other 1xxx are blocks


def visible_text(page: str) -> str:
    """The words a reader would see on `page`, whitespace collapsed."""
    page = re.sub(r"(?is)<(script|style)\b.*?</\1\s*>", " ", page)
    page = re.sub(r"(?s)<!--.*?-->", " ", page)
    page = re.sub(r"(?s)<[^>]*>", " ", page)
    return " ".join(html.unescape(page).split())


def snippet(page: str) -> str:
    words = visible_text(page)
    return words if len(words) <= SNIPPET_CHARS else words[: SNIPPET_CHARS - 1] + "…"


@dataclasses.dataclass(frozen=True)
class KnownPage:
    """A page something in front of AO3 sends instead of the one asked for."""

    name: str
    status: int  # the HTTP status it stands for, which classifies it
    description: str  # starts the error message; TEXT_RULES must classify it the same way
    matches: Callable[[str, str], bool]  # (raw page, visible text)


KNOWN_PAGES = (
    # AO3's bot filter, in front of the Archive since the autumn 2026 incident. It answers
    # 429 with a few lines of plain text; FlareSolverr turns that into a 200.
    KnownPage(
        "ao3_bot_filter",
        429,
        "AO3's bot protection refused the request (it asks to try again later)",
        lambda raw, words: re.search(
            r"systems think you may be a robot|bot servers are currently overloaded", words, re.I
        )
        is not None,
    ),
    # AO3's own rate limit: a bare "Retry later".
    KnownPage(
        "ao3_retry_later",
        429,
        "AO3 asked to retry later (its rate limit)",
        lambda raw, words: len(words) < 200 and words.lower().startswith("retry later"),
    ),
    # Cloudflare's interstitial: the browser check was not passed.
    KnownPage(
        "cloudflare_challenge",
        403,
        "Cloudflare's browser check was not passed",
        lambda raw, words: re.search(
            r"<title>\s*Just a moment\.\.\.\s*</title>|/cdn-cgi/challenge-platform/", raw, re.I
        )
        is not None,
    ),
)


@dataclasses.dataclass(frozen=True)
class Rejection:
    """Why a fetched page is not the page asked for."""

    status: int
    message: str
    retry_now: bool = False


def is_ao3_page(url: str) -> bool:
    """Whether `url` is an AO3 page (not an API call or a file)."""
    parts = urllib.parse.urlsplit(url or "")
    host = (parts.hostname or "").lower()
    on_ao3 = any(host == d or host.endswith("." + d) for d in AO3_DOMAINS)
    return on_ao3 and parts.scheme in ("http", "https") and not parts.path.endswith(".json")


def page_text(content: object) -> str | None:
    """The start of a response as text; None for anything that is not a page."""
    if isinstance(content, (bytes, bytearray)):
        head = bytes(content[:INSPECT_CHARS])
        if b"\x00" in head:
            return None  # binary: an image
        text = head.decode("utf-8", errors="replace")
    elif isinstance(content, str):
        text = content[:INSPECT_CHARS]
    else:
        return None
    if text.lstrip().startswith(("{", "[")):
        return None  # JSON (the login token, FlareSolverr's own answers)
    return text


def judge(url: str, content: object) -> Rejection | None:
    """None if `content` may be the page at `url`, else why it is not."""
    text = page_text(content)
    if text is None or AO3_LAYOUT.search(text):
        return None
    code = cloudflare_error(text)
    if code is not None:
        return Rejection(
            status_for_cloudflare(code),
            "Cloudflare error code: %d instead of %s" % (code, url),
            retry_now=code in CLOUDFLARE_ORIGIN_ERRORS,
        )
    words = visible_text(text)
    for page in KNOWN_PAGES:
        if page.matches(text, words):
            return Rejection(page.status, "%s, instead of %s: “%s”" % (page.description, url, snippet(text)))
    if is_ao3_page(url):
        # Something answered for AO3 that is none of the above: an error page, a proxy, a
        # maintenance notice. Never let FanFicFare parse it as the work.
        return Rejection(503, "the site sent something that is not an AO3 page instead of %s: “%s”" % (url, snippet(text)))
    return None


class PageLog:
    """What the page guard saw: how many pages, and the last AO3 page (to name it on a crash)."""

    def __init__(self) -> None:
        self.judged = 0
        self.url: str | None = None
        self.text = ""

    def record(self, url: str, content: object) -> None:
        self.judged += 1
        text = page_text(content)
        if text is not None and is_ao3_page(url):
            self.url, self.text = url, text


PAGES = PageLog()

# Set on the functions this worker puts into FanFicFare, so installing twice is harmless.
HOOK_MARK = "_fanfold_hook"


def incompatible(what: str) -> WorkerError:
    """This FanFicFare is not the one the worker was written against: no download can be trusted."""
    import fanficfare

    version = getattr(fanficfare, "__version__", None) or "of this image"
    return WorkerError(
        PERMANENT,
        "this worker does not fit FanFicFare %s: %s. Update ffd_worker.py for it." % (version, what),
    )


def subclasses(cls: type) -> list[type]:
    found = []
    for sub in cls.__subclasses__():
        found += [sub, *subclasses(sub)]
    return found


def overriding_fetchers(base: type) -> list[str]:
    """The loaded fetcher classes that would slip past a hook on `base.do_request`."""
    return sorted(c.__name__ for c in subclasses(base) if "do_request" in vars(c))


def check_guard_saw_pages(url: str, pages: PageLog) -> None:
    """Every AO3 download fetches the work page, so the guard must have seen at least one."""
    if is_ao3_page(url) and pages.judged == 0:
        raise incompatible("its pages no longer go through Fetcher.do_request, which the page guard hooks")


def install_page_guard() -> None:
    """Check every page FanFicFare fetches, through any fetcher, before it is parsed.

    Hooks the one method all of FanFicFare's fetchers share (Fetcher.do_request), below its
    cache, so a rejected page is never cached either. Cloudflare's 52x pages are retried at
    once (FanFicFare's plain-HTTP fetcher has retry_http_525_failures; this gives every path
    the same); everything else becomes FanFicFare's own HTTPErrorFFF with the status the page
    stands for, which classify_exception turns into the right kind.

    Why: FlareSolverr reports HTTP 200 for whatever its browser shows, so Cloudflare's "525"
    once became a book with only a title page, and AO3's bot filter became
    "AttributeError: 'NoneType' object has no attribute 'get_text'".

    A fetcher that overrides do_request would slip past the hook, so that is refused here;
    download() also refuses an AO3 download the guard saw no page of.
    """
    from fanficfare import exceptions
    from fanficfare.fetchers import base_fetcher, fetcher_flaresolverr_proxy, fetcher_requests

    fetcher = base_fetcher.Fetcher
    original = fetcher.do_request
    if getattr(original, HOOK_MARK, False):
        return
    # Fanfold's two fetchers are imported above; any other that is loaded is checked too.
    assert issubclass(fetcher_flaresolverr_proxy.FlareSolverr_ProxyFetcher, fetcher_requests.RequestsFetcher)
    overriding = overriding_fetchers(fetcher)
    if overriding:
        raise incompatible("%s override Fetcher.do_request, which the page guard hooks" % ", ".join(overriding))

    def do_request(
        self: base_fetcher.Fetcher,
        method: str,
        url: str,
        parameters: object = None,
        referer: str | None = None,
        usecache: bool = True,
        image: bool = False,
    ) -> base_fetcher.FetcherResponse:
        for attempt in range(CLOUDFLARE_RETRIES + 1):
            # cast: Pyrefly infers FanFicFare's do_request as never returning (the base class's
            # request() only raises), which would leave the rest of this loop unchecked.
            response = cast(
                "base_fetcher.FetcherResponse",
                original(self, method, url, parameters=parameters, referer=referer, usecache=usecache, image=image),
            )
            if image:
                return response
            content = response.content
            rejection = judge(url, content)
            if rejection is None:
                PAGES.record(url, content)
                return response
            if not rejection.retry_now or attempt == CLOUDFLARE_RETRIES:
                raise exceptions.HTTPErrorFFF(url, rejection.status, rejection.message, None)
            delay = CLOUDFLARE_RETRY_DELAY_S * (attempt + 1)
            print("%s; retrying in %.0f s (%d of %d)" % (rejection.message, delay, attempt + 1, CLOUDFLARE_RETRIES))
            time.sleep(delay)
        raise AssertionError("unreachable")

    setattr(do_request, HOOK_MARK, True)
    # setattr: the same inference makes a plain assignment a type error.
    setattr(fetcher, "do_request", do_request)


PARSE_ERRORS = (AttributeError, TypeError, KeyError, IndexError)
# FanFicFare's site adapters, wherever the venv is (site-packages, or Debian's dist-packages).
ADAPTER_DIR = re.compile(r"/(site|dist)-packages/fanficfare/adapters/")


def raised_while_parsing(exc: BaseException) -> bool:
    """Whether `exc` is FanFicFare's site adapter tripping over a page it did not expect."""
    if not isinstance(exc, PARSE_ERRORS):
        return False
    frames = traceback.extract_tb(exc.__traceback__)
    return any(ADAPTER_DIR.search(f.filename.replace(os.sep, "/")) for f in frames)


def unreadable_page(exc: BaseException, pages: PageLog = PAGES) -> WorkerError | None:
    """A clear failure for FanFicFare crashing on a page that passed the guard.

    The guard rejects what is not an AO3 page, so this is an AO3 page FanFicFare cannot read:
    most likely AO3 changed its markup and FanFicFare needs an update. Retried like any
    unclassified failure, with the page named so the job log says what happened.
    """
    if not raised_while_parsing(exc):
        return None
    if pages.url is None:
        where = "a page"
    else:
        where = "%s (it began: “%s”)" % (pages.url, snippet(pages.text))
    return WorkerError(
        TRANSIENT,
        "FanFicFare could not read %s: %s: %s. If this keeps happening, AO3 may have changed "
        "its pages and FanFicFare needs an update." % (where, type(exc).__name__, exc),
    )


# -------------------------------------------------------------------------------- request


@dataclasses.dataclass(frozen=True)
class Clearance:
    flaresolverr: str
    url: str
    timeout_ms: int


@dataclasses.dataclass(frozen=True)
class Request:
    """What the Rust side asks for (protocol.rs, WorkerRequest), checked field by field."""

    op: str
    url: str
    workdir: str
    existing: str | None = None
    personal_ini: str | None = None
    cookies: str | None = None
    user_agent: str | None = None
    clearance: Clearance | None = None


OPS = ("download", "list_series")


def parse_request(raw: object) -> Request:
    """A Request from the decoded JSON, or WorkerError(PERMANENT) saying what is wrong."""

    def bad(why: str) -> WorkerError:
        return WorkerError(PERMANENT, "bad worker request: %s" % why)

    if not isinstance(raw, dict):
        raise bad("not a JSON object")

    def string(obj: dict[str, object], key: str, required: bool = False) -> str | None:
        value = obj.get(key)
        if value is None and not required:
            return None
        if not isinstance(value, str) or not value:
            raise bad("%r must be a non-empty string" % key)
        return value

    op = raw.get("op", "download")
    if op not in OPS:
        raise bad("unknown op %r" % (op,))
    clearance = None
    spec = raw.get("clearance")
    if spec is not None:
        if not isinstance(spec, dict):
            raise bad("'clearance' must be an object")
        timeout_ms = spec.get("timeout_ms", 60000)
        if not isinstance(timeout_ms, int) or isinstance(timeout_ms, bool) or timeout_ms <= 0:
            raise bad("'clearance.timeout_ms' must be a positive integer")
        clearance = Clearance(
            flaresolverr=str(string(spec, "flaresolverr", required=True)),
            url=str(string(spec, "url", required=True)),
            timeout_ms=timeout_ms,
        )
    return Request(
        op=str(op),
        url=str(string(raw, "url", required=True)),
        workdir=str(string(raw, "workdir", required=True)),
        existing=string(raw, "existing"),
        personal_ini=string(raw, "personal_ini"),
        cookies=string(raw, "cookies"),
        user_agent=string(raw, "user_agent"),
        clearance=clearance,
    )


def read_request() -> Request:
    try:
        raw = json.load(sys.stdin)
    except ValueError as exc:
        raise WorkerError(PERMANENT, "bad worker request: %s" % exc) from exc
    return parse_request(raw)


# ------------------------------------------------------------------------- work metadata

# FanFicFare metadata list -> tag kind, in AO3's order.
TAG_LISTS = (
    ("fandoms", "fandom"),
    ("ao3categories", "category"),
    ("warnings", "warning"),
    ("ships", "relationship"),
    ("characters", "character"),
    ("freeformtags", "freeform"),
)
SERIES_SLOTS = 10
AO3_ID = re.compile(r"/(works|series|chapters|users|pseuds)/([^/?#]+)")


def ids_in(url: str | None) -> dict[str, str]:
    """{"works": "123", "users": "name", ...} from an AO3 URL."""
    return {kind: value for kind, value in AO3_ID.findall(url or "")}


def to_int(value: object) -> int | None:
    """AO3 numbers come as text with thousands separators ("120,000"); None if absent."""
    if isinstance(value, bool):
        return None
    if isinstance(value, int):
        return value
    digits = re.sub(r"[^0-9]", "", str(value or ""))
    return int(digits) if digits else None


def to_date(value: object) -> str | None:
    if isinstance(value, (datetime.datetime, datetime.date)):
        return value.strftime("%Y-%m-%d")
    match = re.match(r"(\d{4}-\d{2}-\d{2})", str(value or ""))
    return match.group(1) if match else None


def json_safe(value: object) -> object:
    return json.loads(json.dumps(value, default=str))


def work_metadata(adapter: BaseSiteAdapter) -> dict[str, object]:
    """What FanFicFare learned about the work, in the shape the Rust side records."""
    story = adapter.story

    def text(key: str) -> str:
        return html.unescape(str(story.getMetadata(key, removeallentities=False, doreplacements=False) or ""))

    def listed(key: str) -> list[str]:
        return [html.unescape(str(v)) for v in story.getList(key, doreplacements=False) or [] if str(v).strip()]

    authors = []
    names, urls = listed("author"), listed("authorUrl")
    for i, name in enumerate(names):
        found = ids_in(urls[i] if i < len(urls) else "")
        authors.append({"user": found.get("users", name), "pseud": found.get("pseuds", name)})

    tags = [{"kind": kind, "name": name} for key, kind in TAG_LISTS for name in listed(key)]

    series = []
    for slot in range(SERIES_SLOTS):
        label = text("series%02d" % slot)
        series_id = ids_in(text("series%02dUrl" % slot)).get("series")
        if not label or not series_id or not series_id.isdigit():
            continue
        name, part = label, None
        match = re.match(r"^(.*) \[(\d+)\]$", label)
        if match:
            name, part = match.group(1), int(match.group(2))
        series.append({"id": int(series_id), "name": name, "part": part})

    chapters = []
    for number, info in enumerate(adapter.get_chapters(), start=1):
        if not isinstance(info, dict):
            continue
        chapter_id = ids_in(str(info.get("url") or "")).get("chapters")
        chapters.append({
            "number": number,
            "title": html.unescape(str(info.get("site_title") or info.get("title") or "")),
            "ao3_id": int(chapter_id) if chapter_id and chapter_id.isdigit() else None,
            "posted_on": to_date(info.get("date")),
        })

    posted, _, total = text("chapterslashtotal").partition("/")
    return {
        "title": text("title"),
        "byline": text("byline") or ", ".join(names),
        "summary_html": str(story.getMetadata("description", removeallentities=False, doreplacements=False) or ""),
        "language": text("language"),
        "rating": text("rating"),
        "restricted": bool(text("restricted")),
        "status": text("status"),
        "published_on": to_date(story.getMetadataRaw("datePublished")),
        "updated_on": to_date(story.getMetadataRaw("dateUpdated")),
        "chapters_posted": to_int(posted) or len(chapters) or to_int(text("numChapters")) or 0,
        "chapters_total": to_int(total),
        "words": to_int(text("numWords")) or 0,
        "authors": authors,
        "tags": tags,
        "series": series,
        "chapters": chapters,
        "stats": {key: to_int(text(key)) for key in ("hits", "kudos", "comments", "bookmarks")},
        "raw": json_safe(story.getAllMetadata(removeallentities=False, doreplacements=False)),
    }


class AdapterCapture:
    """Keeps the adapter FanFicFare's CLI creates, so its metadata can be read afterwards."""

    def __init__(self) -> None:
        self.adapter: BaseSiteAdapter | None = None

    def install(self) -> None:
        from fanficfare import adapters

        original = adapters.getAdapter
        original = getattr(original, HOOK_MARK, None) or original

        def get_adapter(*args: object, **kwargs: object) -> BaseSiteAdapter:
            adapter = original(*args, **kwargs)
            self.adapter = adapter
            return adapter

        setattr(get_adapter, HOOK_MARK, original)  # the unhooked one, for the next install
        adapters.getAdapter = get_adapter


# ---------------------------------------------------------------------------------- books


def chapter_count(path: str) -> int | None:
    """How many chapters FanFicFare finds in an EPUB (None if it cannot read one)."""
    from fanficfare.epubutils import get_dcsource_chaptercount

    try:
        count = get_dcsource_chaptercount(path)[1]
    except Exception:  # noqa: BLE001 - not an EPUB FanFicFare can update
        return None
    return count if isinstance(count, int) else None


def sha256(path: str) -> str:
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 16), b""):
            digest.update(chunk)
    return digest.hexdigest()


def epub_title(path: str) -> str | None:
    """The title in the EPUB's package document; None if it cannot be read."""
    try:
        with zipfile.ZipFile(path) as archive:
            container = ET.fromstring(archive.read("META-INF/container.xml"))
            rootfile = next(e for e in container.iter() if e.tag.endswith("rootfile"))
            opf = ET.fromstring(archive.read(rootfile.attrib["full-path"]))
    except (OSError, KeyError, StopIteration, zipfile.BadZipFile, ET.ParseError):
        return None
    for element in opf.iter():
        if element.tag == "{http://purl.org/dc/elements/1.1/}title":
            return (element.text or "").strip()
    return None


# ------------------------------------------------------------------------------- sessions


def build_options(request: Request, existing: str | None, target: str) -> Values:
    from fanficfare import cli

    argv = ["--non-interactive", "-f", "epub"]
    if request.personal_ini:
        argv += ["-c", request.personal_ini]
    if request.cookies:
        argv += ["--mozilla-cookies", request.cookies]
    if request.user_agent:
        argv += ["-o", "user_agent=" + request.user_agent]
    if existing:
        argv += ["-u"]
    else:
        argv += ["-o", "output_filename=" + os.path.join(request.workdir, "story.epub")]
    argv.append(target)  # validateOptions insists on a positional argument
    parser = cli.mkParser(False)
    options, args = parser.parse_args(argv)
    cli.expandOptions(options)
    cli.validateOptions(parser, options, args)
    return options


def apply_clearance(request: Request, clearance: Clearance, cookies: str) -> Request:
    """Have FlareSolverr pass Cloudflare once; leave its cookies and User-Agent for FanFicFare.

    Used for the login fallback. AO3 refuses a login submitted *through* FlareSolverr (its
    form post carries no session cookie), but plain HTTP works once Cloudflare has been
    passed: the clearance cookie is bound to the browser's User-Agent, so FanFicFare then
    sends exactly that. FlareSolverr's fresh cookies replace the stored ones of the same
    name, except the long-lived login cookie, which only a real login can issue.
    Returns the request with FlareSolverr's User-Agent.
    """
    import http.cookiejar
    import urllib.request

    body = json.dumps({"cmd": "request.get", "url": clearance.url, "maxTimeout": clearance.timeout_ms}).encode()
    http_request = urllib.request.Request(
        clearance.flaresolverr.rstrip("/") + "/v1", body, {"Content-Type": "application/json"}
    )
    try:
        with urllib.request.urlopen(http_request, timeout=clearance.timeout_ms / 1000 + 30) as response:
            answer = json.load(response)
    except Exception as exc:  # noqa: BLE001 - any failure to talk to FlareSolverr
        raise WorkerError(SITE_UNAVAILABLE, "cannot reach FlareSolverr: %s" % exc) from exc
    if not isinstance(answer, dict):
        raise WorkerError(SITE_UNAVAILABLE, "FlareSolverr sent no JSON object")
    solution = answer.get("solution") if answer.get("status") == "ok" else None
    if not isinstance(solution, dict):
        raise WorkerError(SITE_UNAVAILABLE, "FlareSolverr failed: %s" % answer.get("message"))
    # Clearance earned on an error page is no clearance: say what the site really sent.
    rejection = judge(clearance.url, solution.get("response"))
    if rejection is not None:
        raise WorkerError(classify_status(rejection.status), rejection.message)

    earned = []
    for cookie in solution.get("cookies") or []:
        if not isinstance(cookie, dict):
            continue
        name, value, domain = cookie.get("name"), cookie.get("value"), cookie.get("domain")
        if not (isinstance(name, str) and isinstance(value, str) and isinstance(domain, str) and name and domain):
            print("FlareSolverr sent a cookie without a name, value or domain; skipped")
            continue
        expires = cookie.get("expires") or cookie.get("expiry")
        expires = int(expires) if isinstance(expires, (int, float)) and expires > 0 else None
        path = cookie.get("path")
        earned.append(
            http.cookiejar.Cookie(
                0, name, value, None, False, domain, True, domain.startswith("."),
                path if isinstance(path, str) and path else "/", True,
                bool(cookie.get("secure")), expires, expires is None, None, None, {},
            )
        )

    jar = http.cookiejar.MozillaCookieJar(cookies)
    try:
        jar.load(ignore_discard=True, ignore_expires=True)
    except OSError:  # includes http.cookiejar.LoadError
        pass  # nothing stored yet, or nothing usable: start from FlareSolverr's
    fresh = {cookie.name for cookie in earned}
    for cookie in list(jar):
        if cookie.name in fresh and cookie.name != "remember_user_token":
            jar.clear(cookie.domain, cookie.path, cookie.name)
    for cookie in earned:
        jar.set_cookie(cookie)
    jar.save(ignore_discard=True, ignore_expires=True)

    user_agent = solution.get("userAgent")
    if not isinstance(user_agent, str) or not user_agent:
        return request
    # The caller keeps the User-Agent next to the cookies it earned.
    with open(os.path.join(request.workdir, "user_agent.txt"), "w") as handle:
        handle.write(user_agent)
    return dataclasses.replace(request, user_agent=user_agent)


def save_cookies(options: Values, request: Request) -> None:
    """Write the cookie jar back to the file it was loaded from, success or not.

    A run that Cloudflare refused may still have rotated cookies, and the caller decides
    what to keep. Never fails the download.
    """
    jar = getattr(options, "cookiejar", None)
    if not request.cookies or jar is None:
        return
    try:
        jar.save_cookiejar(request.cookies)
    except Exception as exc:  # noqa: BLE001
        print("could not save the rotated cookies: %s" % exc)


# ------------------------------------------------------------------------------ operations


def list_series(request: Request, buffer: io.StringIO) -> dict[str, object]:
    """A series page: its name, summary, status and works in order."""
    from fanficfare import cli
    from fanficfare.geturls import GetURLs

    options = build_options(request, None, request.url)
    try:
        with contextlib.redirect_stdout(buffer), contextlib.redirect_stderr(buffer):
            configuration = cli.get_configuration(request.url, None, None, options)
            page = GetURLs(configuration).get_urls_from_page(request.url)
    finally:
        save_cookies(options, request)
    if not isinstance(page, dict):
        raise incompatible("GetURLs.get_urls_from_page returned %s, not a dict" % type(page).__name__)
    works: list[int] = []
    for url in page.get("urllist") or []:
        work = ids_in(str(url)).get("works")
        if work and work.isdigit() and int(work) not in works:
            works.append(int(work))
    if not works:
        raise WorkerError(
            classify_text(buffer.getvalue()) or SITE_UNAVAILABLE,
            "the series page listed no works (the site probably sent an error page)",
        )
    return {
        "status": "listed",
        "name": html.unescape(str(page.get("name") or "")),
        "summary_html": str(page.get("desc") or ""),
        "complete": {"Completed": True, "In-Progress": False}.get(str(page.get("status"))),
        "works": works,
        "raw": json_safe({key: page[key] for key in page if key != "desc"}),
    }


def run_fanficfare(request: Request, existing: str | None, buffer: io.StringIO) -> tuple[str, list[str]]:
    """Run FanFicFare's download once. Returns (output_path, failures it reported)."""
    from fanficfare import cli

    failures: list[str] = []

    def warn(message: object) -> None:
        print(message)

    def fail(message: object) -> None:
        failures.append(str(message))
        print(message)

    target = existing or request.url
    options = build_options(request, existing, target)
    out_path = existing or os.path.join(request.workdir, "story.epub")
    try:
        with contextlib.redirect_stdout(buffer), contextlib.redirect_stderr(buffer):
            cli.do_download(target, options, None, None, warn, fail)
    finally:
        save_cookies(options, request)
    return out_path, failures


def download(request: Request, buffer: io.StringIO, capture: AdapterCapture) -> dict[str, object]:
    existing = request.existing
    if existing and chapter_count(existing) == 0:
        # A book with no chapters (written before the checks below existed) cannot be
        # updated: FanFicFare refuses it as "no recognizable chapters", forever. Replace it.
        print("the stored book has no chapters: downloading it again", file=buffer)
        existing = None
    before = sha256(existing) if existing else None
    out_path, failures = run_fanficfare(request, existing, buffer)

    # A failed update leaves the old file where it was, so "a file exists" proves nothing:
    # FanFicFare reporting a failure is a failure, whatever is on disk. It reports some of
    # them only by printing a line and returning, with no callback and no exception.
    log = buffer.getvalue()
    printed = [line for line in log.splitlines() if line.startswith(FATAL_PRINTS)]
    if failures or printed:
        message = (failures or printed)[-1]
        raise WorkerError(classify_text(message) or classify_text(log) or TRANSIENT, message)

    if not (os.path.exists(out_path) and os.path.getsize(out_path) > 0):
        message = (log.strip().splitlines() or ["no output produced"])[-1]
        raise WorkerError(classify_text(message) or classify_text(log) or TRANSIENT, message)

    # The guard seeing no page means FanFicFare fetches some other way now: the book may
    # hold anything, so it is not published.
    check_guard_saw_pages(request.url, PAGES)

    if chapter_count(out_path) == 0:
        # FanFicFare checks the chapter count the site *claims* (AO3's work stats), not the
        # chapters it found, so an error page in place of the chapter list gives a book with
        # only a title page, "successfully". On an update it would replace a good book. The
        # page guard should catch that page first; this is the last line of defence.
        raise WorkerError(
            classify_text(log) or SITE_UNAVAILABLE,
            "FanFicFare found no chapters (the site probably sent an error page)",
        )

    metadata = None
    if capture.adapter is not None:
        try:
            metadata = work_metadata(capture.adapter)
        except Exception:  # noqa: BLE001 - the book matters more than its details
            print("could not read the work's details:\n" + traceback.format_exc(), file=buffer)
    title = epub_title(out_path)
    if title is None:
        print("could not read the book's title from the EPUB", file=buffer)
        title = str(metadata.get("title") or "") if metadata else ""
    return {
        "status": "ok",
        "path": out_path,
        "title": title,
        "changed": before is None or sha256(out_path) != before,
        "metadata": metadata,
    }


# ------------------------------------------------------------------------------------ main


def emit(payload: dict[str, object]) -> None:
    """Print the one result line. Never fails: an unprintable payload becomes an error."""
    try:
        line = json.dumps(payload, default=str)
    except (TypeError, ValueError) as exc:
        line = json.dumps({"status": "error", "kind": TRANSIENT, "message": "unprintable result: %s" % exc, "log": ""})
    out = sys.__stdout__
    assert out is not None, "the worker always runs with a stdout"
    out.write("\n" + line + "\n")
    out.flush()


def tail(text: str) -> str:
    return text[-LOG_TAIL_CHARS:]


def run(request: Request, buffer: io.StringIO) -> dict[str, object]:
    """Do what `request` asks; returns the success payload, raises on failure."""
    logging.getLogger("fanficfare").setLevel(logging.WARNING)  # DEBUG would flood stderr
    os.makedirs(request.workdir, exist_ok=True)
    os.chdir(request.workdir)
    install_page_guard()
    capture = AdapterCapture()
    capture.install()
    if request.clearance and request.cookies:
        request = apply_clearance(request, request.clearance, request.cookies)
    if request.op == "list_series":
        return list_series(request, buffer)
    return download(request, buffer, capture)


def main() -> None:
    buffer = io.StringIO()
    try:
        payload = run(read_request(), buffer)
        emit(dict(payload, log=tail(buffer.getvalue())))
    except WorkerError as exc:
        emit({"status": "error", "kind": exc.kind, "message": exc.message, "log": tail(buffer.getvalue())})
    except SystemExit as exc:  # FanFicFare's option parser exits on bad options
        emit({"status": "error", "kind": PERMANENT, "message": "invalid options: %s" % exc, "log": tail(buffer.getvalue())})
    except Exception as exc:  # noqa: BLE001 - classification is the point
        known = unreadable_page(exc)
        emit({
            "status": "error",
            "kind": known.kind if known else classify_exception(exc),
            "message": known.message if known else "%s: %s" % (type(exc).__name__, exc),
            "log": tail(buffer.getvalue() + "\n" + traceback.format_exc()),
        })


if __name__ == "__main__":
    main()
