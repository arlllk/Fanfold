"""The web UI in a real browser: what unit tests of the HTML cannot see.

    just test-ui-browser
    # or: cargo build -p fanfold-adapter-web --example demo && \\
    #     .venv/bin/python -I -B -m unittest discover -s crates/adapters/web/tests/browser

Runs the demo (the real UI over realistic in-memory data) on a free port and drives it with
Playwright: live updates connect, the search suggests as you type, picking a suggestion or a
filter edits the query, chips remove, tabs keep the search, the filter panel folds on a phone,
and nothing the Content-Security-Policy forbids is ever attempted. These are the moving parts
(htmx, Alpine, app.js) that a version bump of a vendored script could break silently.

The browser: $FANFOLD_BROWSER, else the first of chromium, google-chrome found on PATH.
"""

from __future__ import annotations

import os
import shutil
import socket
import re
import subprocess
import time
import unittest
import urllib.request
from pathlib import Path

from playwright.sync_api import Browser, ConsoleMessage, Page, Route, expect, sync_playwright

ROOT = Path(__file__).resolve().parents[5]
DEMO = Path(os.environ.get("FANFOLD_DEMO", ROOT / "target" / "debug" / "examples" / "demo"))
TIMEOUT_MS = 5_000


def browser_path() -> str:
    for candidate in (os.environ.get("FANFOLD_BROWSER"), "chromium", "google-chrome", "google-chrome-stable"):
        if candidate and (found := shutil.which(candidate)):
            return found
    raise unittest.SkipTest("no Chromium found; set FANFOLD_BROWSER")


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return int(s.getsockname()[1])


class BrowserTests(unittest.TestCase):
    demo: subprocess.Popen[bytes]
    base: str
    browser: Browser

    @classmethod
    def setUpClass(cls) -> None:
        if not DEMO.is_file():
            raise unittest.SkipTest(f"{DEMO} is not built: cargo build -p fanfold-adapter-web --example demo")
        port = free_port()
        cls.base = f"http://127.0.0.1:{port}"
        cls.demo = subprocess.Popen([str(DEMO), "--port", str(port)], stdout=subprocess.DEVNULL)
        deadline = time.monotonic() + 30
        while True:
            try:
                urllib.request.urlopen(cls.base + "/", timeout=1).close()
                break
            except OSError:
                if time.monotonic() > deadline or cls.demo.poll() is not None:
                    cls.demo.kill()
                    raise
                time.sleep(0.2)
        cls.playwright = sync_playwright().start()
        cls.browser = cls.playwright.chromium.launch(executable_path=browser_path())

    @classmethod
    def tearDownClass(cls) -> None:
        cls.browser.close()
        cls.playwright.stop()
        cls.demo.terminate()
        cls.demo.wait(timeout=10)

    def setUp(self) -> None:
        self.problems: list[str] = []
        self.page = self.browser.new_page()
        self.page.set_default_timeout(TIMEOUT_MS)

        def console(message: ConsoleMessage) -> None:
            if message.type in ("error", "warning"):
                self.problems.append(message.text)

        self.page.on("console", console)
        self.page.on("pageerror", lambda error: self.problems.append(str(error)))

    def tearDown(self) -> None:
        self.page.close()
        self.assertEqual(self.problems, [], "the browser reported problems (a CSP violation shows here)")

    def open(self, path: str) -> Page:
        self.page.goto(self.base + path)
        return self.page

    def books(self) -> int:
        return self.page.locator("ul.books > li").count()

    def test_live_updates_connect_on_every_page(self) -> None:
        for path in ["/", "/jobs", "/library", "/jobs/1"]:
            with self.subTest(path=path):
                page = self.open(path)
                expect(page.locator("html")).to_have_attribute("data-live", "on")

    def test_searching_suggests_and_a_picked_suggestion_filters_exactly(self) -> None:
        page = self.open("/library")
        before = self.books()
        box = page.locator("#q")
        box.click()
        box.press_sequentially("quil", delay=30)
        options = page.locator("#suggest [role=option]")
        expect(options.first).to_be_visible()
        expect(box).to_have_attribute("aria-expanded", "true")
        box.press("ArrowDown")
        expect(options.first).to_have_attribute("aria-selected", "true")
        expect(box).to_have_attribute("aria-activedescendant", "sug-0")
        box.press("Enter")
        expect(box).to_have_value('author:"quillandink" ')
        expect(page.locator(".filter-chip")).to_have_count(1)
        page.wait_for_url("**/library?q=author*")
        self.assertLess(self.books(), before)
        self.assertGreater(self.books(), 0)
        expect(page.locator("#suggest")).to_be_hidden()

    def test_the_moon_reader_link_shows_on_android_only(self) -> None:
        page = self.open("/library")
        book = page.locator("ul.books > li a").first.get_attribute("href")
        assert book is not None
        link = self.open(book).get_by_role("link", name="Open in Moon+ Reader")
        expect(link).to_be_hidden()

        android = self.browser.new_page(user_agent="Mozilla/5.0 (Android 16; Mobile; rv:146.0) Gecko/146.0 Firefox/146.0")
        try:
            android.on("pageerror", lambda error: self.problems.append(str(error)))
            android.goto(self.base + book)
            link = android.get_by_role("link", name="Open in Moon+ Reader")
            expect(link).to_be_visible()
            expect(link).to_have_attribute("href", re.compile(r"^fanfold://open\?path=[^/?#]+\.epub$"))
        finally:
            android.close()

    def test_escape_closes_the_suggestions(self) -> None:
        page = self.open("/library")
        box = page.locator("#q")
        box.click()
        box.press_sequentially("quil", delay=30)
        expect(page.locator("#suggest")).to_be_visible()
        box.press("Escape")
        expect(page.locator("#suggest")).to_be_hidden()

    def test_filters_and_chips_edit_the_query(self) -> None:
        page = self.open("/library?q=author%3A%22quillandink%22")
        box = page.locator("#q")
        page.locator("aside").get_by_role("link", name="Complete").click()
        expect(box).to_have_value('author:"quillandink" is:complete ')
        expect(page.locator(".filter-chip")).to_have_count(2)
        page.locator(".filter-chip").first.click()
        expect(box).to_have_value("is:complete ")
        expect(page.locator(".filter-chip")).to_have_count(1)
        page.get_by_role("link", name="Clear all").click()
        expect(box).to_have_value("")
        expect(page.locator(".filter-chip")).to_have_count(0)

    def test_a_live_refresh_never_cancels_a_search(self) -> None:
        # The live region re-fetches its own (old) address on every event. If that refresh
        # could replace a search still in flight, the page would fall back to the old results
        # while the box showed the new query. Hold the search, refresh, then let it through.
        page = self.open("/library?q=author%3A%22quillandink%22")
        held: list[Route] = []
        page.route(lambda url: "complete" in url, lambda route: held.append(route))
        page.locator("aside").get_by_role("link", name="Complete").click()
        while not held:
            page.wait_for_timeout(10)
        # `refresh from:body delay:200ms` is a timer; one of 300ms set after it always runs
        # later, so the refresh has been sent (or dropped) by the time this returns.
        page.evaluate("""() => {
            document.body.dispatchEvent(new Event('refresh'));
            return new Promise((done) => setTimeout(done, 300));
        }""")
        held[0].continue_()
        page.unroute_all()
        expect(page.locator(".filter-chip")).to_have_count(2)
        expect(page.locator("#q")).to_have_value('author:"quillandink" is:complete ')

    def test_tabs_keep_the_search(self) -> None:
        page = self.open("/library?q=harbor")
        page.get_by_role("link", name="Series").click()
        page.wait_for_url("**/library/series?q=harbor")
        expect(page.locator("#q")).to_have_value("harbor")
        page.get_by_role("link", name="Authors").click()
        page.wait_for_url("**/library/authors?q=harbor")

    def test_the_filter_panel_folds_away_on_a_phone(self) -> None:
        self.page.set_viewport_size({"width": 390, "height": 800})
        page = self.open("/library")
        expect(page.locator("#filters")).to_be_hidden()
        page.get_by_role("button", name="Filters").click()
        expect(page.locator("#filters")).to_be_visible()

    def test_a_typed_search_survives_a_live_refresh(self) -> None:
        page = self.open("/library")
        expect(page.locator("html")).to_have_attribute("data-live", "on")
        box = page.locator("#q")
        box.click()
        box.press_sequentially("rating:e is:", delay=30)
        # A live refresh of the results must never touch what is being typed.
        page.evaluate("document.body.dispatchEvent(new Event('refresh'))")
        page.wait_for_timeout(600)
        expect(box).to_have_value("rating:e is:")
        expect(box).to_be_focused()


if __name__ == "__main__":
    unittest.main()
