"""Optional browser acceptance suite. See README for the isolated command."""
from contextlib import closing
import os
from pathlib import Path
import sqlite3
import sys
import unittest
import uuid

from playwright.sync_api import sync_playwright, expect

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import test_service

class Panel(unittest.TestCase):
    def test_accounts_pagination_and_mobile(self):
        service = test_service.Service("test_web_authenticates_reads_and_mutations")
        service.claim = False
        self.addCleanup(service.doCleanups)
        service.setUp()
        port = service.port
        config = service.root / "panel-config"
        config.mkdir()
        with sync_playwright() as pw:
            browser = pw.chromium.launch(args=["--disable-dev-shm-usage"])
            page = browser.new_page(viewport={"width": 1240, "height": 850}, permissions=["clipboard-read", "clipboard-write"])
            errors = []
            page.on("pageerror", lambda e: errors.append(str(e)))
            page.route("**/api/v1/session",lambda route: route.fulfill(status=503,body=""),times=1)
            page.goto(f"http://127.0.0.1:{port}")
            page.get_by_role("button",name="Retry").click()
            page.get_by_label("New password",exact=True).fill(service.password)
            confirm=page.get_by_label("Confirm password",exact=True)
            confirm.fill("different password")
            page.get_by_role("button",name="Set password").click()
            expect(page.get_by_role("alert")).to_have_text("Passwords do not match.")
            confirm.fill(service.password)
            page.get_by_role("button",name="Set password").click()
            expect(page.get_by_role("heading",name="Startups")).to_be_visible()
            page.get_by_role("button",name="Sign out").click()
            page.get_by_label("Password",exact=True).fill(service.password)
            page.get_by_role("button",name="Sign in").click()
            expect(page.get_by_role("heading",name="Startups")).to_be_visible()
            _, session, headers = service.api("POST","login",{"password":service.password})
            service.session=session["session"]
            page.reload()
            expect(page.get_by_role("heading",name="Startups")).to_be_visible()
            page.get_by_role("navigation").get_by_text("Accounts").click()
            page.get_by_role("button",name="Add config").first.click()
            page.get_by_label("Label",exact=True).fill("Work")
            page.get_by_label("Existing config directory").fill(str(config))
            page.get_by_role("dialog").get_by_role("button",name="Add config").click()
            page.get_by_role("checkbox",name="Enabled").check()
            expect(page.get_by_role("button",name="Set current")).to_be_enabled()
            page.get_by_role("button",name="Set current").focus()
            page.keyboard.press("Enter")
            expect(page.locator(".account-row").get_by_text("Current")).to_be_visible()
            page.get_by_role("button",name="Edit config").click()
            page.get_by_label("Proxy mode").select_option("custom")
            page.get_by_label("HTTPS_PROXY",exact=True).fill("http://user:proxysecret@proxy.example:8080")
            page.get_by_role("button",name="Save changes").click()
            expect(page.locator(".detail")).to_contain_text("proxy.example")
            self.assertNotIn("proxysecret",page.locator("body").inner_text())
            page.on("dialog", lambda d: d.accept())
            page.get_by_role("checkbox",name="Enabled").click()
            expect(page.get_by_role("button",name="Set current")).to_be_disabled()
            self.assertEqual(service.api("GET","accounts?tool=claude")[1]["items"][0]["current"],False)
            self.snapshot(page,"accounts")
            page.set_viewport_size({"width":390,"height":844})
            self.snapshot(page,"accounts-mobile")
            self.assertLessEqual(page.evaluate("document.documentElement.scrollWidth"),390)
            page.set_viewport_size({"width":1240,"height":850})
            sql="INSERT INTO sessions(tool,native_id,owner,evidence,workspace,bound_ms,last_routed_ms) VALUES('claude',?,1,'assigned','/work',1,1)"
            with closing(sqlite3.connect(service.root/"state/cc-profile/router.sqlite3")) as db, db:
                db.executemany(sql,[(str(uuid.uuid4()),) for _ in range(60)])
                db.commit()
                page.get_by_role("navigation").get_by_text("Sessions").click()
                expect(page.locator("tbody tr")).to_have_count(25)
                first=page.locator("tbody .uuid").all_text_contents()
                page.get_by_role("button",name="Next page").click()
                expect(page.get_by_text("Page 2",exact=True)).to_be_visible()
                second=page.locator("tbody .uuid").all_text_contents()
                self.assertFalse(set(first)&set(second))
                db.execute(sql,(str(uuid.uuid4()),))
                db.commit()
                page.get_by_role("button",name="Refresh",exact=True).click()
                expect(page.locator("tbody .uuid").first).to_have_text(second[0])
                page.get_by_role("button",name="Copy resume command").first.click()
                expect(page.get_by_role("status")).to_contain_text("Copied")
                expect(page.get_by_role("cell",name="/work",exact=True).first).to_be_visible()
                self.assertTrue(page.evaluate("navigator.clipboard.readText()").startswith("cd '/work' && ccp claude --resume "))
                self.snapshot(page,"sessions")
            self.assertEqual(errors,[])
            browser.close()

    def snapshot(self,page,name):
        path=os.environ.get("CCP_SCREENSHOTS")
        if path:
            Path(path).mkdir(exist_ok=True)
            page.screenshot(path=str(Path(path)/(name+".png")))

if __name__=="__main__":
    unittest.main()
