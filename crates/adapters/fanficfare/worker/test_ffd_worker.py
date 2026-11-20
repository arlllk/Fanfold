"""Unit tests for ffd_worker.py: fast, offline, no FlareSolverr.

    python -I -m unittest discover -s crates/adapters/fanficfare/worker

The page guard tests need FanFicFare (the pinned venv, as for `just test-worker`); the rest
are pure. End-to-end behaviour through the real FanFicFare CLI is covered by the Rust
adapter's ignored tests.
"""

from __future__ import annotations

import contextlib
import importlib.util
import io
import unittest

import ffd_worker as w

HAS_FANFICFARE = importlib.util.find_spec("fanficfare") is not None

WORK = "https://archiveofourown.org/works/10000001?view_adult=true"

# What AO3 answered from a flagged address in November 2026 (HTTP 429, plain text).
AO3_BOT_FILTER = (
    "The bot servers are currently overloaded, please try again later. You are seeing this "
    "page because our systems think you may be a robot. If you are not a robot, this could be "
    "due to using a VPN or privacy-focused browser."
)
CLOUDFLARE_525 = (
    '<html><head><title>archiveofourown.org | 525: SSL handshake failed</title></head><body>'
    '<div id="cf-wrapper"><div id="cf-error-details"><h1>SSL handshake failed</h1>'
    '<span class="cf-error-type">Error code 525</span></div></div></body></html>'
)
CLOUDFLARE_1015 = CLOUDFLARE_525.replace("525", "1015")
CLOUDFLARE_CHALLENGE = (
    "<!DOCTYPE html><html><head><title>Just a moment...</title></head><body>"
    '<script src="/cdn-cgi/challenge-platform/h/b/orchestrate/chl_page/v1"></script></body></html>'
)
# A real AO3 work whose *text* happens to contain every error page's wording.
AO3_WORK_QUOTING_ERRORS = (
    '<html><body><div id="outer"><div id="inner"><div id="main" class="works-show region" '
    'role="main"><h2 class="title heading">Retry later</h2><div class="userstuff">'
    + AO3_BOT_FILTER + " Just a moment... " + CLOUDFLARE_525
    + "</div></div></div></div></body></html>"
)


class JudgeTest(unittest.TestCase):
    def test_ao3_s_own_pages_pass_whatever_they_say(self):
        self.assertIsNone(w.judge(WORK, AO3_WORK_QUOTING_ERRORS))
        self.assertIsNone(w.judge(WORK, AO3_WORK_QUOTING_ERRORS.encode()))

    def test_the_bot_filter_is_a_rate_limit(self):
        rejection = w.judge(WORK, AO3_BOT_FILTER)
        self.assertEqual(rejection.status, 429)
        self.assertFalse(rejection.retry_now, "asking again at once only makes it worse")
        self.assertEqual(w.classify_status(rejection.status), w.RATE_LIMITED)
        self.assertEqual(w.classify_text(rejection.message), w.RATE_LIMITED)
        self.assertIn(WORK, rejection.message)
        self.assertIn("think you may be a robot", rejection.message)

    def test_a_bare_retry_later_is_a_rate_limit(self):
        for page in ("Retry later\n", "<html><body><p>Retry later</p></body></html>"):
            self.assertEqual(w.judge(WORK, page).status, 429, page)
        self.assertEqual(w.classify_text(w.judge(WORK, "Retry later").message), w.RATE_LIMITED)

    def test_cloudflare_origin_errors_are_retried_at_once_and_mean_the_site_is_down(self):
        rejection = w.judge(WORK, CLOUDFLARE_525)
        self.assertEqual((rejection.status, rejection.retry_now), (525, True))
        self.assertEqual(w.classify_status(rejection.status), w.SITE_UNAVAILABLE)
        self.assertIn("525", rejection.message)

    def test_cloudflare_blocks_and_rate_limits_are_not_retried_at_once(self):
        self.assertEqual(w.judge(WORK, CLOUDFLARE_1015), w.Rejection(429, w.judge(WORK, CLOUDFLARE_1015).message))
        blocked = w.judge(WORK, CLOUDFLARE_1015.replace("1015", "1020"))
        self.assertEqual((blocked.status, blocked.retry_now), (403, False))

    def test_a_cloudflare_challenge_is_a_block(self):
        rejection = w.judge(WORK, CLOUDFLARE_CHALLENGE)
        self.assertEqual(rejection.status, 403)
        self.assertEqual(w.classify_text(rejection.message), w.BLOCKED)

    def test_any_other_page_in_ao3_s_place_is_an_outage_named_in_the_message(self):
        rejection = w.judge(WORK, "<html><body><h1>502 Bad Gateway</h1><hr>nginx</body></html>")
        self.assertEqual(w.classify_status(rejection.status), w.SITE_UNAVAILABLE)
        self.assertEqual(w.classify_text(rejection.message), w.SITE_UNAVAILABLE)
        self.assertIn("502 Bad Gateway nginx", rejection.message)
        self.assertIn(WORK, rejection.message)

    def test_what_is_not_an_ao3_page_is_not_judged_as_one(self):
        odd = "<html><body>no main region</body></html>"
        self.assertIsNone(w.judge("https://archiveofourown.org/token_dispenser.json", odd))
        self.assertIsNone(w.judge(WORK, '{"token": "x"}'), "JSON is never a page")
        self.assertIsNone(w.judge(WORK, b"\x89PNG\r\n\x1a\n\x00\x00"), "nor is an image")
        self.assertIsNone(w.judge("https://example.com/cover.html", odd), "other sites are theirs")
        self.assertIsNone(w.judge("file:///tmp/story.html", odd))
        self.assertIsNone(w.judge(WORK, None))

    def test_cloudflare_s_pages_are_caught_on_any_site(self):
        self.assertEqual(w.judge("https://example.com/", CLOUDFLARE_525).status, 525)

    def test_every_ao3_host_is_ao3(self):
        for host in ("archiveofourown.org", "www.archiveofourown.org", "ao3.org", "archiveofourown.gay"):
            self.assertTrue(w.is_ao3_page("https://%s/works/1" % host), host)
        self.assertFalse(w.is_ao3_page("https://notarchiveofourown.org/works/1"))
        self.assertFalse(w.is_ao3_page("https://archiveofourown.org.evil.example/works/1"))

    def test_only_the_start_of_a_large_page_is_read(self):
        huge = '<div id="main">' + "x" * (10 * w.INSPECT_CHARS)
        self.assertEqual(len(w.page_text(huge)), w.INSPECT_CHARS)
        self.assertIsNone(w.judge(WORK, huge))


class SnippetTest(unittest.TestCase):
    def test_a_snippet_is_the_visible_text_shortened(self):
        page = "<html><head><style>p{}</style><script>var x=1</script></head><body><p>A&amp;B</p>" \
               "<!-- hidden --><p>" + "word " * 100 + "</p></body></html>"
        text = w.snippet(page)
        self.assertTrue(text.startswith("A&B word word"), text)
        self.assertEqual(len(text), w.SNIPPET_CHARS)
        self.assertTrue(text.endswith("…"))
        self.assertNotIn("var x", text)
        self.assertNotIn("hidden", text)


class ClassifyTest(unittest.TestCase):
    def test_the_guard_s_own_messages_classify_like_their_status(self):
        for page in w.KNOWN_PAGES:
            self.assertEqual(w.classify_text(page.description), w.classify_status(page.status), page.name)

    def test_statuses(self):
        cases = {429: w.RATE_LIMITED, 403: w.BLOCKED, 404: w.NOT_FOUND, 410: w.NOT_FOUND,
                 503: w.SITE_UNAVAILABLE, 525: w.SITE_UNAVAILABLE, 418: w.TRANSIENT}
        for status, kind in cases.items():
            self.assertEqual(w.classify_status(status), kind, status)

    def test_a_wrapped_http_error_classifies_by_its_status(self):
        class HTTPError(Exception):
            status_code = 525

        try:
            try:
                raise HTTPError()
            except HTTPError as inner:
                raise RuntimeError("Failed getting login token") from inner
        except RuntimeError as outer:
            self.assertEqual(w.classify_exception(outer), w.SITE_UNAVAILABLE)

    def test_every_kind_is_one_the_rust_side_knows(self):
        kinds = set(w.EXCEPTION_KINDS.values()) | {kind for _, kind in w.TEXT_RULES}
        self.assertLessEqual(kinds, set(w.KINDS))


class UnreadablePageTest(unittest.TestCase):
    @staticmethod
    def crash_in(filename):
        code = compile("None.get_text()", filename, "exec")
        try:
            exec(code, {})
        except AttributeError as exc:
            return exc
        raise AssertionError("no crash")

    def test_a_crash_inside_a_site_adapter_names_the_page(self):
        pages = w.PageLog()
        pages.record(WORK, '<div id="main"><p>Something new</p></div>')
        exc = self.crash_in("/venv/site-packages/fanficfare/adapters/base_otw_adapter.py")
        error = w.unreadable_page(exc, pages)
        self.assertEqual(error.kind, w.TRANSIENT)
        self.assertIn(WORK, error.message)
        self.assertIn("Something new", error.message)
        self.assertIn("'get_text'", error.message)

    def test_other_crashes_are_left_alone(self):
        self.assertIsNone(w.unreadable_page(self.crash_in("/src/fanficfare/crates/adapters/else.py"), w.PageLog()))
        self.assertIsNone(w.unreadable_page(RuntimeError("no"), w.PageLog()))


@unittest.skipUnless(HAS_FANFICFARE, "needs FanFicFare (the pinned venv)")
class PageGuardTest(unittest.TestCase):
    """The guard inside FanFicFare's real fetcher class, with the network replaced."""

    def setUp(self):
        from fanficfare.fetchers import base_fetcher

        w.install_page_guard()
        w.install_page_guard()  # idempotent
        self.served = []

        class Scripted(base_fetcher.Fetcher):
            def request(fetcher, method, url, headers=None, parameters=None):
                self.served.append(url)
                return base_fetcher.FetcherResponse(self.pages.pop(0), url)

        self.fetcher = Scripted(lambda key, default=None: default, lambda key: [])
        self.sleeps = []
        self.real_sleep, w.time.sleep = w.time.sleep, self.sleeps.append

    def tearDown(self):
        w.time.sleep = self.real_sleep

    def fetch(self, *pages):
        self.pages = list(pages)
        with contextlib.redirect_stdout(io.StringIO()):
            return self.fetcher.do_request("GET", WORK)

    def test_ao3_s_page_goes_through(self):
        self.assertEqual(self.fetch(AO3_WORK_QUOTING_ERRORS).content, AO3_WORK_QUOTING_ERRORS)
        self.assertEqual(w.PAGES.url, WORK)

    def test_the_bot_filter_becomes_fanficfare_s_own_http_429(self):
        from fanficfare import exceptions

        with self.assertRaises(exceptions.HTTPErrorFFF) as caught:
            self.fetch(AO3_BOT_FILTER, AO3_WORK_QUOTING_ERRORS)
        self.assertEqual(caught.exception.status_code, 429)
        self.assertEqual(w.classify_exception(caught.exception), w.RATE_LIMITED)
        self.assertEqual(len(self.served), 1, "a rate limit is not asked again at once")

    def test_a_few_525s_are_retried_through(self):
        response = self.fetch(CLOUDFLARE_525, CLOUDFLARE_525, AO3_WORK_QUOTING_ERRORS)
        self.assertEqual(response.content, AO3_WORK_QUOTING_ERRORS)
        self.assertEqual(self.sleeps, [1.0, 2.0])

    def test_525s_that_do_not_stop_are_an_outage(self):
        from fanficfare import exceptions

        with self.assertRaises(exceptions.HTTPErrorFFF) as caught:
            self.fetch(*[CLOUDFLARE_525] * (w.CLOUDFLARE_RETRIES + 1))
        self.assertEqual(caught.exception.status_code, 525)
        self.assertEqual(len(self.served), w.CLOUDFLARE_RETRIES + 1)

    def test_images_are_not_judged(self):
        self.pages = [b"\x89PNG not a page"]
        self.fetcher.do_request("GET", WORK, image=True)


if __name__ == "__main__":
    unittest.main()
