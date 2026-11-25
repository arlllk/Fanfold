"""fanfold-browser: one Camoufox browser that makes every request Fanfold sends to AO3.

Cloudflare checks that a request comes from the client its clearance was issued to (TLS
fingerprint included), so cookies earned by a browser cannot be replayed from another HTTP
client. Every request (pages, the login token, the login form itself) therefore goes through
this one browser, which keeps its cookies (Cloudflare's clearance, AO3's session and login) in
its own profile. Nothing else holds them.

Gentle on the site, whatever the callers do:
  * one browser, one tab, one request at a time: concurrent downloads queue up (FIFO, so they
    take turns request by request) and never reach the site in parallel;
  * every request starts at least MIN_INTERVAL_S (jittered) after the previous one;
  * when the site refuses (HTTP 429, AO3's bot filter, a Cloudflare block or a challenge that
    does not clear) the service cools down: for a while, growing exponentially, it refuses
    every request itself, without touching the site, so the callers back off together;
  * only hosts in ALLOWED_HOSTS are fetched.

A POST is a form built and submitted inside a page of the target site, the way a person's
browser submits a form: same origin, so the browser sends its session cookie and a correct
Origin, and the values arrive untouched. (FlareSolverr posts from a data: URL page and
percent-encodes every value, which AO3 rejects with "Auth Error".)

API (JSON over HTTP):
  POST /fetch  {"method": "GET"|"POST", "url", "form": [[name, value], ...], "timeout_ms"}
    200 {"status", "url", "content_type", "body_b64"}   what the site answered (any status)
    429 {"error": "cooling_down", "retry_after_s", "message"}
    503 {"error": "busy" | "timeout" | "browser", "message"}
    400 / 403 / 404  a bad request, a host that is not allowed, an unknown path
  GET /health  {"ok", "queued", "cooling_down_s"}   (never touches the site)

Settings come from the environment; see Settings.from_env.
"""

from __future__ import annotations

import base64
import dataclasses
import http.server
import json
import logging
import os
import queue
import random
import re
import threading
import time
import urllib.parse
from collections.abc import Callable

log = logging.getLogger("fanfold-browser")


# ---------------------------------------------------------------------------- settings


@dataclasses.dataclass(frozen=True)
class Settings:
    listen_host: str = "0.0.0.0"
    listen_port: int = 8191
    profile_dir: str = "/data/profile"
    proxy: str | None = None
    allowed_hosts: tuple[str, ...] = ("archiveofourown.org",)
    min_interval_s: float = 6.0
    challenge_timeout_s: float = 45.0
    cooldown_base_s: float = 120.0
    cooldown_cap_s: float = 3600.0
    max_queue: int = 16
    headless: bool | str = True
    # The OS the browser presents as. Linux by default: it matches the TCP/IP stack underneath,
    # which a browser cannot change, and the image keeps only Linux's fonts.
    browser_os: str = "linux"
    # Directories to create before launching (the image points one of Camoufox's into /tmp).
    runtime_dirs: tuple[str, ...] = ()

    @classmethod
    def from_env(cls, env: dict[str, str]) -> Settings:
        def number(name: str, default: float) -> float:
            value = float(env.get(name, default))
            if value < 0:
                raise ValueError("%s must not be negative" % name)
            return value

        headless_raw = env.get("HEADLESS", "true").lower()
        headless: bool | str = headless_raw if headless_raw == "virtual" else headless_raw != "false"
        hosts = tuple(h.strip().lower() for h in env.get("ALLOWED_HOSTS", "archiveofourown.org").split(",") if h.strip())
        if not hosts:
            raise ValueError("ALLOWED_HOSTS must name at least one host")
        return cls(
            listen_host=env.get("LISTEN_HOST", cls.listen_host),
            listen_port=int(env.get("LISTEN_PORT", cls.listen_port)),
            profile_dir=env.get("PROFILE_DIR", cls.profile_dir),
            proxy=env.get("BROWSER_PROXY") or None,
            allowed_hosts=hosts,
            min_interval_s=number("MIN_INTERVAL_S", cls.min_interval_s),
            challenge_timeout_s=number("CHALLENGE_TIMEOUT_S", cls.challenge_timeout_s),
            cooldown_base_s=number("COOLDOWN_BASE_S", cls.cooldown_base_s),
            cooldown_cap_s=number("COOLDOWN_CAP_S", cls.cooldown_cap_s),
            max_queue=max(1, int(env.get("MAX_QUEUE", cls.max_queue))),
            headless=headless,
            browser_os=env.get("BROWSER_OS", cls.browser_os),
            runtime_dirs=tuple(d for d in env.get("RUNTIME_DIRS", "").split(":") if d),
        )


# ------------------------------------------------------------------------- the pure part


@dataclasses.dataclass(frozen=True)
class Fetch:
    method: str
    url: str
    form: tuple[tuple[str, str], ...]
    timeout_s: float


@dataclasses.dataclass(frozen=True)
class Outcome:
    status: int
    url: str
    content_type: str
    body: bytes
    # The page was still Cloudflare's challenge when the browser gave up waiting.
    challenge_unsolved: bool = False


class BadRequest(Exception):
    def __init__(self, status: int, message: str) -> None:
        super().__init__(message)
        self.status = status
        self.message = message


def parse_fetch(raw: object, allowed_hosts: tuple[str, ...]) -> Fetch:
    if not isinstance(raw, dict):
        raise BadRequest(400, "the body must be a JSON object")
    method = raw.get("method", "GET")
    if method not in ("GET", "POST"):
        raise BadRequest(400, "method must be GET or POST")
    url = raw.get("url")
    if not isinstance(url, str):
        raise BadRequest(400, "url must be a string")
    parsed = urllib.parse.urlsplit(url)
    host = (parsed.hostname or "").lower()
    if parsed.scheme != "https" or not host:
        raise BadRequest(400, "url must be an https URL")
    if not any(host == allowed or host.endswith("." + allowed) for allowed in allowed_hosts):
        raise BadRequest(403, "%s is not an allowed host" % host)
    form_raw = raw.get("form") or []
    if not isinstance(form_raw, list) or not all(
        isinstance(pair, list) and len(pair) == 2 and all(isinstance(x, str) for x in pair) for pair in form_raw
    ):
        raise BadRequest(400, "form must be a list of [name, value] string pairs")
    if form_raw and method != "POST":
        raise BadRequest(400, "only a POST carries a form")
    timeout_ms = raw.get("timeout_ms", 120_000)
    if not isinstance(timeout_ms, int) or isinstance(timeout_ms, bool) or timeout_ms <= 0:
        raise BadRequest(400, "timeout_ms must be a positive integer")
    return Fetch(method=method, url=url, form=tuple((k, v) for k, v in form_raw), timeout_s=timeout_ms / 1000)


BOT_FILTER = re.compile(rb"may be a robot|Retry later", re.IGNORECASE)
CLOUDFLARE_BLOCK = re.compile(rb"error code:?\s*10\d\d|Sorry, you have been blocked", re.IGNORECASE)


def refused(outcome: Outcome) -> str | None:
    """Why the site refused this client, when it did: a reason to cool down."""
    head = outcome.body[:64 * 1024]
    if outcome.challenge_unsolved:
        return "Cloudflare's challenge did not clear"
    if outcome.status == 429:
        return "the site answered 429 Too Many Requests"
    if BOT_FILTER.search(head) and b'id="main"' not in head:
        return "AO3's bot filter refused the request"
    if outcome.status == 403 and CLOUDFLARE_BLOCK.search(head):
        return "Cloudflare blocked the request"
    return None


class Pacer:
    """Spaces the starts of requests at least `interval` apart, jittered to 1-1.5x."""

    def __init__(
        self,
        interval: float,
        clock: Callable[[], float] = time.monotonic,
        sleep: Callable[[float], None] = time.sleep,
        jitter: Callable[[], float] = random.random,
    ) -> None:
        self.interval = interval
        self.clock = clock
        self.sleep = sleep
        self.jitter = jitter
        self.last: float | None = None

    def wait_turn(self) -> None:
        if self.last is not None:
            gap = self.interval * (1 + 0.5 * self.jitter())
            remaining = self.last + gap - self.clock()
            if remaining > 0:
                self.sleep(remaining)
        self.last = self.clock()


class Cooldown:
    """After a refusal nothing is sent for a while: base, doubling per refusal, up to cap."""

    def __init__(self, base: float, cap: float, clock: Callable[[], float] = time.monotonic) -> None:
        self.base = base
        self.cap = cap
        self.clock = clock
        self.level = 0
        self.until = 0.0
        self.reason = ""

    def remaining(self) -> float:
        return max(0.0, self.until - self.clock())

    def refused(self, reason: str) -> float:
        delay = min(self.cap, self.base * 2 ** self.level)
        self.level += 1
        self.until = self.clock() + delay
        self.reason = reason
        return delay

    def succeeded(self) -> None:
        self.level = 0


# ---------------------------------------------------------------------- the browser part


SUBMIT_FORM = """([url, fields]) => {
    const form = document.createElement('form');
    form.method = 'POST';
    form.action = url;
    form.style.display = 'none';
    for (const [name, value] of fields) {
        const input = document.createElement('input');
        input.type = 'hidden';
        input.name = name;
        input.value = value;
        form.appendChild(input);
    }
    document.body.appendChild(form);
    form.submit();
}"""

CHALLENGE_TITLES = ("just a moment", "attention required", "un momento", "einen moment")
TURNSTILE_HOST = "challenges.cloudflare.com"


def is_turnstile_frame(url: str) -> bool:
    """Whether a frame is Cloudflare's own challenge: https on exactly its host. A substring
    test would also match a page that merely mentions the host in its path or query, or a
    look-alike host such as challenges.cloudflare.com.example."""
    parts = urllib.parse.urlsplit(url)
    return parts.scheme == "https" and (parts.hostname or "").lower() == TURNSTILE_HOST


def origin(url: str) -> str:
    parts = urllib.parse.urlsplit(url)
    return "%s://%s" % (parts.scheme, parts.netloc)


class CamoufoxDriver:
    """The browser: launched on first use, kept for the life of the service (a fresh launch
    is a fresh fingerprint), relaunched if it dies. Only ever used from one thread."""

    def __init__(self, settings: Settings, pacer: Pacer) -> None:
        self.settings = settings
        self.pacer = pacer
        self._manager = None
        self._context = None
        self._page = None

    def _launch(self) -> None:
        from camoufox.sync_api import Camoufox

        options: dict[str, object] = {
            "persistent_context": True,
            "user_data_dir": self.settings.profile_dir,
            "headless": self.settings.headless,
            "os": self.settings.browser_os,
            # Timezone, locale and WebRTC address follow the address the browser leaves from.
            "geoip": True,
            "humanize": True,
        }
        if self.settings.proxy:
            options["proxy"] = {"server": self.settings.proxy}
        for directory in (self.settings.profile_dir, *self.settings.runtime_dirs):
            os.makedirs(directory, exist_ok=True)
        log.info("launching Camoufox (profile %s, proxy %s)", self.settings.profile_dir, self.settings.proxy or "none")
        manager = Camoufox(**options)
        context = manager.__enter__()
        self._manager, self._context = manager, context
        self._page = context.pages[0] if context.pages else context.new_page()

    def close(self) -> None:
        manager, self._manager, self._context, self._page = self._manager, None, None, None
        if manager is not None:
            try:
                manager.__exit__(None, None, None)
            except Exception:  # noqa: BLE001 - a dead browser closes badly
                log.exception("closing the browser failed")

    def page(self):  # noqa: ANN201 - Playwright's Page
        if self._page is None or self._page.is_closed():
            self.close()
            self._launch()
        return self._page

    def is_challenge(self, page) -> bool:  # noqa: ANN001
        try:
            title = (page.title() or "").lower()
        except Exception:  # noqa: BLE001 - mid-navigation
            return True
        if any(t in title for t in CHALLENGE_TITLES):
            return True
        return page.locator("#challenge-form, #challenge-stage, iframe[src^='https://challenges.cloudflare.com/']").count() > 0

    def try_click_turnstile(self, page) -> None:  # noqa: ANN001
        """Best effort: tick Cloudflare's checkbox when its challenge shows one."""
        for frame in page.frames:
            if is_turnstile_frame(frame.url or ""):
                try:
                    box = frame.frame_element().bounding_box()
                    if box:
                        page.mouse.click(box["x"] + 30, box["y"] + box["height"] / 2)
                        log.info("clicked Cloudflare's checkbox")
                except Exception:  # noqa: BLE001
                    pass

    def wait_out_challenge(self, page, deadline: float) -> bool:  # noqa: ANN001
        """Wait while Cloudflare's challenge runs (the browser solves it and Cloudflare
        reloads the page). True when it cleared."""
        clicked = False
        while time.monotonic() < deadline:
            if not self.is_challenge(page):
                return True
            if not clicked and time.monotonic() > deadline - self.settings.challenge_timeout_s + 8:
                self.try_click_turnstile(page)
                clicked = True
            page.wait_for_timeout(1000)
        return not self.is_challenge(page)

    def perform(self, fetch: Fetch) -> Outcome:
        page = self.page()
        timeout_ms = max(1000, int(fetch.timeout_s * 1000))
        if fetch.method == "POST" and origin(page.url) != origin(fetch.url):
            # A form is submitted from a page of its own site: open that site first.
            self.pacer.wait_turn()
            self._settle(page, page.goto(origin(fetch.url) + "/", wait_until="domcontentloaded", timeout=timeout_ms))
        self.pacer.wait_turn()
        if fetch.method == "POST":
            with page.expect_navigation(wait_until="domcontentloaded", timeout=timeout_ms) as navigation:
                page.evaluate(SUBMIT_FORM, [fetch.url, [list(pair) for pair in fetch.form]])
            response = navigation.value
        else:
            response = page.goto(fetch.url, wait_until="domcontentloaded", timeout=timeout_ms)
        return self._settle(page, response)

    def _settle(self, page, response) -> Outcome:  # noqa: ANN001
        """What the navigation ended on, once any Cloudflare challenge is over."""
        status = response.status if response is not None else 0
        headers = response.headers if response is not None else {}
        challenged = headers.get("cf-mitigated") == "challenge" or self.is_challenge(page)
        if challenged:
            log.info("Cloudflare challenge on %s; waiting", page.url)
            cleared = self.wait_out_challenge(page, time.monotonic() + self.settings.challenge_timeout_s)
            page.wait_for_load_state("domcontentloaded")
            return Outcome(
                status=200 if cleared else (status or 403),
                url=page.url,
                content_type="text/html; charset=utf-8",
                body=page.content().encode("utf-8"),
                challenge_unsolved=not cleared,
            )
        body: bytes
        try:
            body = response.body() if response is not None else page.content().encode("utf-8")
        except Exception:  # noqa: BLE001 - some navigations keep no body
            body = page.content().encode("utf-8")
        return Outcome(
            status=status,
            url=response.url if response is not None else page.url,
            content_type=headers.get("content-type", ""),
            body=body,
        )


# ------------------------------------------------------------------------------ service


@dataclasses.dataclass
class Job:
    fetch: Fetch
    deadline: float
    done: threading.Event = dataclasses.field(default_factory=threading.Event)
    reply: tuple[int, dict[str, object]] | None = None

    def answer(self, status: int, body: dict[str, object]) -> None:
        self.reply = (status, body)
        self.done.set()


class Service:
    def __init__(self, settings: Settings, driver: object, cooldown: Cooldown) -> None:
        self.settings = settings
        self.driver = driver
        self.cooldown = cooldown
        self.jobs: queue.Queue[Job] = queue.Queue(maxsize=settings.max_queue)

    def submit(self, fetch: Fetch) -> tuple[int, dict[str, object]]:
        """Queue a fetch and wait for its answer (called from the HTTP threads)."""
        wait = self.cooldown.remaining()
        if wait > 0:
            return self._cooling(wait)
        job = Job(fetch=fetch, deadline=time.monotonic() + fetch.timeout_s)
        try:
            self.jobs.put_nowait(job)
        except queue.Full:
            return 503, {"error": "busy", "message": "%d requests are already waiting" % self.settings.max_queue}
        if not job.done.wait(fetch.timeout_s + 5) or job.reply is None:
            return 503, {"error": "timeout", "message": "no answer within %.0f s" % fetch.timeout_s}
        return job.reply

    def _cooling(self, wait: float) -> tuple[int, dict[str, object]]:
        return 429, {
            "error": "cooling_down",
            "retry_after_s": round(wait),
            "message": "not asking the site anything for %.0f s more: %s" % (wait, self.cooldown.reason),
        }

    def run_one(self, job: Job) -> None:
        """Handle one queued fetch (called from the browser thread only)."""
        wait = self.cooldown.remaining()
        if wait > 0:  # a refusal happened while this one was queued
            job.answer(*self._cooling(wait))
            return
        remaining = job.deadline - time.monotonic()
        if remaining <= 1:
            job.answer(503, {"error": "timeout", "message": "waited in the queue until its deadline"})
            return
        fetch = dataclasses.replace(job.fetch, timeout_s=remaining)
        try:
            outcome = self.driver.perform(fetch)  # type: ignore[attr-defined]
        except Exception as exc:  # noqa: BLE001 - report, and start afresh next time
            log.warning("%s %s failed: %s", fetch.method, fetch.url, exc)
            close = getattr(self.driver, "close", None)
            if close is not None and "closed" in str(exc).lower():
                close()
            job.answer(503, {"error": "browser", "message": "%s: %s" % (type(exc).__name__, exc)})
            return
        reason = refused(outcome)
        if reason:
            delay = self.cooldown.refused(reason)
            log.warning("%s on %s; pausing all requests for %.0f s", reason, fetch.url, delay)
        else:
            self.cooldown.succeeded()
        log.info("%s %s -> %d (%d bytes)", fetch.method, fetch.url, outcome.status, len(outcome.body))
        job.answer(200, {
            "status": outcome.status,
            "url": outcome.url,
            "content_type": outcome.content_type,
            "body_b64": base64.b64encode(outcome.body).decode("ascii"),
        })

    def loop(self) -> None:
        while True:
            self.run_one(self.jobs.get())


def make_handler(service: Service) -> type[http.server.BaseHTTPRequestHandler]:
    class Handler(http.server.BaseHTTPRequestHandler):
        server_version = "fanfold-browser"

        def _send(self, status: int, body: dict[str, object]) -> None:
            data = json.dumps(body).encode("utf-8")
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)

        def do_GET(self) -> None:  # noqa: N802
            if self.path != "/health":
                self._send(404, {"error": "not_found", "message": "unknown path"})
                return
            self._send(200, {
                "ok": True,
                "queued": service.jobs.qsize(),
                "cooling_down_s": round(service.cooldown.remaining()),
            })

        def do_POST(self) -> None:  # noqa: N802
            if self.path != "/fetch":
                self._send(404, {"error": "not_found", "message": "unknown path"})
                return
            try:
                length = int(self.headers.get("Content-Length") or 0)
                if length > 1024 * 1024:
                    raise BadRequest(400, "request too large")
                fetch = parse_fetch(json.loads(self.rfile.read(length) or b"null"), service.settings.allowed_hosts)
            except BadRequest as exc:
                self._send(exc.status, {"error": "bad_request", "message": exc.message})
                return
            except ValueError as exc:
                self._send(400, {"error": "bad_request", "message": "not JSON: %s" % exc})
                return
            self._send(*service.submit(fetch))

        def log_message(self, format: str, *args: object) -> None:  # noqa: A002
            log.debug("%s " + format, self.address_string(), *args)

    return Handler


def main() -> None:
    logging.basicConfig(level=os.environ.get("LOG_LEVEL", "INFO").upper(), format="%(asctime)s %(levelname)s %(message)s")
    settings = Settings.from_env(dict(os.environ))
    pacer = Pacer(settings.min_interval_s)
    service = Service(settings, CamoufoxDriver(settings, pacer), Cooldown(settings.cooldown_base_s, settings.cooldown_cap_s))
    threading.Thread(target=service.loop, name="browser", daemon=True).start()
    server = http.server.ThreadingHTTPServer((settings.listen_host, settings.listen_port), make_handler(service))
    log.info("listening on %s:%d, hosts %s", settings.listen_host, settings.listen_port, ", ".join(settings.allowed_hosts))
    server.serve_forever()


if __name__ == "__main__":
    main()
