"""Unit tests for fanfold_browser's pure parts and its queue, with a fake browser.

Run: python -I -B -m unittest discover -s browser   (no Camoufox needed)
"""

from __future__ import annotations

import base64
import json
import threading
import time
import unittest
import urllib.error
import urllib.request
from collections.abc import Callable

import fanfold_browser as fb

AO3 = ("archiveofourown.org",)


class FakeClock:
    def __init__(self) -> None:
        self.now = 1000.0
        self.slept: list[float] = []

    def __call__(self) -> float:
        return self.now

    def sleep(self, seconds: float) -> None:
        self.slept.append(seconds)
        self.now += seconds


class ParseFetchTest(unittest.TestCase):
    def test_a_get_and_a_post(self) -> None:
        get = fb.parse_fetch({"url": "https://archiveofourown.org/works/1"}, AO3)
        self.assertEqual((get.method, get.form, get.timeout_s), ("GET", (), 120.0))
        post = fb.parse_fetch(
            {"method": "POST", "url": "https://archiveofourown.org/users/login",
             "form": [["user[login]", "a"], ["authenticity_token", "x+/="]], "timeout_ms": 5000},
            AO3,
        )
        self.assertEqual(post.form, (("user[login]", "a"), ("authenticity_token", "x+/=")))
        self.assertEqual(post.timeout_s, 5.0)

    def test_only_allowed_https_hosts(self) -> None:
        for url, status in [
            ("https://example.org/", 403),
            ("https://archiveofourown.org.evil.example/", 403),
            ("http://archiveofourown.org/", 400),
            ("file:///etc/passwd", 400),
        ]:
            with self.assertRaises(fb.BadRequest) as caught:
                fb.parse_fetch({"url": url}, AO3)
            self.assertEqual(caught.exception.status, status, url)
        fb.parse_fetch({"url": "https://www.archiveofourown.org/"}, AO3)

    def test_malformed_requests(self) -> None:
        for raw in [
            None,
            {"method": "DELETE", "url": "https://archiveofourown.org/"},
            {"url": "https://archiveofourown.org/", "form": [["a", "b"]]},
            {"method": "POST", "url": "https://archiveofourown.org/", "form": [["a"]]},
            {"url": "https://archiveofourown.org/", "timeout_ms": 0},
            {"url": "https://archiveofourown.org/", "timeout_ms": True},
        ]:
            with self.assertRaises(fb.BadRequest, msg=repr(raw)):
                fb.parse_fetch(raw, AO3)


def outcome(status: int = 200, body: bytes = b'<div id="main">work</div>', **kw: object) -> fb.Outcome:
    return fb.Outcome(status=status, url="https://archiveofourown.org/works/1", content_type="text/html", body=body, **kw)


class RefusedTest(unittest.TestCase):
    def test_what_counts_as_a_refusal(self) -> None:
        self.assertIsNone(fb.refused(outcome()))
        self.assertIsNone(fb.refused(outcome(404, b'<div id="main">Error 404</div>')))
        self.assertIsNone(fb.refused(outcome(525, b"SSL handshake failed")), "AO3's origin trouble, not a refusal")
        self.assertIn("429", fb.refused(outcome(429, b"")) or "")
        self.assertIn("bot filter", fb.refused(outcome(200, b"our systems think you may be a robot")) or "")
        self.assertIn("blocked", fb.refused(outcome(403, b"error code: 1020")) or "")
        self.assertIn("challenge", fb.refused(outcome(403, b"Just a moment", challenge_unsolved=True)) or "")

    def test_a_work_that_mentions_robots_is_not_the_bot_filter(self) -> None:
        self.assertIsNone(fb.refused(outcome(200, b'<div id="main">I may be a robot, said the android</div>')))


class TurnstileFrameTest(unittest.TestCase):
    def test_only_cloudflare_s_own_host_counts(self) -> None:
        self.assertTrue(fb.is_turnstile_frame("https://challenges.cloudflare.com/cdn-cgi/challenge-platform/h/b/turnstile/if/ov2"))
        self.assertTrue(fb.is_turnstile_frame("https://Challenges.Cloudflare.com/x"))
        for url in (
            "https://evil.example/?next=https://challenges.cloudflare.com/",
            "https://evil.example/challenges.cloudflare.com",
            "https://challenges.cloudflare.com.evil.example/",
            "https://challenges.cloudflare.com@evil.example/",
            "http://challenges.cloudflare.com/",
            "about:blank",
            "",
        ):
            self.assertFalse(fb.is_turnstile_frame(url), url)


class PacerTest(unittest.TestCase):
    def test_requests_start_at_least_the_interval_apart(self) -> None:
        clock = FakeClock()
        pacer = fb.Pacer(6.0, clock=clock, sleep=clock.sleep, jitter=lambda: 0.0)
        pacer.wait_turn()
        self.assertEqual(clock.slept, [], "the first request goes at once")
        clock.now += 2
        pacer.wait_turn()
        self.assertEqual(clock.slept, [4.0])
        clock.now += 30
        pacer.wait_turn()
        self.assertEqual(clock.slept, [4.0], "no wait after a quiet spell")

    def test_jitter_stretches_the_gap_up_to_half(self) -> None:
        clock = FakeClock()
        pacer = fb.Pacer(6.0, clock=clock, sleep=clock.sleep, jitter=lambda: 1.0)
        pacer.wait_turn()
        pacer.wait_turn()
        self.assertEqual(clock.slept, [9.0])


class CooldownTest(unittest.TestCase):
    def test_doubles_up_to_the_cap_and_resets_on_success(self) -> None:
        clock = FakeClock()
        cooldown = fb.Cooldown(120, 600, clock=clock)
        self.assertEqual(cooldown.remaining(), 0)
        self.assertEqual([cooldown.refused("429") for _ in range(4)], [120, 240, 480, 600])
        self.assertEqual(cooldown.remaining(), 600)
        clock.now += 600
        self.assertEqual(cooldown.remaining(), 0)
        cooldown.succeeded()
        self.assertEqual(cooldown.refused("429"), 120)


class FakeDriver:
    """Answers from a script, records the order requests reached the 'site' in."""

    def __init__(self, answers: dict[str, fb.Outcome] | None = None, delay: float = 0.0) -> None:
        self.answers = answers or {}
        self.delay = delay
        self.seen: list[str] = []
        self.active = 0
        self.max_active = 0
        self.lock = threading.Lock()

    def perform(self, fetch: fb.Fetch) -> fb.Outcome:
        with self.lock:
            self.active += 1
            self.max_active = max(self.max_active, self.active)
            self.seen.append(fetch.url)
        time.sleep(self.delay)
        with self.lock:
            self.active -= 1
        return self.answers.get(fetch.url, outcome())


def serve(driver: FakeDriver, **settings: object) -> tuple[str, fb.Service, Callable]:
    service = fb.Service(
        fb.Settings(listen_port=0, **settings),  # type: ignore[arg-type]
        driver,
        fb.Cooldown(120, 3600),
    )
    threading.Thread(target=service.loop, daemon=True).start()
    server = fb.http.server.ThreadingHTTPServer(("127.0.0.1", 0), fb.make_handler(service))
    threading.Thread(target=server.serve_forever, daemon=True).start()
    def stop() -> None:
        server.shutdown()
        server.server_close()

    return "http://127.0.0.1:%d" % server.server_address[1], service, stop


def call(base: str, body: object, path: str = "/fetch") -> tuple[int, dict]:
    request = urllib.request.Request(base + path, json.dumps(body).encode(), {"Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            return response.status, json.load(response)
    except urllib.error.HTTPError as exc:
        return exc.code, json.load(exc)


class ServiceTest(unittest.TestCase):
    def test_concurrent_downloads_reach_the_site_one_request_at_a_time(self) -> None:
        driver = FakeDriver(delay=0.05)
        base, _, stop = serve(driver)
        self.addCleanup(stop)
        results: list[int] = []

        def download(work: int) -> None:
            for chapter in range(3):
                status, _ = call(base, {"url": "https://archiveofourown.org/works/%d/chapters/%d" % (work, chapter)})
                results.append(status)

        threads = [threading.Thread(target=download, args=(w,)) for w in (1, 2)]
        for t in threads:
            t.start()
        for t in threads:
            t.join()
        self.assertEqual(results, [200] * 6)
        self.assertEqual(driver.max_active, 1, "never two requests in flight")
        self.assertEqual(len(driver.seen), 6)

    def test_the_answer_carries_the_site_s_status_url_and_bytes(self) -> None:
        url = "https://archiveofourown.org/works/9"
        driver = FakeDriver({url: fb.Outcome(404, url, "text/html", b'<div id="main">gone</div>')})
        base, _, stop = serve(driver)
        self.addCleanup(stop)
        status, body = call(base, {"url": url})
        self.assertEqual(status, 200)
        self.assertEqual((body["status"], body["url"]), (404, url))
        self.assertEqual(base64.b64decode(body["body_b64"]), b'<div id="main">gone</div>')

    def test_a_refusal_pauses_everyone_without_touching_the_site(self) -> None:
        bot = "https://archiveofourown.org/works/1"
        driver = FakeDriver({bot: outcome(429, b"Retry later")})
        base, service, stop = serve(driver)
        self.addCleanup(stop)
        status, body = call(base, {"url": bot})
        self.assertEqual((status, body["status"]), (200, 429), "the refusal itself is passed on")
        status, body = call(base, {"url": "https://archiveofourown.org/works/2"})
        self.assertEqual((status, body["error"]), (429, "cooling_down"))
        self.assertGreater(body["retry_after_s"], 100)
        self.assertEqual(driver.seen, [bot], "the second request never reached the site")
        self.assertGreater(service.cooldown.remaining(), 100)

    def test_a_full_queue_is_refused_not_grown(self) -> None:
        driver = FakeDriver(delay=0.3)
        base, _, stop = serve(driver, max_queue=1)
        self.addCleanup(stop)
        statuses: list[int] = []

        def one(i: int) -> None:
            statuses.append(call(base, {"url": "https://archiveofourown.org/works/%d" % i})[0])

        threads = [threading.Thread(target=one, args=(i,)) for i in range(4)]
        for t in threads:
            t.start()
            time.sleep(0.02)
        for t in threads:
            t.join()
        self.assertIn(503, statuses)
        self.assertIn(200, statuses)

    def test_a_request_whose_deadline_passed_in_the_queue_is_not_sent(self) -> None:
        driver = FakeDriver(delay=1.5)
        base, _, stop = serve(driver)
        self.addCleanup(stop)
        first = threading.Thread(target=call, args=(base, {"url": "https://archiveofourown.org/works/1"}))
        first.start()
        time.sleep(0.1)
        status, body = call(base, {"url": "https://archiveofourown.org/works/2", "timeout_ms": 1000})
        first.join()
        self.assertEqual((status, body["error"]), (503, "timeout"))
        self.assertEqual(driver.seen, ["https://archiveofourown.org/works/1"])

    def test_bad_requests_and_health(self) -> None:
        base, _, stop = serve(FakeDriver())
        self.addCleanup(stop)
        self.assertEqual(call(base, {"url": "https://example.org/"})[0], 403)
        with urllib.request.urlopen(base + "/health", timeout=5) as response:
            self.assertEqual(json.load(response), {"ok": True, "queued": 0, "cooling_down_s": 0})


class SettingsTest(unittest.TestCase):
    def test_from_env(self) -> None:
        s = fb.Settings.from_env({
            "BROWSER_PROXY": "http://home-egress.egress:8080",
            "ALLOWED_HOSTS": "archiveofourown.org, ao3.org",
            "MIN_INTERVAL_S": "8",
            "HEADLESS": "virtual",
        })
        self.assertEqual(s.proxy, "http://home-egress.egress:8080")
        self.assertEqual(s.allowed_hosts, ("archiveofourown.org", "ao3.org"))
        self.assertEqual((s.min_interval_s, s.headless), (8.0, "virtual"))
        self.assertEqual(fb.Settings.from_env({}).proxy, None)
        with self.assertRaises(ValueError):
            fb.Settings.from_env({"MIN_INTERVAL_S": "-1"})


if __name__ == "__main__":
    unittest.main()
