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
            context = browser.new_context(viewport={"width": 1240, "height": 850}, permissions=["clipboard-read", "clipboard-write"])
            page = context.new_page()
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
            tab=context.new_page()
            tab.goto(f"http://127.0.0.1:{port}")
            expect(tab.get_by_role("heading",name="Sign in")).to_be_visible()
            tab.close()
            page.get_by_role("navigation").get_by_text("Accounts").click()
            page.get_by_role("button",name="Add profile").first.click()
            page.get_by_label("Label",exact=True).fill("Work")
            page.get_by_label("Profile directory",exact=True).select_option("existing")
            page.get_by_label("Existing config directory").fill(str(config))
            page.get_by_role("dialog").get_by_role("button",name="Add profile").click()
            enabled = page.get_by_role("checkbox",name="Enable profile",exact=True)
            expect(enabled).to_have_accessible_description("Disabling this profile blocks new chats and resumes. Chats already running are not stopped.")
            enabled.check()
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
            enabled.click()
            expect(page.get_by_role("button",name="Set current")).to_be_disabled()
            self.assertEqual(service.api("GET","accounts?tool=claude")[1]["items"][0]["current"],False)
            page.get_by_role("button",name="Add profile").first.click()
            page.get_by_label("Label",exact=True).fill("Personal")
            expect(page.get_by_label("Existing config directory")).to_be_hidden()
            page.get_by_role("dialog").get_by_role("button",name="Add profile").click()
            expect(page.get_by_role("heading",name="Personal",exact=True)).to_be_visible()
            page.get_by_role("button",name="Copy Claude sign-in command").click()
            self.assertEqual(page.evaluate("navigator.clipboard.readText()"),"ccp login 2")
            expect(enabled).not_to_be_checked()
            page.get_by_role("button",name="Codex",exact=True).click()
            page.get_by_role("button",name="Add profile").first.click()
            page.get_by_label("Label",exact=True).fill("Codex Personal")
            page.get_by_role("dialog").get_by_role("button",name="Add profile").click()
            page.get_by_role("button",name="Sign in with ChatGPT").click()
            verification = page.get_by_role("link",name="Open OpenAI verification")
            expect(verification).to_have_attribute("href","https://auth.openai.com/codex/device")
            # Hold a poll for the first attempt until its replacement has a code.
            url = "**/accounts/*/device-login"
            held = []
            with page.expect_request(lambda r: r.method == "GET" and r.url.endswith("/device-login")):
                page.route(url, lambda route: held.append(route), times=1)
            page.get_by_role("button",name="Cancel sign-in").click()
            expect(page.get_by_text("Stopped waiting for sign-in.",exact=True)).to_be_visible()
            page.get_by_role("button",name="Sign in with ChatGPT").click()
            expect(verification).to_be_visible()
            held[0].fulfill(json={"state":"cancelled","attempt":"old"})
            codex = service.api("GET","accounts?tool=codex")[1]["items"][0]
            finish = Path(codex["home"])/"finish-login"
            finish.touch()
            completed = page.get_by_text("Device sign-in completed for Codex Personal.")
            expect(completed).to_be_visible()
            finish.unlink()
            # A delayed full Accounts refresh must not replace a newer attempt either.
            old_panel = page.get_by_role("heading",name="Profile sign-in").element_handle()
            held.clear()
            with page.expect_request(lambda r: r.method == "GET" and r.url.endswith("/device-login")):
                page.route(url, lambda route: held.append(route), times=1)
                page.get_by_role("button",name="Refresh enabled quotas").click()
            page.get_by_role("button",name="Sign in with ChatGPT").click()
            expect(verification).to_be_visible()
            held[0].fulfill(json={"state":"completed","attempt":"old"})
            page.wait_for_function("node => !node.isConnected", arg=old_panel)
            expect(verification).to_be_visible()
            page.reload()
            page.get_by_role("button",name="Copy device code").click()
            self.assertEqual(page.evaluate("navigator.clipboard.readText()"),"TEST-ONLY")
            finish.touch()
            expect(completed).to_be_visible()
            expect(verification).to_have_count(0)
            expect(page.get_by_role("button",name="Check observer")).to_be_visible()
            expect(enabled).not_to_be_checked()
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
