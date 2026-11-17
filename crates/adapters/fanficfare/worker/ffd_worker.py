"""One-shot FanFicFare worker.

Protocol (one story per process):
  stdin : a JSON object  {"url", "existing", "workdir", "personal_ini",
                          "cookies", "user_agent", "clearance"}   (the last three are optional)
          `cookies` is a Netscape cookies.txt that FanFicFare loads, and the worker writes
          it back (cookies rotate during a run) so the caller can keep them.
          `clearance` {"flaresolverr", "timeout_ms", "url"} asks the worker to have FlareSolverr
          pass Cloudflare once first (see apply_clearance); FanFicFare then fetches by itself.
  stdout: FanFicFare's own chatter is captured; the LAST line printed is a JSON
          object, either
            {"status": "ok", "path", "title", "changed", "log"}
          or
            {"status": "error", "kind", "message", "log"}
          `kind` is one of: site_unavailable, rate_limited, blocked, auth_failed,
          not_found, transient, permanent  (see fanfold-domain's FailureKind).

The Rust side never parses FanFicFare's text; this script is the only place that
knows how FanFicFare reports problems.
"""

import contextlib
import hashlib
import io
import json
import logging
import os
import re
import sys
import traceback
import zipfile
import xml.etree.ElementTree as ET

LOG_TAIL_CHARS = 20000

EXCEPTION_KINDS = {
    "StoryDoesNotExist": "not_found",
    "FailedToLogin": "auth_failed",
    "NeedTimedOneTimePassword": "auth_failed",
    "AccessDenied": "auth_failed",
    "AdultCheckRequired": "permanent",
    "InvalidStoryURL": "permanent",
    "UnknownSite": "permanent",
    "PersonalIniFailed": "permanent",
    "RegularExpresssionFailed": "permanent",
    "ConflictingOptions": "permanent",
}

# FanFicFare reports many problems by printing and returning; match on its text.
TEXT_RULES = (
    ("error code: 1010", "blocked"),
    ("retry later", "rate_limited"),
    ("too many requests", "rate_limited"),
    ("login failed", "auth_failed"),
    ("adult check required", "permanent"),
    ("does not exist", "not_found"),
    ("downtime", "site_unavailable"),
    ("maintenance", "site_unavailable"),
    ("timed out", "site_unavailable"),
    ("connection", "site_unavailable"),
)


# Lines FanFicFare's CLI prints before giving up without raising or calling its failure callback.
FATAL_PRINTS = ("Login Failed on non-interactive process",)

# Cloudflare's own error page (5xx origin errors, 1xxx blocks). FlareSolverr renders it in its
# browser and reports HTTP 200, so FanFicFare would parse it as the page it asked for.
CLOUDFLARE_ERROR_PAGE = re.compile(r'id="cf-error-details"')
CLOUDFLARE_ERROR_CODE = re.compile(r"Error code:?\s*(\d{3,4})", re.IGNORECASE)


def cloudflare_error(text):
    """The error code if `text` is a Cloudflare error page, else None."""
    if not isinstance(text, str) or not CLOUDFLARE_ERROR_PAGE.search(text):
        return None
    match = CLOUDFLARE_ERROR_CODE.search(text)
    return int(match.group(1)) if match else 520


def status_for_cloudflare(code):
    """The HTTP status that classifies a Cloudflare error code like the real response would."""
    if code < 1000:
        return code  # 520-530: the origin (AO3) is unreachable
    return 429 if code == 1015 else 403  # 1015 is Cloudflare's rate limit, other 1xxx are blocks


def install_cloudflare_guard():
    """Make FanFicFare's FlareSolverr fetcher fail on a Cloudflare error page.

    Without this, an AO3 page that came back as Cloudflare's "525: SSL handshake failed"
    is taken for a real page: a chapter list with no chapters gives a book with only a
    title page, and FanFicFare reports success.
    """
    from fanficfare import exceptions
    from fanficfare.fetchers import fetcher_flaresolverr_proxy as module

    fetcher = module.FlareSolverr_ProxyFetcher
    if getattr(fetcher.request, "_cloudflare_guard", False):
        return
    original = fetcher.request

    def request(self, method, url, headers=None, parameters=None):
        response = original(self, method, url, headers=headers, parameters=parameters)
        code = cloudflare_error(getattr(response, "content", None))
        if code is not None:
            raise exceptions.HTTPErrorFFF(
                url,
                status_for_cloudflare(code),
                "Cloudflare error code: %d (through FlareSolverr)" % code,
                None,
            )
        return response

    request._cloudflare_guard = True
    fetcher.request = request


def chapter_count(path):
    """How many chapters FanFicFare finds in an EPUB (None if it cannot read one)."""
    from fanficfare.epubutils import get_dcsource_chaptercount

    try:
        return get_dcsource_chaptercount(path)[1]
    except Exception:  # noqa: BLE001 - not an EPUB FanFicFare can update
        return None


def classify_text(text):
    lowered = text.lower()
    for needle, kind in TEXT_RULES:
        if needle in lowered:
            return kind
    return None


def classify_exception(exc):
    status = getattr(exc, "status_code", None)
    if isinstance(status, int):
        if status == 429:
            return "rate_limited"
        if status == 403:
            return "blocked"
        if status in (404, 410):
            return "not_found"
        if status >= 500:  # includes Cloudflare 520-526
            return "site_unavailable"
        return "transient"
    name = type(exc).__name__
    if name in EXCEPTION_KINDS:
        return EXCEPTION_KINDS[name]
    lowered = name.lower()
    if "timeout" in lowered or "connection" in lowered:
        return "site_unavailable"
    return classify_text(str(exc)) or "transient"


def emit(payload):
    out = sys.__stdout__
    out.write("\n" + json.dumps(payload) + "\n")
    out.flush()


def tail(text):
    return text[-LOG_TAIL_CHARS:]


def sha256(path):
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 16), b""):
            digest.update(chunk)
    return digest.hexdigest()


def epub_title(path):
    with zipfile.ZipFile(path) as archive:
        container = ET.fromstring(archive.read("META-INF/container.xml"))
        rootfile = next(e for e in container.iter() if e.tag.endswith("rootfile"))
        opf = ET.fromstring(archive.read(rootfile.attrib["full-path"]))
    for element in opf.iter():
        if element.tag == "{http://purl.org/dc/elements/1.1/}title":
            return (element.text or "").strip()
    return ""


def build_options(cli, request, existing, target):
    argv = ["--non-interactive", "-f", "epub"]
    if request.get("personal_ini"):
        argv += ["-c", request["personal_ini"]]
    if request.get("cookies"):
        argv += ["--mozilla-cookies", request["cookies"]]
    if request.get("user_agent"):
        argv += ["-o", "user_agent=" + request["user_agent"]]
    if existing:
        argv += ["-u"]
    else:
        out_path = os.path.join(request["workdir"], "story.epub")
        argv += ["-o", "output_filename=" + out_path]
    argv.append(target)  # validateOptions insists on a positional argument
    parser = cli.mkParser(False)
    options, args = parser.parse_args(argv)
    cli.expandOptions(options)
    cli.validateOptions(parser, options, args)
    return options


class ClearanceError(Exception):
    """FlareSolverr could not pass Cloudflare for us."""


def apply_clearance(request):
    """Have FlareSolverr pass Cloudflare once; leave its cookies and User-Agent for FanFicFare.

    Used for the login fallback. AO3 refuses a login submitted *through* FlareSolverr (its
    form post carries no session cookie), but plain HTTP works once Cloudflare has been
    passed: the clearance cookie is bound to the browser's User-Agent, so FanFicFare then
    sends exactly that. FlareSolverr's fresh cookies replace the stored ones of the same
    name, except the long-lived login cookie, which only a real login can issue.
    """
    import http.cookiejar
    import urllib.request

    spec = request["clearance"]
    timeout_ms = int(spec.get("timeout_ms", 60000))
    body = json.dumps({"cmd": "request.get", "url": spec["url"], "maxTimeout": timeout_ms}).encode()
    http_request = urllib.request.Request(
        spec["flaresolverr"].rstrip("/") + "/v1", body, {"Content-Type": "application/json"}
    )
    try:
        with urllib.request.urlopen(http_request, timeout=timeout_ms / 1000 + 30) as response:
            answer = json.load(response)
    except Exception as exc:  # noqa: BLE001 - any failure to talk to FlareSolverr
        raise ClearanceError("cannot reach FlareSolverr: %s" % exc)
    solution = answer.get("solution") if answer.get("status") == "ok" else None
    if not solution:
        raise ClearanceError("FlareSolverr failed: %s" % answer.get("message"))

    path = request["cookies"]
    jar = http.cookiejar.MozillaCookieJar(path)
    try:
        jar.load(ignore_discard=True, ignore_expires=True)
    except OSError:
        pass
    fresh = {cookie["name"] for cookie in solution.get("cookies", [])}
    for cookie in list(jar):
        if cookie.name in fresh and cookie.name != "remember_user_token":
            jar.clear(cookie.domain, cookie.path, cookie.name)
    for cookie in solution.get("cookies", []):
        expires = cookie.get("expires") or cookie.get("expiry")
        expires = int(expires) if isinstance(expires, (int, float)) and expires > 0 else None
        domain = cookie["domain"]
        jar.set_cookie(
            http.cookiejar.Cookie(
                0, cookie["name"], cookie["value"], None, False, domain, True,
                domain.startswith("."), cookie.get("path") or "/", True,
                bool(cookie.get("secure")), expires, expires is None, None, None, {},
            )
        )
    jar.save(ignore_discard=True, ignore_expires=True)
    user_agent = solution.get("userAgent")
    if user_agent:
        request["user_agent"] = user_agent
        # The caller keeps the User-Agent next to the cookies it earned.
        with open(os.path.join(request["workdir"], "user_agent.txt"), "w") as handle:
            handle.write(user_agent)


def save_cookies(options, request):
    """Write the cookie jar back to the file it was loaded from, success or not.

    A run that Cloudflare refused may still have rotated cookies, and the caller decides
    what to keep. Never fails the download.
    """
    path = request.get("cookies")
    jar = getattr(options, "cookiejar", None)
    if not path or jar is None:
        return
    try:
        jar.save_cookiejar(path)
    except Exception:  # noqa: BLE001
        pass


def attempt(cli, request, existing, buffer):
    """Run FanFicFare once. Returns (output_path, failures)."""
    failures = []

    def warn(message):
        print(message)

    def fail(message):
        failures.append(str(message))
        print(message)

    target = existing or request["url"]
    options = build_options(cli, request, existing, target)
    out_path = existing or os.path.join(request["workdir"], "story.epub")
    try:
        with contextlib.redirect_stdout(buffer), contextlib.redirect_stderr(buffer):
            cli.do_download(target, options, None, None, warn, fail)
    finally:
        save_cookies(options, request)
    return out_path, failures


def read_request():
    try:
        request = json.load(sys.stdin)
        for key in ("url", "workdir"):
            if not isinstance(request.get(key), str) or not request[key]:
                raise ValueError("missing %r" % key)
        return request
    except Exception as exc:  # noqa: BLE001
        emit({"status": "error", "kind": "permanent", "message": "bad worker request: %s" % exc, "log": ""})
        return None


def main():
    request = read_request()
    if request is None:
        return
    logging.getLogger("fanficfare").setLevel(logging.WARNING)  # DEBUG would flood stderr
    os.makedirs(request["workdir"], exist_ok=True)
    os.chdir(request["workdir"])
    buffer = io.StringIO()
    try:
        from fanficfare import cli

        install_cloudflare_guard()
        if request.get("clearance") and request.get("cookies"):
            try:
                apply_clearance(request)
            except ClearanceError as exc:
                emit({"status": "error", "kind": "site_unavailable", "message": str(exc), "log": ""})
                return
        existing = request.get("existing")
        if existing and chapter_count(existing) == 0:
            # A book with no chapters (written before the checks below existed) cannot be
            # updated: FanFicFare refuses it as "no recognizable chapters", forever. Replace it.
            print("the stored book has no chapters: downloading it again", file=buffer)
            existing = None
        before = sha256(existing) if existing else None
        out_path, failures = attempt(cli, request, existing, buffer)

        # A failed update leaves the old file where it was, so "a file exists" proves nothing:
        # FanFicFare reporting a failure is a failure, whatever is on disk. It reports some of
        # them only by printing a line and returning, with no callback and no exception.
        log = buffer.getvalue()
        printed = [line for line in log.splitlines() if line.startswith(FATAL_PRINTS)]
        if failures or printed:
            message = (failures or printed)[-1]
            emit({
                "status": "error",
                "kind": classify_text(message) or classify_text(log) or "transient",
                "message": message,
                "log": tail(log),
            })
            return

        produced = os.path.exists(out_path) and os.path.getsize(out_path) > 0
        if produced and chapter_count(out_path) == 0:
            # FanFicFare checks the chapter count the site *claims* (AO3's work stats), not the
            # chapters it found, so an error page in place of the chapter list gives a book with
            # only a title page, "successfully". On an update it would replace a good book.
            log = buffer.getvalue()
            emit({
                "status": "error",
                "kind": classify_text(log) or "site_unavailable",
                "message": "FanFicFare found no chapters (the site probably sent an error page)",
                "log": tail(log),
            })
            return
        if produced:
            changed = before is None or sha256(out_path) != before
            emit({
                "status": "ok",
                "path": out_path,
                "title": epub_title(out_path),
                "changed": changed,
                "log": tail(buffer.getvalue()),
            })
            return
        log = buffer.getvalue()
        message = failures[-1] if failures else (log.strip().splitlines() or ["no output produced"])[-1]
        emit({
            "status": "error",
            "kind": classify_text(message) or classify_text(log) or "transient",
            "message": message,
            "log": tail(log),
        })
    except SystemExit as exc:
        emit({"status": "error", "kind": "permanent", "message": "invalid options: %s" % exc, "log": tail(buffer.getvalue())})
    except Exception as exc:  # noqa: BLE001 - classification is the point
        emit({
            "status": "error",
            "kind": classify_exception(exc),
            "message": "%s: %s" % (type(exc).__name__, exc),
            "log": tail(buffer.getvalue() + "\n" + traceback.format_exc()),
        })


if __name__ == "__main__":
    main()
