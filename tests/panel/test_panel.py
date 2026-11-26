"""Optional browser acceptance suite. See README for the isolated command."""
from contextlib import closing
import json
import os
from pathlib import Path
import re
import shlex
import sqlite3
import sys
import unittest
import uuid

from playwright.sync_api import sync_playwright, expect

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import test_service

class Panel(unittest.TestCase):
    def start_service(self, claim=True):
        service = test_service.Service("test_web_authenticates_reads_and_mutations")
        service.claim = claim
        service.password = "1234"
        self.addCleanup(service.doCleanups)
        service.setUp()
        return service

    def test_no_auth_opens_settings_without_password_controls(self):
        service = self.start_service(claim=False)
        service.stop()
        service.start("--no-auth", warning="WARNING: --no-auth")
        with sync_playwright() as pw, pw.chromium.launch(args=["--disable-dev-shm-usage"]) as browser:
            page = browser.new_page()
            page.goto(f"http://127.0.0.1:{service.port}/#settings")
            mode = page.get_by_label("Routing mode")
            expect(mode).to_be_visible()
            expect(page.locator("input[type=password]")).to_have_count(0)
            expect(page.get_by_role("button", name="Sign out")).to_be_hidden()
            mode.select_option("priority")
            page.get_by_role("button", name="Save routing mode").click()
            expect(page.get_by_text("Routing mode saved.")).to_be_visible()
            page.reload()
            expect(mode).to_have_value("priority")

    def test_tab_navigation_shows_loading_until_the_selected_view_is_ready(self):
        service = self.start_service()
        with sync_playwright() as pw, pw.chromium.launch(args=["--disable-dev-shm-usage"]) as browser:
            page = browser.new_page()
            page.goto(f"http://127.0.0.1:{service.port}/#settings")
            page.get_by_label("Password", exact=True).fill(service.password)
            page.get_by_role("button", name="Sign in").click()
            expect(page.get_by_label("Routing mode")).to_be_visible()

            sessions, startups = [], []
            page.route("**/api/v1/sessions?*", lambda route: sessions.append(route))
            page.route("**/api/v1/startups?*", lambda route: startups.append(route), times=1)
            with page.expect_request("**/api/v1/sessions?*"):
                page.get_by_role("link", name="Sessions", exact=True).click()
            spinner = page.get_by_role("status", name="Loading Sessions")
            for width in (1240, 390):
                page.set_viewport_size({"width":width, "height":844})
                expect(spinner).to_be_visible()
                box = spinner.bounding_box()
                self.assertAlmostEqual(box["x"] + box["width"] / 2, width / 2, delta=1)
            expect(page.get_by_label("Routing mode")).to_have_count(0)

            with page.expect_request("**/api/v1/startups?*"):
                page.get_by_role("link", name="Dashboard", exact=True).click()
            with page.expect_response("**/api/v1/sessions?*"):
                sessions[0].continue_()
            expect(page.get_by_role("status", name="Loading Dashboard")).to_be_visible()
            startups[0].fulfill(status=503, json={"message":"Router busy"})
            expect(page.get_by_role("main")).to_contain_text("Router busy")
            page.get_by_role("button", name="Retry").click()
            expect(page.get_by_role("region", name="Account quotas")).to_be_visible()
            expect(page.locator(".router-loading")).to_have_count(0)

            settings = []
            page.route("**/api/v1/settings", lambda route: settings.append(route), times=1)
            with page.expect_request("**/api/v1/settings"):
                page.get_by_role("link", name="Settings", exact=True).click()
            expect(page.get_by_role("status", name="Loading Settings")).to_be_visible()
            expect(page.get_by_role("region", name="Account quotas")).to_have_count(0)
            settings[0].continue_()
            expect(page.get_by_label("Routing mode")).to_be_visible()

    def test_accounts_provider_change_shows_loading_but_not_on_refresh(self):
        service = self.start_service()
        service.account("claude", "Previous profile")
        with sync_playwright() as pw, pw.chromium.launch(args=["--disable-dev-shm-usage"]) as browser:
            page = browser.new_page()
            page.goto(f"http://127.0.0.1:{service.port}/#accounts?tool=claude")
            page.get_by_label("Password", exact=True).fill(service.password)
            page.get_by_role("button", name="Sign in").click()
            expect(page.locator(".account-row", has_text="Previous profile")).to_be_visible()
            held = []
            endpoint = "**/api/v1/accounts?tool=codex"
            page.route(endpoint, lambda route: held.append(route), times=1)
            with page.expect_request(endpoint):
                page.get_by_role("button", name="Codex", exact=True).click()
            expect(page.get_by_role("status", name="Loading Codex accounts")).to_be_visible()
            expect(page.get_by_role("button", name="Codex", exact=True)).to_have_attribute("aria-pressed", "true")
            expect(page.locator(".account-row")).to_have_count(0)
            held.pop().continue_()
            empty = page.get_by_role("heading", name="No Codex profiles")
            expect(empty).to_be_visible()
            page.route(endpoint, lambda route: held.append(route), times=1)
            with page.expect_request(endpoint):
                page.get_by_role("button", name="Refresh enabled quotas").click()
            expect(empty).to_be_visible()
            expect(page.locator(".router-loading")).to_have_count(0)
            held.pop().continue_()

    def test_accounts_pagination_and_mobile(self):
        service = self.start_service(claim=False)
        port = service.port
        config = service.root / "panel-config"
        config.mkdir()
        with sync_playwright() as pw:
            browser = pw.chromium.launch(args=["--disable-dev-shm-usage"])
            context = browser.new_context(viewport={"width": 1240, "height": 850}, permissions=["clipboard-read", "clipboard-write"])
            page = context.new_page()
            errors = []
            page.on("pageerror", lambda e: errors.append(str(e)))
            held = []
            page.route("**/api/v1/session", lambda route: held.append(route), times=1)
            with page.expect_request("**/api/v1/session"):
                page.goto(f"http://127.0.0.1:{port}", wait_until="commit")
            loading = page.get_by_role("status", name="Connecting to router")
            for width in (1240, 390):
                page.set_viewport_size({"width": width, "height": 844})
                expect(loading).to_be_visible()
                box = loading.bounding_box()
                self.assertAlmostEqual(box["x"] + box["width"] / 2, width / 2, delta=1)
            held[0].fulfill(status=503, body="")
            page.get_by_role("button",name="Retry").click()
            new_password=page.get_by_label("New password",exact=True)
            expect(new_password).to_have_accessible_description("Use at least 4 characters.")
            new_password.fill(service.password)
            confirm=page.get_by_label("Confirm password",exact=True)
            confirm.fill("different password")
            page.get_by_role("button",name="Set password").click()
            expect(page.get_by_role("alert")).to_have_text("Passwords do not match.")
            self.check_layout(page,"setup-mobile")
            confirm.fill(service.password)
            page.get_by_role("button",name="Set password").click()
            expect(page.get_by_role("main",name="Dashboard")).to_be_visible()
            expect(page.get_by_role("region",name="Account quotas")).to_contain_text("No profiles yet")
            page.get_by_role("button",name="Sign out").click()
            page.get_by_label("Password",exact=True).fill(service.password)
            page.get_by_role("button",name="Sign in").click()
            expect(page.get_by_role("main",name="Dashboard")).to_be_visible()
            _, session, headers = service.api("POST","login",{"password":service.password})
            service.session=session["session"]
            page.reload()
            expect(page.get_by_role("main",name="Dashboard")).to_be_visible()
            tab=context.new_page()
            tab.goto(f"http://127.0.0.1:{port}")
            expect(tab.get_by_role("heading",name="Sign in")).to_be_visible()
            tab.close()
            page.get_by_role("navigation").get_by_text("Accounts").click()
            expect(page.get_by_role("button",name="Compatibility links")).to_have_count(0)
            page.set_viewport_size({"width":1240,"height":850})
            page.get_by_role("button",name="Add profile").click()
            page.get_by_label("Label",exact=True).fill("Work")
            page.get_by_label("Profile directory",exact=True).select_option("existing")
            page.get_by_label("Existing config directory").fill(str(config))
            page.get_by_role("dialog").get_by_role("button",name="Add profile").click()
            enabled = page.get_by_role("checkbox",name="Enable profile",exact=True)
            expect(enabled).to_have_accessible_description("Disabling this profile blocks new chats and resumes. Chats already running are not stopped.")
            enabled.check()
            expect(page.get_by_role("button",name="Set as default")).to_be_enabled()
            page.get_by_role("button",name="Set as default").focus()
            page.keyboard.press("Enter")
            expect(page.locator(".account-row").get_by_text("Default", exact=True)).to_be_visible()
            page.get_by_role("button",name="Clear default account").click()
            expect(page.locator(".account-row").get_by_text("Default", exact=True)).to_have_count(0)
            page.get_by_role("button",name="Set as default").click()
            expect(page.get_by_role("button",name="Clear default account")).to_be_visible()
            moved = service.root/"moved panel config"
            config.rename(moved)
            page.get_by_role("button",name="Edit directory").click()
            directory = page.get_by_role("dialog",name="Edit directory").get_by_label("Directory",exact=True)
            expect(directory).to_be_focused()
            expect(directory).to_have_value(str(config))
            wrong = service.root/"another home"
            wrong.mkdir()
            directory.fill(str(wrong))
            page.get_by_role("button",name="Save changes").click()
            expect(page.get_by_role("alert")).to_contain_text("original account directory")
            directory.fill(str(moved))
            page.get_by_role("button",name="Save changes").click()
            expect(page.get_by_role("dialog")).to_be_hidden()
            expect(page.locator(".detail")).to_contain_text(str(moved))
            page.get_by_role("button", name="Copy directory", exact=True).click()
            self.assertEqual(page.evaluate("navigator.clipboard.readText()"), str(moved))
            config = moved
            page.get_by_role("button",name="Edit config").click()
            page.get_by_label("Proxy mode").select_option("custom")
            proxy = page.get_by_role("textbox",name="Proxy URL",exact=True)
            expect(proxy).to_have_count(1)
            expect(proxy).to_have_attribute("type","text")
            proxy.fill("http://user:proxysecret@proxy.example:8080")
            page.get_by_role("button",name="Save changes").click()
            expect(page.locator(".detail")).to_contain_text("proxy.example")
            self.assertNotIn("proxysecret",page.locator("body").inner_text())
            page.get_by_role("button",name="Edit config").click()
            expect(proxy).to_have_value("")
            expect(proxy).to_have_accessible_description("Leave blank to keep the saved proxy. Switch to Direct or Inherit to clear it.")
            self.check_layout(page,"edit-proxy")
            page.get_by_role("button",name="Save changes").click()
            expect(page.locator(".detail")).to_contain_text("proxy.example")
            page.get_by_role("button",name="Edit config").click()
            page.get_by_label("Proxy mode").select_option("direct")
            page.get_by_role("button",name="Save changes").click()
            expect(page.locator(".detail")).not_to_contain_text("proxy.example")
            page.get_by_role("button",name="Edit config").click()
            page.get_by_label("Proxy mode").select_option("custom")
            page.get_by_role("button",name="Save changes").click()
            expect(page.get_by_role("alert")).to_have_text("Set a proxy URL for Custom mode.")
            proxy.fill("http://other.invalid:3128")
            page.get_by_role("button",name="Save changes").click()
            expect(page.get_by_role("dialog")).to_be_hidden()
            expect(page.locator(".detail")).not_to_contain_text("proxy.example")
            page.on("dialog", lambda d: d.accept())
            enabled.click()
            expect(page.get_by_role("button",name="Set as default")).to_be_disabled()
            self.assertEqual(service.api("GET","accounts?tool=claude")[1]["items"][0]["current"],False)
            (config/".credentials.json").write_text("fixture login")
            page.get_by_role("button",name="Refresh quota",exact=True).click()
            expect(page.locator(".detail")).to_contain_text("CLI checked")
            expect(page.get_by_role("progressbar").first).to_have_attribute("value","75")
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
            status = page.get_by_role("status", name="Sign-in status")
            expect(status).to_have_text("Not signed in")
            expect(status.locator("svg")).to_have_count(0)
            finish.unlink()
            # A delayed full Accounts refresh must not replace a newer attempt either.
            old_panel = page.get_by_role("heading",name="Sign-in status").element_handle()
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
            expect(page.get_by_role("button",name="Install observer")).to_have_count(0)
            page.get_by_role("button",name="Copy Codex sign-in command").click()
            self.assertEqual(page.evaluate("navigator.clipboard.readText()"), "ccp login " + str(codex["id"]))
            expect(page.get_by_text("Ready",exact=True)).to_be_visible()
            page.get_by_role("button",name="Refresh quota",exact=True).click()
            expect(page.get_by_role("progressbar").first).to_have_attribute("value","100")
            expect(page.locator(".quota time").first).to_have_attribute("datetime","2033-05-18T03:33:20.000Z")
            accounts = service.api("GET","accounts?tool=codex")[1]
            accounts["items"][0]["quota"].update(stale=True, status="rate_limited")
            page.route("**/accounts?tool=codex", lambda route: route.fulfill(json=accounts), times=1)
            page.get_by_role("button",name="Refresh enabled quotas").click()
            quota = page.locator("section").filter(has=page.get_by_role("heading",name="Quota & resets"))
            for value in ("Stale", "Rate limited", "Last checked", "100% left"):
                expect(quota).to_contain_text(value)
            expect(enabled).not_to_be_checked()
            self.check_layout(page,"accounts")
            page.set_viewport_size({"width":390,"height":844})
            self.check_layout(page,"accounts-mobile")
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
                self.check_layout(page,"sessions")
            self.assertEqual(errors,[])
            browser.close()

    def test_antigravity_profile_login_and_inline_quota(self):
        service=self.start_service()
        _, c, _=service.api("POST","accounts",{"tool":"agy","label":"ACP work","create":True})
        home = Path(c["home"])
        cache = service.root/"state/cc-profile/router.sqlite3"
        with sync_playwright() as pw, pw.chromium.launch(args=["--disable-dev-shm-usage"]) as browser:
            page=browser.new_page(viewport={"width":390,"height":844},permissions=["clipboard-read","clipboard-write"])
            probes = []
            endpoint = f"/accounts/{c['id']}/identity"
            page.on("request", lambda r: probes.append(r.method) if r.url.endswith(endpoint) else None)
            page.goto(f"http://127.0.0.1:{service.port}/#accounts?tool=agy")
            page.get_by_label("Password",exact=True).fill(service.password)
            page.get_by_role("button",name="Sign in").click()
            expect(page.get_by_role("heading",name="ACP work",exact=True)).to_be_visible()
            expect(page.get_by_text("No saved login", exact=True)).to_be_visible()
            status = page.get_by_role("status", name="Sign-in status")
            check = status.locator("use[href='/icons.svg#check']")
            expect(check).to_have_count(0)
            self.assertNotIn("POST", probes)
            verify = page.get_by_role("button", name="Refresh sign-in status")
            verify.click()
            expect(page.get_by_text("Sign-in required", exact=True)).to_be_visible()
            expect(check).to_have_count(0)
            self.assertEqual(service.cli("login",str(c["id"])).returncode,0)
            page.reload()
            expect(status).to_have_text("Saved login found (validity not checked)")
            expect(check).to_have_count(0)
            self.assertEqual(probes.count("POST"), 1)
            verify.click()
            expect(status).to_have_text("Catalog accessible")
            expect(check).to_be_visible()
            page.evaluate("window.dispatchEvent(new Event('focus'))")
            held = []
            page.route("**" + endpoint, lambda r: held.append(r), times=1)
            with page.expect_request(lambda r: r.method == "POST" and r.url.endswith(endpoint)):
                verify.click()
            expect(verify).to_have_attribute("aria-busy", "true")
            expect(status).to_have_text("Catalog accessible")
            held[0].fulfill(json={"state":"unavailable","email":None})
            expect(page.get_by_text("Couldn’t verify catalog access", exact=True)).to_be_visible()
            expect(check).to_have_count(0)
            self.assertEqual(probes.count("POST"), 3)
            expect(page.locator(".account-list").get_by_role("progressbar")).to_have_count(0)
            expect(page.get_by_role("button",name="Antigravity",exact=True)).to_have_attribute("aria-pressed","true")
            page.get_by_role("button",name="Copy Antigravity sign-in command").click()
            self.assertEqual(page.evaluate("navigator.clipboard.readText()"),"ccp login "+str(c["id"]))
            expect(page.get_by_role("checkbox",name="Enable profile")).not_to_be_checked()
            expect(page.get_by_role("button",name="Sign in with ChatGPT")).to_have_count(0)
            page.get_by_role("checkbox",name="Enable profile").check()
            expect(page.locator(".account-row")).to_contain_text("Enabled")
            (home/"hold-report").touch()
            page.get_by_role("navigation").get_by_text("Dashboard").click()
            card = page.locator(".quota-card")
            refresh = card.get_by_role("button", name="Refresh quota")
            expect(page.get_by_role("button", name="Refresh enabled quotas")).to_be_enabled()
            expect(card).to_contain_text("Checking quota")
            expect(refresh).to_be_disabled()
            (home/"hold-report").unlink()
            expect(card).to_contain_text("Model-pool limits")
            expect(card).to_contain_text("Last checked")
            expect(card.get_by_role("progressbar", name="Gemini Models, Weekly remaining")).to_have_attribute("value","76")
            expect(card.get_by_role("progressbar", name="Claude and GPT models, Weekly remaining")).to_have_attribute("value","0")
            empty = card.locator(".quota-value").filter(has=page.locator("progress[value='0']"))
            expect(empty).to_have_text("0% left")
            exhausted_color = empty.evaluate("n => getComputedStyle(n).color")
            expect(card.locator("time")).to_have_attribute("datetime","2030-01-01T00:00:00.000Z")
            expect(card.get_by_text("Reset unknown", exact=True)).to_have_count(2)
            expect(page.get_by_role("dialog")).to_be_hidden()
            page.set_viewport_size({"width":320, "height":720})
            self.check_layout(page,"agy-quota-mobile")
            with closing(sqlite3.connect(cache)) as db, db:
                db.execute("UPDATE quota_cache SET success_ms=success_ms-300001,retry_ms=0")
            (home/"report.json").write_text("fixture-private, not a usage result")
            page.get_by_role("link",name="Antigravity / ACP work").click()
            expect(card).to_contain_text("Stale · Unavailable")
            expect(card).to_contain_text("Retry after")
            expect(card.get_by_role("progressbar")).to_have_count(3)
            expect(card.locator(".stale-meter")).to_have_count(3)
            self.assertNotEqual(empty.evaluate("n => getComputedStyle(n).color"), exhausted_color)
            expect(page.locator(".account-row")).to_contain_text("Stale")
            expect(page.locator(".account-row").get_by_role("progressbar")).to_have_count(2)
            expect(page.locator("body")).not_to_contain_text("fixture-private")
            (home/"report.json").unlink()
            with closing(sqlite3.connect(cache)) as db, db:
                db.execute("UPDATE quota_cache SET retry_ms=0")
            (home/"hold-report").touch()
            refresh.press("Enter")
            expect(refresh).to_have_attribute("aria-busy", "true")
            expect(card).to_contain_text("Stale · Unavailable")
            expect(card.get_by_role("progressbar", name="Gemini Models, Weekly remaining")).to_have_attribute("value", "76")
            (home/"hold-report").unlink()
            expect(card).not_to_contain_text("Stale")
            expect(refresh).to_have_attribute("aria-busy", "false")
            expect(card).not_to_contain_text("Checking quota")
            expect(card.get_by_role("progressbar")).to_have_count(3)
            self.assertEqual(len((home/"usage-calls").read_text().splitlines()), 3)
            page.set_viewport_size({"width":1240,"height":850})
            self.check_layout(page,"agy-quota-desktop")
            sid = "acp:it's here"
            tui_id = "707996ed-9668-43f7-b4d8-bc92e2021fe1"
            both_id = str(uuid.uuid4())
            discovered, discovered_after_acp = str(uuid.uuid4()), str(uuid.uuid4())
            with closing(sqlite3.connect(service.root/"state/cc-profile/router.sqlite3")) as db, db:
                db.execute("INSERT INTO sessions(tool,native_id,owner,evidence,workspace,bound_ms,last_routed_ms) "
                           "VALUES('agy',?,?,'observed','/my workspace',1,1)",(sid,c["id"]))
                db.execute("INSERT INTO sessions(tool,native_id,owner,evidence,workspace,bound_ms,last_routed_ms) "
                           "VALUES('agy',?,?,'saved_file','/a tui workspace',2,2)",(tui_id,c["id"]))
                db.execute("INSERT INTO sessions(tool,native_id,owner,evidence,workspace,tui_workspace,bound_ms,last_routed_ms) "
                           "VALUES('agy',?,?,'observed','/dual acp','/dual tui',3,4)",(both_id,c["id"]))
                db.execute("INSERT INTO sessions(tool,native_id,owner,evidence,bound_ms,saved_file_seen_ms) "
                           "VALUES('agy',?,?,'saved_file',5,5)",(discovered,c["id"]))
                db.execute("INSERT INTO sessions(tool,native_id,owner,evidence,workspace,bound_ms,last_routed_ms,saved_file_seen_ms) "
                           "VALUES('agy',?,?,'observed','/acp known',6,6,7)",(discovered_after_acp,c["id"]))
                db.execute("INSERT INTO sessions(tool,native_id,owner,evidence,workspace,bound_ms) "
                           "VALUES('agy','catalog-only',?,'acp_listed','/from catalog',8)",(c["id"],))
            page.get_by_role("navigation").get_by_text("Sessions").click()
            listed = page.get_by_role("row").filter(has_text="Listed by ACP")
            expect(listed).to_contain_text("Not routed")
            listed.get_by_role("button",name="Copy resume command").click()
            self.assertEqual(shlex.split(page.evaluate("navigator.clipboard.readText()")),
                             ["cd","/from catalog","&&","ccp","agy","acp","--for-session","catalog-only"])
            expect(page.get_by_role("button",name=sid,exact=True)).to_be_visible()
            page.get_by_role("row").filter(has_text="/my workspace").get_by_role("button",name="Copy resume command").click()
            self.assertEqual(shlex.split(page.evaluate("navigator.clipboard.readText()")),
                             ["cd","/my workspace","&&","ccp","agy","acp","--for-session",sid])
            page.get_by_role("row").filter(has_text="/a tui workspace").get_by_role("button",name="Copy resume command").click()
            self.assertEqual(shlex.split(page.evaluate("navigator.clipboard.readText()")),
                             ["cd","/a tui workspace","&&","ccp","agy","--conversation="+tui_id])
            both = page.get_by_role("row").filter(has_text="/dual acp")
            for label, args in [("Copy ACP resume command",["/dual acp","&&","ccp","agy","acp","--for-session",both_id]),
                                ("Copy terminal resume command",["/dual tui","&&","ccp","agy","--conversation="+both_id])]:
                both.get_by_role("button",name=label).click()
                self.assertEqual(shlex.split(page.evaluate("navigator.clipboard.readText()")),["cd",*args])
            for needle, label, native in [("Unknown","Copy resume command",discovered),
                                          ("/acp known","Copy terminal resume command",discovered_after_acp)]:
                row = page.get_by_role("row").filter(has_text=needle)
                if native == discovered:
                    expect(row).to_contain_text("Not routed")
                row.get_by_role("button", name=label).click()
                self.assertEqual(shlex.split(page.evaluate("navigator.clipboard.readText()")),
                                 ["ccp","agy","--conversation="+native])
            self.assertEqual(service.cli("agy").returncode, 0)
            page.get_by_role("navigation").get_by_text("Dashboard").click()
            logs = page.get_by_role("region", name="Startup logs")
            expect(logs.get_by_role("cell", name="Unknown", exact=True)).to_be_visible()
            expect(logs.get_by_role("cell", name="Unknown", exact=True).get_by_role("button")).to_have_count(0)

    def test_codex_listed_workspace_and_resume_command(self):
        service = self.start_service()
        profile = service.account("codex", "work")
        thread = str(uuid.uuid4())
        cwd = "/native/workspace's name"
        with closing(sqlite3.connect(service.root/"state/cc-profile/router.sqlite3")) as db, db:
            db.execute("INSERT INTO sessions(tool,native_id,owner,evidence,workspace,bound_ms) "
                       "VALUES('codex',?,?,'codex_listed',?,1)", (thread, profile["id"], cwd))
        with sync_playwright() as pw, pw.chromium.launch(args=["--disable-dev-shm-usage"]) as browser:
            page = browser.new_page(permissions=["clipboard-read", "clipboard-write"])
            page.goto(f"http://127.0.0.1:{service.port}/#sessions")
            page.get_by_label("Password", exact=True).fill(service.password)
            page.get_by_role("button", name="Sign in", exact=True).click()
            page.get_by_role("navigation").get_by_text("Sessions").click()
            row = page.get_by_role("row").filter(has_text="Listed by Codex")
            expect(row).to_contain_text("Not routed")
            expect(row).to_contain_text(cwd)
            row.get_by_role("button", name="Copy resume command").click()
            self.assertEqual(shlex.split(page.evaluate("navigator.clipboard.readText()")),
                             ["cd", cwd, "&&", "ccp", "codex", "resume", thread])

    def test_antigravity_session_id_copy_over_http(self):
        service = self.start_service()
        status, profile, _ = service.api("POST","accounts",{"tool":"agy","label":"Offline","create":True})
        self.assertEqual(status, 201)
        uid = "acp:fixture session's ID?x=1&y=2"
        with closing(sqlite3.connect(service.root/"state/cc-profile/router.sqlite3")) as db, db:
            db.execute("INSERT INTO sessions(tool,native_id,owner,evidence,workspace,bound_ms,last_routed_ms) "
                       "VALUES('agy',?,?,'observed','/work',1,1)", (uid, profile["id"]))
            db.execute("INSERT INTO launches(request_id,tool,intent,state,reason,native_id,config_id,"
                       "workspace,digest,ticket_digest,created_ms) VALUES('pinned','agy','resume','routed',"
                       "'sticky',?,?,'/work',X'01',X'01',1)", (uid, profile["id"]))
        with sync_playwright() as pw, pw.chromium.launch(args=["--disable-dev-shm-usage", "--no-proxy-server",
                "--host-resolver-rules=MAP ccp.test 127.0.0.1"]) as browser:
            page = browser.new_page()
            page.goto(f"http://ccp.test:{service.port}/#dashboard")
            page.get_by_label("Password", exact=True).fill(service.password)
            page.get_by_role("button", name="Sign in", exact=True).click()
            session_id = page.get_by_title(uid, exact=True)
            session_id.click()
            expect(page.get_by_role("main", name="Sessions", exact=True)).to_be_visible()
            expect(page.get_by_role("searchbox", name="Session ID")).to_have_value(uid)
            self.assertFalse(page.evaluate("window.isSecureContext || !!navigator.clipboard"))
            page.evaluate("""document.addEventListener('copy', () => {
                window.copied = document.activeElement.value;
            })""")
            session_id.click()
            expect(page.locator("#notice")).to_have_text("Copied to clipboard.")
            self.assertEqual(page.evaluate("window.copied"), uid)
            expect(session_id).to_be_focused()
            page.evaluate("""document.execCommand = () => false""")
            session_id.click()
            manual = page.get_by_role("textbox", name="Text to copy")
            expect(manual).to_be_focused()
            self.assertEqual(manual.evaluate("e => e.value.slice(e.selectionStart, e.selectionEnd)"), uid)
            page.get_by_role("button", name="Done", exact=True).click()
            page.evaluate("""Object.defineProperty(navigator, 'clipboard', {value: {
                writeText: () => Promise.reject(new Error('Permission denied'))
            }})""")
            page.get_by_role("button", name="Copy resume command", exact=True).click()
            expect(manual).to_be_visible()
            self.assertEqual(shlex.split(manual.input_value()), ["cd","/work","&&","ccp","agy","acp","--for-session",uid])

    def test_copy_directory_without_https(self):
        service = self.start_service()
        account = service.account("claude", "Work's profile")
        with sync_playwright() as pw, pw.chromium.launch(args=["--disable-dev-shm-usage", "--no-proxy-server",
                "--host-resolver-rules=MAP ccp.test 127.0.0.1"]) as browser:
            page = browser.new_page(viewport={"width": 320, "height": 740})
            page.goto(f"http://ccp.test:{service.port}/#accounts")
            page.get_by_label("Password", exact=True).fill(service.password)
            page.get_by_role("button", name="Sign in", exact=True).click()
            self.assertFalse(page.evaluate("window.isSecureContext || !!navigator.clipboard"))
            copy = page.get_by_role("button", name="Copy directory", exact=True)
            copy.click()
            expect(page.locator("#notice")).to_have_text("Copied to clipboard.")
            page.get_by_role("button", name="Edit directory").click()
            path = page.get_by_role("dialog").get_by_label("Directory", exact=True)
            path.fill("")
            path.press("Control+v")
            expect(path).to_have_value(account["home"])
            page.get_by_role("button", name="Cancel", exact=True).click()

            page.evaluate("document.execCommand = () => false")
            copy.click()
            manual = page.get_by_role("dialog", name="Copy manually").get_by_role("textbox")
            expect(manual).to_be_focused()
            self.assertEqual(manual.evaluate("e => e.value.slice(e.selectionStart, e.selectionEnd)"), account["home"])
            page.get_by_role("button", name="Done").click()

            for view in ("Dashboard", "Sessions", "Settings", "Accounts"):
                link = page.get_by_role("link", name=view, exact=True)
                link.click()
                expect(link).to_have_attribute("aria-current", "page")
                expect(page.get_by_role("main", name=view)).to_be_visible()
                expect(page.get_by_role("heading", name=view, exact=True)).to_have_count(0)

    def test_all_accounts_select_and_edit_with_the_right_provider(self):
        service = self.start_service()
        for t in ("claude", "codex", "agy"):
            self.assertEqual(service.api("POST", "accounts", {"tool":t,"create":True,"label":"Work"})[0], 201)
        with sync_playwright() as pw, pw.chromium.launch(args=["--disable-dev-shm-usage"]) as browser:
            page = browser.new_page(viewport={"width":320,"height":720})
            page.goto(f"http://127.0.0.1:{service.port}/#accounts")
            page.get_by_label("Password", exact=True).fill(service.password)
            page.get_by_role("button", name="Sign in", exact=True).click()
            tabs = page.get_by_role("group", name="Provider")
            expect(tabs.get_by_role("button")).to_have_text(["All", "Claude", "Codex", "Antigravity"])
            all_tab = tabs.get_by_role("button", name="All", exact=True)
            expect(all_tab).to_have_attribute("aria-pressed","true")
            rows = page.locator(".account-row")
            expect(rows).to_have_count(3)
            expect(rows.locator(".provider-name")).to_have_text(["Claude","Codex","Antigravity"])
            for row in rows.all():
                provider = row.locator(".provider-name").bounding_box()
                self.assertLessEqual(provider["y"] + provider["height"], row.locator("strong").bounding_box()["y"])
            expect(page.get_by_text("codex-cli", exact=False)).to_have_count(0)
            rows.filter(has_text="Codex").click()
            expect(page.get_by_role("button", name="Sign in with ChatGPT")).to_be_visible()
            expect(all_tab).to_have_attribute("aria-pressed","true")
            rows.filter(has_text="Antigravity").click()
            expect(page.get_by_role("button", name="Copy Antigravity sign-in command")).to_be_visible()
            expect(page.locator(".detail-heading .provider-name")).to_have_text("Antigravity")
            page.get_by_role("checkbox", name="Enable profile").check()
            page.get_by_role("button", name="Set as default").click()
            expect(rows.filter(has_text="Antigravity")).to_contain_text("Default")
            self.assertTrue(service.api("GET", "accounts?tool=agy")[1]["items"][0]["current"])
            page.get_by_role("button", name="Edit config").click()
            page.get_by_label("Label", exact=True).fill("Renamed")
            page.get_by_role("button", name="Save changes").click()
            expect(rows.filter(has_text="Antigravity")).to_contain_text("Renamed")
            expect(all_tab).to_have_attribute("aria-pressed","true")
            page.get_by_role("button", name="Add profile", exact=True).click()
            form = page.get_by_role("dialog")
            form.get_by_role("combobox",name="Provider",exact=True).select_option("codex")
            form.get_by_label("Label",exact=True).fill("Personal")
            form.get_by_role("button",name="Add profile").click()
            expect(rows).to_have_count(4)
            expect(page.get_by_role("button", name="Copy Codex sign-in command")).to_be_visible()
            expect(all_tab).to_have_attribute("aria-pressed","true")
            tabs.get_by_role("button", name="Codex", exact=True).click()
            expect(rows).to_have_count(2)
            expect(rows.locator(".provider-name")).to_have_count(0)
            self.check_layout(page, "accounts-provider-filter-mobile")
            page.get_by_role("navigation").get_by_text("Settings").click()
            versions = page.get_by_role("region",name="Binary versions")
            expect(versions).to_contain_text("codex-cli 0.171.0")
            expect(versions).to_contain_text("Antigravity")
            entry = service.root / "bin/codex"
            entry.write_text(entry.read_text().replace("0.171.0","0.180.1"))
            versions.get_by_role("button",name="Refresh binary versions").click()
            expect(versions).to_contain_text("codex-cli 0.180.1")
            page.get_by_role("navigation").get_by_text("Accounts").click()
            expect(all_tab).to_have_attribute("aria-pressed","true")
            expect(page.get_by_text("codex-cli 0.180.1")).to_have_count(0)
            self.check_layout(page, "all-accounts-mobile")

    def test_codex_saved_id_has_native_resume_command_and_no_startup_attribution(self):
        service = self.start_service()
        c = service.account("codex", "Saved")
        self.assertEqual(service.cli("codex").returncode, 0)
        uid = str(uuid.uuid4())
        with closing(sqlite3.connect(service.root/"state/cc-profile/router.sqlite3")) as db, db:
            db.execute("INSERT INTO sessions(tool,native_id,owner,evidence,bound_ms,saved_file_seen_ms) "
                       "VALUES('codex',?,?,'saved_file',5,5)", (uid,c["id"]))
        with sync_playwright() as pw, pw.chromium.launch(args=["--disable-dev-shm-usage"]) as browser:
            page = browser.new_page(permissions=["clipboard-read","clipboard-write"])
            page.goto(f"http://127.0.0.1:{service.port}/#sessions?tool=codex")
            page.get_by_label("Password",exact=True).fill(service.password)
            page.get_by_role("button",name="Sign in").click()
            row = page.get_by_role("row").filter(has_text="Not routed")
            expect(row).to_contain_text("Unknown")
            row.get_by_role("button",name="Copy resume command").click()
            self.assertEqual(shlex.split(page.evaluate("navigator.clipboard.readText()")),["ccp","codex","resume",uid])
            page.get_by_role("navigation").get_by_text("Dashboard").click()
            logs = page.get_by_role("region",name="Startup logs")
            expect(logs.get_by_role("cell",name="Unknown",exact=True)).to_be_visible()
            expect(logs.get_by_role("cell",name="Pending",exact=True)).to_have_count(0)

    def test_identity_follows_selected_account_not_delayed_probe(self):
        service = self.start_service()
        a, b = (service.account("claude", label) for label in ("Work", "Home"))
        (Path(a["home"])/"fixture-identity.json").write_text('{"loggedIn":true,"email":"work@example.test"}')
        with sync_playwright() as pw, pw.chromium.launch(args=["--disable-dev-shm-usage"]) as browser:
            page = browser.new_page()
            page.goto(f"http://127.0.0.1:{service.port}/#accounts?tool=claude&id={a['id']}")
            page.get_by_label("Password",exact=True).fill(service.password)
            page.get_by_role("button",name="Sign in").click()
            expect(page.get_by_text("Signed in: work@example.test",exact=True)).to_be_visible()
            status = page.get_by_role("status", name="Sign-in status")
            check = status.locator("use[href='/icons.svg#check']")
            expect(check).to_be_visible()
            endpoint = f"**/accounts/{a['id']}/identity"
            held = []
            page.route(endpoint, lambda route: held.append(route), times=1)
            with page.expect_request(endpoint):
                page.get_by_role("button",name="Refresh sign-in status").click()
            expect(page.get_by_role("button",name="Refresh sign-in status")).to_have_attribute("aria-busy", "true")
            expect(status).to_have_text("Signed in: work@example.test")
            page.locator(".account-row").filter(has_text=b["label"]).click()
            expect(page.get_by_text("Not signed in",exact=True)).to_be_visible()
            with page.expect_response(endpoint):
                held[0].fulfill(json={"state":"signed_in","email":"late@example.test"})
            expect(page.get_by_text("Not signed in",exact=True)).to_be_visible()
            expect(page.get_by_text("late@example.test",exact=False)).to_have_count(0)
            expect(check).to_have_count(0)
            page.route(f"**/accounts/{b['id']}/identity", lambda route:
                       route.fulfill(status=409,json={"error":"busy"}), times=1)
            refresh = page.get_by_role("button",name="Refresh sign-in status")
            refresh.click()
            expect(page.get_by_text("Other sign-in checks are running. Refresh to try again.")).to_be_visible()
            expect(check).to_have_count(0)
            refresh.click()
            expect(page.get_by_text("Not signed in",exact=True)).to_be_visible()

    def test_codex_verified_identity_wraps_in_both_themes(self):
        service = self.start_service()
        c = service.account("codex", "Personal")
        email = "long-address-" + ("a"*90) + "@example.test"
        (Path(c["home"])/"fixture-identity.json").write_text(json.dumps({"account":{"type":"chatgpt","email":email}}))
        with sync_playwright() as pw, pw.chromium.launch(args=["--disable-dev-shm-usage"]) as browser:
            page = browser.new_page(viewport={"width":320,"height":720})
            page.goto(f"http://127.0.0.1:{service.port}/#accounts?tool=codex&id={c['id']}")
            page.get_by_label("Password", exact=True).fill(service.password)
            page.get_by_role("button", name="Sign in", exact=True).click()
            status = page.get_by_role("status", name="Sign-in status")
            expect(status).to_have_text("Signed in: " + email)
            glyph = status.locator("svg")
            expect(glyph).to_have_attribute("aria-hidden", "true")
            expect(glyph.locator("use")).to_have_attribute("href", "/icons.svg#check")
            expect(glyph.locator("use")).to_be_visible()
            for scheme in ("light", "dark"):
                page.emulate_media(color_scheme=scheme)
                self.assertTrue(glyph.evaluate("svg => svg.getBBox().width > 0"))
                r, g, b = glyph.evaluate("""svg => {
                    const ctx = document.createElement('canvas').getContext('2d');
                    ctx.fillStyle = getComputedStyle(svg).stroke;
                    ctx.fillRect(0, 0, 1, 1);
                    return Array.from(ctx.getImageData(0, 0, 1, 1).data).slice(0, 3);
                }""")
                self.assertGreater(g, max(r, b))
                self.assertLessEqual(status.bounding_box()["x"] + status.bounding_box()["width"],
                                     page.get_by_role("button", name="Refresh sign-in status").bounding_box()["x"])
                self.check_layout(page, "codex-identity-"+scheme)

    def test_dashboard_quotas_above_independent_startup_filters(self):
        service = self.start_service()
        claude = service.account("claude", "Work")
        home = Path(claude["home"])
        (home / ".credentials.json").write_text('{"claudeAiOauth":{"accessToken":"private-test-token"}}')
        hold = home / "hold-fetch"
        hold.touch()
        status, codex, _ = service.api("POST", "accounts", {"tool":"codex", "label":"Work", "create":True})
        self.assertEqual(status, 201)
        with sync_playwright() as pw, pw.chromium.launch(args=["--disable-dev-shm-usage"]) as browser:
            page = browser.new_page(viewport={"width":1240, "height":850})
            page.clock.set_fixed_time("2033-05-15T04:00:00Z")
            page.goto(f"http://127.0.0.1:{service.port}/#startups?tool=codex")
            page.get_by_label("Password",exact=True).fill(service.password)
            page.get_by_role("button",name="Sign in").click()
            overview = page.get_by_role("region", name="Account quotas")
            logs = page.get_by_role("region", name="Startup logs")
            expect(page.get_by_role("link", name="Dashboard")).to_have_attribute("aria-current","page")
            expect(logs.get_by_role("combobox",name="Tool",exact=True)).to_have_value("codex")
            expect(overview).to_contain_text("Checking quota")
            search = logs.get_by_role("searchbox")
            search.fill("unfinished")
            hold.unlink()
            expect(overview).to_contain_text("75% left")
            expect(search).to_have_value("unfinished")
            expect(search).to_be_focused()
            search.fill("")
            disabled = overview.locator("section").filter(has=page.get_by_role("link",name="Codex / Work"))
            expect(disabled).to_contain_text("Disabled")
            expect(disabled).to_contain_text("Not checked")
            disabled.get_by_role("button",name="Refresh quota",exact=True).click()
            expect(disabled).to_contain_text("100% left")
            expect(disabled).to_contain_text("Earned resets available: 2")
            self.assertLess(overview.bounding_box()["y"] + overview.bounding_box()["height"], logs.bounding_box()["y"])
            countdown = disabled.get_by_text("· in 3d", exact=True)
            for width in [1240, 780, 320]:
                page.set_viewport_size({"width":width, "height":850})
                self.check_layout(page,"dashboard-" + str(width))
                self.assertEqual(countdown.evaluate("""s => {
                    const r = document.createRange(); r.selectNodeContents(s); return r.getClientRects().length;
                }"""), 1)
                if width == 780:
                    first = overview.locator("section").first.bounding_box()
                    self.assertEqual(disabled.bounding_box()["y"], first["y"])
                    self.assertLess(first["width"], 400)
                if width <= 780:
                    instant = disabled.locator("time span").first.bounding_box()
                    meter = disabled.get_by_role("progressbar").bounding_box()
                    self.assertEqual(instant["y"], countdown.bounding_box()["y"])
                    self.assertGreater(instant["y"], meter["y"] + meter["height"])
            disabled.get_by_role("link",name="Codex / Work").click()
            expect(page).to_have_url(f"http://127.0.0.1:{service.port}/#accounts?tool=codex&id={codex['id']}")
            expect(page.get_by_role("heading",name="Sign-in status")).to_be_visible()
            row = page.locator(".account-row")
            bar = row.get_by_role("progressbar", name="5h remaining")
            expect(bar).to_be_visible()
            expect(bar).to_have_attribute("value", "100")
            expect(row).to_have_accessible_name(re.compile("5h 100% left"))
            self.assertLess(row.locator("strong").bounding_box()["y"], bar.bounding_box()["y"])
            self.assertLess(bar.bounding_box()["y"], row.locator(".caption").bounding_box()["y"])
            page.emulate_media(color_scheme="dark")
            self.check_layout(page, "quota-bars-dark-mobile")

    def test_quota_poll_keeps_card_controls_interactive(self):
        service = self.start_service()
        for tool in ["claude", "codex"]:
            status, profile, _ = service.api("POST", "accounts", {"tool":tool, "label":"Work", "create":True})
            self.assertEqual(status, 201)
            if tool == "claude":
                home = Path(profile["home"])
        (home / ".credentials.json").write_text('{"claudeAiOauth":{"accessToken":"private-test-token"}}')
        hold = home / "hold-fetch"
        hold.touch()
        with sync_playwright() as pw, pw.chromium.launch(args=["--disable-dev-shm-usage"]) as browser:
            page = browser.new_page(viewport={"width":1240, "height":850})
            page.goto(f"http://127.0.0.1:{service.port}")
            page.get_by_label("Password", exact=True).fill(service.password)
            page.get_by_role("button", name="Sign in").click()
            overview = page.get_by_role("region", name="Account quotas")
            card = overview.locator("section").filter(has=page.get_by_role("link", name="Claude / Work"))
            refresh = card.get_by_role("button", name="Refresh quota")
            posts = []
            page.on("request", lambda req: posts.append(req.url) if req.method == "POST" and req.url.endswith("/quota/refresh") else None)
            held = []
            endpoint = "**/accounts/*/quota/refresh"
            page.route(endpoint, lambda route: held.append(route))
            refresh.focus()
            with page.expect_request(lambda req: req.method == "POST" and req.url.endswith("/quota/refresh")):
                refresh.press("Enter")
            page.keyboard.press("Enter")
            expect(refresh).to_be_disabled()
            expect(refresh).to_be_focused()
            expect(refresh).to_have_css("opacity", "1")
            expect(refresh).to_have_attribute("aria-busy", "true")
            expect(refresh.locator("svg")).to_have_css("animation-name", "router-spin")
            page.emulate_media(reduced_motion="reduce")
            expect(refresh.locator("svg")).to_have_css("animation-name", "none")
            expect(refresh).to_have_css("outline-style", "solid")
            self.assertEqual(len(held), 1)
            held[0].fulfill(status=503, json={"message": "Refresh temporarily unavailable"})
            expect(page.get_by_role("status")).to_contain_text("Refresh temporarily unavailable")
            expect(refresh).to_be_enabled()
            expect(refresh).to_have_attribute("aria-busy", "false")
            page.unroute(endpoint)
            refresh.press("Enter")
            expect(card).to_contain_text("Checking quota")
            expect(refresh).to_be_disabled()
            expect(refresh).to_be_focused()
            page.keyboard.press("Enter")

            link = overview.get_by_role("link", name="Codex / Work")
            link.hover()
            page.mouse.down()
            hold.unlink()
            expect(card).to_contain_text("75% left")
            expect(refresh).to_be_enabled()
            self.assertEqual(len(posts), 2)  # One failed request and one successful retry.
            expect(link).to_be_focused()
            page.mouse.up()
            expect(page.get_by_role("heading", name="Sign-in status")).to_be_visible()
            expect(page).to_have_url(f"http://127.0.0.1:{service.port}/#accounts?tool=codex&id={profile['id']}")

    def test_log_filters_and_session_navigation(self):
        service = self.start_service()
        profile = service.account("claude", "Work" * 24)
        launch = service.cli("claude", "fixture")
        self.assertEqual(launch.returncode, 0, launch.stderr)
        record, = service.api("GET", "startups")[1]["items"]
        with sync_playwright() as pw, pw.chromium.launch(args=["--disable-dev-shm-usage"]) as browser:
            page = browser.new_page(viewport={"width": 1240, "height": 850})
            fonts = []
            page.on("request", lambda req: fonts.append(req.url) if req.resource_type == "font" else None)
            page.goto(f"http://127.0.0.1:{service.port}")
            page.get_by_label("Password", exact=True).fill(service.password)
            page.get_by_role("button", name="Sign in").click()
            page.get_by_role("button", name="Startup details").click()
            expect(page.get_by_role("button", name="Startup details", expanded=True)).to_be_visible()
            expect(page.get_by_text(record["request_id"], exact=True)).to_be_visible()
            expect(page.get_by_text("Earliest expiry, most quota remaining", exact=True)).to_have_count(1)
            self.check_layout(page, "startups")
            page.set_viewport_size({"width": 320, "height": 720})
            config_filter = page.get_by_role("combobox", name="Config", exact=True)
            config_filter.select_option(str(profile["id"]))
            expect(config_filter).to_have_value(str(profile["id"]))
            self.check_layout(page, "startups-mobile")
            self.check_mobile_records(page, "Startup details", "Decision")
            page.locator("tbody .uuid").click()
            expect(page.get_by_role("main", name="Sessions", exact=True)).to_be_visible()
            search = page.get_by_role("searchbox", name="Session ID")
            expect(search).to_have_value(record["native_id"])
            expect(page.get_by_role("button", name="Copy resume command")).to_be_enabled()
            expect(page.get_by_role("button", name="Next page")).to_have_count(0)
            self.check_layout(page, "sessions-mobile")
            self.check_mobile_records(page, "Copy resume command", "Workspace")
            search.fill(str(uuid.uuid4()))
            search.press("Enter")
            expect(page.get_by_role("heading", name="No matching sessions")).to_be_visible()
            expect(page.get_by_role("table")).to_have_count(0)
            page.get_by_role("button", name="Clear filters").click()
            page.get_by_role("button", name=profile["label"], exact=True).click()
            expect(page.get_by_role("heading", name=profile["label"])).to_be_visible()
            self.check_layout(page, "profile-mobile")
            self.assertEqual(fonts, [])
            self.assertEqual(page.evaluate("document.fonts.size"), 0)

    def test_confirmed_reset_reaches_only_the_selected_native_grant(self):
        service = self.start_service()
        c, home, _, _, _ = service.reset_fixture()
        calls = home / "reset-consumes.jsonl"
        with sync_playwright() as pw, pw.chromium.launch(args=["--disable-dev-shm-usage"]) as browser:
            page = browser.new_page()
            page.goto(f"http://127.0.0.1:{service.port}/#accounts?tool=codex&id={c['id']}")
            page.get_by_label("Password", exact=True).fill(service.password)
            page.get_by_role("button", name="Sign in").click()
            list_grants = page.get_by_role("button", name="List reset grants")
            list_grants.click()
            dialog = page.get_by_role("dialog")
            dialog.get_by_role("button", name="Review grant 1: One").click()
            dialog.get_by_role("button", name="Close", exact=True).click()
            self.assertFalse(calls.exists())
            list_grants.click()
            dialog.get_by_role("button", name="Review grant 2: Two").click()
            expect(dialog).to_contain_text(c["label"])
            expect(dialog).to_contain_text("Grant 2: Two")
            self.assertFalse(calls.exists())
            dialog.get_by_role("button", name="Redeem this grant").click()
            expect(dialog).to_contain_text("Grant redeemed")
            sent, = [json.loads(line) for line in calls.read_text().splitlines()]
            self.assertEqual(sent["params"]["creditId"], "opaque-credit-Two")
            self.assertEqual(sent["home"], str(home))
            self.assertEqual(sent["proxy"], "http://fixture.invalid:8111/")
            self.assertFalse(service.api("GET", "startups")[1]["items"])

    def check_mobile_records(self, page, action, column):
        table = page.get_by_role("table")
        expect(table.get_by_role("columnheader", name=column, exact=True)).to_have_count(1)
        expect(table.locator(".cell-label", has_text=re.compile("^" + column + "$")).first).to_be_visible()
        self.assertTrue(page.locator(".tablewrap").evaluate("e => e.scrollWidth <= e.clientWidth"))
        box = table.get_by_role("button", name=action, exact=True).first.bounding_box()
        self.assertGreaterEqual(box["x"], 0)
        self.assertLessEqual(box["x"] + box["width"], page.viewport_size["width"])
        self.assertGreaterEqual(min(box["width"], box["height"]), 44)

    def check_layout(self,page,name):
        self.assertLessEqual(page.evaluate("document.documentElement.scrollWidth"),page.viewport_size["width"],name)
        path=os.environ.get("CCP_SCREENSHOTS")
        if path:
            Path(path).mkdir(exist_ok=True)
            page.screenshot(path=str(Path(path)/(name+".png")),full_page=True)

    def open_grant_page(self, page, service, fragment):
        page.goto(f"http://127.0.0.1:{service.port}/#{fragment}")
        page.get_by_label("Password", exact=True).fill(service.password)
        page.get_by_role("button", name="Sign in").click()

    def test_reset_grants_require_a_fresh_list_selection_and_confirmation(self):
        service = self.start_service()
        _, c, _ = service.api("POST", "accounts", {"tool":"codex","label":"Work","create":True})
        service.api("POST", "accounts", {"tool":"claude","label":"Personal","create":True})
        base = f"/api/v1/accounts/{c['id']}"
        with sync_playwright() as pw, pw.chromium.launch(args=["--disable-dev-shm-usage"]) as browser:
            page = browser.new_page(viewport={"width":1240,"height":850})
            lists, redeems, refreshes = [], [], []
            checking = False
            def accounts(route):
                nonlocal checking
                data = route.fetch().json()
                data["items"][0]["quota"] = {"loading":checking, "status":"fresh", "success_ms":1,
                    "snapshot":{"windows":[], "earned_resets":7 - len(redeems) + int(checking)}}
                checking = False
                route.fulfill(json=data)
            page.route("**/accounts?tool=codex", accounts)
            def grants(route):
                self.assertEqual(route.request.method, "POST")
                self.assertTrue(route.request.headers["x-ccp-session"])
                if route.request.url.endswith(base+"/resets/list"):
                    self.assertEqual(route.request.post_data_json, {})
                    lists.append(route.request.url)
                    route.fulfill(json={"available_count":7, "details_available":True, "grants":[
                        {"handle":"expired", "title":"Earlier grant", "expires_at_ms":1},
                        {"handle":"not-selected", "title":"Other grant", "expires_at_ms":None},
                        {"handle":f"chosen-{len(lists)}", "title":"Bonus <reset>", "expires_at_ms":None}]})
                else:
                    self.assertTrue(route.request.url.endswith(base+"/resets/redeem"))
                    redeems.append(route)
            page.route("**/api/v1/accounts/*/resets/*", grants)
            page.on("request", lambda r: refreshes.append(r.url) if r.url.endswith(base+"/quota/refresh") else None)
            self.open_grant_page(page, service, "dashboard")
            action = page.get_by_role("button", name="List reset grants")
            expect(action).to_have_count(1)
            self.assertFalse(lists or redeems)
            dlg = page.get_by_role("dialog")
            action.press("Enter")
            expect(dlg).to_contain_text("Only some grants are listed")
            expect(dlg.get_by_role("button", name="Review grant 1: Earlier grant")).to_be_disabled()
            self.check_layout(page, "codex-grants-desktop")
            page.set_viewport_size({"width":320,"height":720})
            review = dlg.get_by_role("button", name="Review grant 3: Bonus <reset>")
            review.press("Enter")
            expect(dlg).to_have_accessible_name("Confirm redemption")
            expect(dlg).to_have_accessible_description(f"Codex / Work (#{c['id']})")
            expect(dlg).to_contain_text("Grant 3: Bonus <reset>")
            expect(dlg).to_contain_text("No expiration")
            self.check_layout(page, "codex-grant-mobile")
            self.assertLessEqual(dlg.evaluate("d => d.scrollWidth"), dlg.evaluate("d => d.clientWidth"))
            dlg.get_by_role("button", name="Close").click()
            self.assertFalse(redeems)
            for i, (outcome, message) in enumerate([
                ("reset","Grant redeemed."), ("alreadyRedeemed","already redeemed"),
                ("nothingToReset","no usage limit to reset"), ("noCredit","No earned reset was available")]):
                action.click()
                review.click()
                confirm = dlg.get_by_role("button", name="Redeem this grant")
                with page.expect_request("**"+base+"/resets/redeem"):
                    confirm.evaluate("button => { button.click(); button.click(); }")
                expect(dlg).to_contain_text("Waiting for Codex")
                self.assertEqual(len(redeems), i+1)
                self.assertEqual(redeems[-1].request.post_data_json, {"handle":f"chosen-{i+2}"})
                checking = True
                redeems[-1].fulfill(json={"outcome":outcome})
                expect(dlg).to_contain_text(message)
                card = page.locator(".quota-card").filter(has=action)
                expect(card.get_by_role("button", name="Refresh quota")).to_have_attribute("aria-busy", "true")
                expect(card).to_contain_text(f"Earned resets available: {6-i}")
                self.assertEqual(refreshes, [])  # The daemon already queued quota reconciliation.
                expect(dlg.get_by_role("button", name="Redeem this grant")).to_have_count(0)
                dlg.get_by_role("button", name="Close").click()

    def test_reset_grant_errors_and_expiry_cannot_spend(self):
        service = self.start_service()
        c = service.account("codex", "Work")
        with sync_playwright() as pw, pw.chromium.launch(args=["--disable-dev-shm-usage"]) as browser:
            page = browser.new_page()
            page.clock.set_fixed_time("2030-01-01T00:00:00Z")
            endpoint = f"**/accounts/{c['id']}/resets/"
            redeems = []
            def reject(route):
                redeems.append(route.request.post_data_json)
                if error:
                    route.fulfill(status=409 if error == "stale_grant" else 503,
                                  json={"error":error,"message":"/private/provider/path"})
                else:
                    route.abort("timedout")
            error = "stale_grant"
            page.route(endpoint+"redeem", reject)
            self.open_grant_page(page, service, f"accounts?tool=codex&id={c['id']}")
            action = page.get_by_role("button", name="List reset grants")
            dlg = page.get_by_role("dialog")
            def listed(body, status=200):
                page.route(endpoint+"list", lambda r: r.fulfill(status=status, json=body), times=1)
                action.click()
            for body, status, message in [
                ({"available_count":None,"details_available":False,"grants":[]}, 200, "No grant details available"),
                ({"available_count":0,"details_available":True,"grants":[]}, 200, "No available grants"),
                ({"error":"unavailable","message":"/private/provider/path"}, 503, "Grant details are unavailable"),
                ({"error":"busy","message":"/private/provider/path"}, 409, "Another request is in progress")]:
                listed(body, status)
                expect(dlg).to_contain_text(message)
                expect(dlg.get_by_role("button", name="Redeem this grant")).to_have_count(0)
                expect(dlg).not_to_contain_text("/private")
                dlg.get_by_role("button", name="Close").click()
            def grant(expires):
                return {"available_count":1,"details_available":True,"grants":[
                    {"handle":"selected", "title":"One reset", "expires_at_ms":expires}]}
            listed(grant(1893456060000))
            dlg.get_by_role("button", name="Review grant 1: One reset").click()
            page.clock.set_fixed_time("2030-01-01T00:02:00Z")
            dlg.get_by_role("button", name="Redeem this grant").click()
            expect(dlg).to_contain_text("expired or is no longer available")
            self.assertEqual(redeems, [])
            dlg.get_by_role("button", name="Close").click()
            for error, message in [("stale_grant", "expired or is no longer available"),
                                   ("native_rpc", "Redemption was not submitted"),
                                   ("rate_limited", "Redemption was not submitted"),
                                   (None, "earlier attempt may have used this grant")]:
                listed(grant(None))
                dlg.get_by_role("button", name="Review grant 1: One reset").click()
                dlg.get_by_role("button", name="Redeem this grant").click()
                expect(dlg).to_contain_text(message)
                expect(dlg).not_to_contain_text("/private")
                expect(dlg.get_by_role("button", name="Retry same grant")).to_have_count(int(error is None))
                dlg.get_by_role("button", name="Close").click()
            self.assertEqual(redeems, [{"handle":"selected"}]*4)

    def test_uncertain_reset_retries_only_the_same_profile_and_handle(self):
        service = self.start_service()
        a, b = (service.account("codex", label) for label in ("Work", "Other"))
        with sync_playwright() as pw, pw.chromium.launch(args=["--disable-dev-shm-usage"]) as browser:
            page = browser.new_page()
            lists, pending = [], []
            def grants(route):
                if route.request.url.endswith("/list"):
                    lists.append(route.request.url)
                    route.fulfill(json={"available_count":1,"details_available":True,"grants":[
                        {"handle":f"original-{len(lists)}", "title":"One grant", "expires_at_ms":None}]})
                else:
                    pending.append(route)
            page.route("**/api/v1/accounts/*/resets/*", grants)
            self.open_grant_page(page, service, f"accounts?tool=codex&id={a['id']}")
            action = page.get_by_role("button", name="List reset grants")
            dlg = page.get_by_role("dialog")
            action.click()
            dlg.get_by_role("button", name="Review grant 1: One grant").click()
            with page.expect_request("**/resets/redeem"):
                dlg.get_by_role("button", name="Redeem this grant").click()
            def navigate(c):
                page.evaluate("id => {location.hash = 'accounts?tool=codex&id=' + id}", c["id"])
                expect(page.locator(".detail h2")).to_have_text(c["label"])
                expect(dlg).to_be_hidden()
            navigate(b)
            action.click()
            expect(dlg).to_have_accessible_description(f"Codex / Other (#{b['id']})")
            pending[0].fulfill(status=504, json={"error":"outcome_unknown","message":"/private/provider/path"})
            expect(dlg.get_by_role("button", name="Review grant 1: One grant")).to_be_visible()
            self.assertEqual(len(pending), 1)
            navigate(a)
            action.click()
            expect(dlg).to_have_accessible_name("Unconfirmed redemption")
            expect(dlg).not_to_contain_text("/private")
            retry = dlg.get_by_role("button", name="Retry same grant")
            for i, error in enumerate(("native_rpc", "rate_limited", "timeout", None), start=1):
                with page.expect_request("**/resets/redeem"):
                    retry.press("Enter")
                self.assertEqual(len(pending), i+1)
                self.assertTrue(pending[i].request.url.endswith(f"/accounts/{a['id']}/resets/redeem"))
                self.assertEqual(pending[i].request.post_data_json, {"handle":"original-1"})
                if error:
                    if error == "timeout":
                        pending[i].abort("timedout")
                    else:
                        pending[i].fulfill(status=503, json={"error":error})
                    expect(dlg).to_have_accessible_name("Unconfirmed redemption")
                    expect(dlg).not_to_contain_text("Redemption was not submitted")
                    expect(retry).to_be_visible()
                else:
                    pending[i].fulfill(json={"outcome":"alreadyRedeemed"})
                    expect(dlg).to_contain_text("already redeemed")
            self.assertEqual(len(lists), 2)

    def test_late_grant_list_cannot_confirm_after_profile_switch(self):
        service = self.start_service()
        a, b = (service.account("codex", label) for label in ("First", "Second"))
        with sync_playwright() as pw, pw.chromium.launch(args=["--disable-dev-shm-usage"]) as browser:
            page = browser.new_page()
            held, redeems = [], []
            page.route("**/resets/list", lambda r: held.append(r))
            page.on("request", lambda r: redeems.append(r) if r.url.endswith("/resets/redeem") else None)
            self.open_grant_page(page, service, f"accounts?tool=codex&id={a['id']}")
            action = page.get_by_role("button", name="List reset grants")
            with page.expect_request("**/resets/list"):
                action.click()
            dlg = page.get_by_role("dialog")
            expect(dlg).to_contain_text("Loading grants")
            page.evaluate("id => {location.hash = 'accounts?tool=codex&id=' + id}", b["id"])
            expect(page.locator(".detail h2")).to_have_text("Second")
            with page.expect_request("**/resets/list"):
                action.click()
            held[1].fulfill(json={"available_count":0,"details_available":True,"grants":[]})
            expect(dlg).to_contain_text("No available grants")
            with page.expect_response("**/resets/list"):
                held[0].fulfill(json={"available_count":1,"details_available":True,"grants":[
                    {"handle":"wrong-profile", "title":"First only", "expires_at_ms":None}]})
            expect(dlg).to_have_accessible_description(f"Codex / Second (#{b['id']})")
            expect(dlg).not_to_contain_text("First only")
            dlg.get_by_role("button", name="Close").click()
            with page.expect_request("**/resets/list"):
                action.click()
            # The card was rendered before a different tab edited this profile.
            status, _, _ = service.api("PATCH", f"accounts/{b['id']}", {"revision":b["revision"],"label":"Updated"})
            self.assertEqual(status, 200)
            held[2].fulfill(json={"available_count":1,"details_available":True,"grants":[
                {"handle":"new-revision", "title":"Updated only", "expires_at_ms":None}]})
            expect(dlg).to_be_hidden()
            expect(page.locator("#notice")).to_contain_text("This profile or its sign-in changed")
            expect(page.locator(".detail h2")).to_have_text("Updated")
            self.assertEqual(redeems, [])

    def check_border_contrast(self, control):
        border, surface = control.evaluate("""e => [
            getComputedStyle(e).borderTopColor, getComputedStyle(document.documentElement).backgroundColor
        ]""")
        def luminance(color):
            rgb = [int(v) / 255 for v in re.fullmatch(r"rgb\((\d+), (\d+), (\d+)\)", color).groups()]
            return sum(w * (v / 12.92 if v <= 0.04045 else ((v + 0.055) / 1.055) ** 2.4)
                       for w, v in zip((0.2126, 0.7152, 0.0722), rgb))
        low, high = sorted(map(luminance, (border, surface)))
        self.assertGreaterEqual((high + 0.05) / (low + 0.05), 3, f"{border} against {surface}")

    def test_theme_before_and_after_sign_in(self):
        service = self.start_service()
        with sync_playwright() as pw, pw.chromium.launch(args=["--disable-dev-shm-usage"]) as browser:
            page = browser.new_page(color_scheme="dark", viewport={"width":320, "height":720})
            page.goto(f"http://127.0.0.1:{service.port}")
            theme = page.get_by_role("banner").get_by_role("button", name=re.compile("^Theme:"))
            expect(theme).to_have_accessible_name("Theme: system. Switch to light.")
            root = page.locator("html")
            background = lambda: root.evaluate("e => getComputedStyle(e).backgroundColor")
            dark = background()
            page.emulate_media(color_scheme="light")
            light = background()
            self.assertNotEqual(dark, light)
            theme.press("Enter")
            expect(theme).to_have_accessible_name("Theme: light. Switch to dark.")
            theme.press("Space")
            page.reload()
            expect(theme).to_have_accessible_name("Theme: dark. Switch to system.")
            expect(root).to_have_css("background-color", dark)
            self.check_layout(page, "dark-signin")
            page.get_by_label("Password", exact=True).fill(service.password)
            page.get_by_role("button", name="Sign in").click()
            page.get_by_role("link", name="Settings", exact=True).click()
            expect(theme).to_have_accessible_name("Theme: dark. Switch to system.")
            logout = page.get_by_role("button", name="Sign out")
            for width in (320, 1240):
                page.set_viewport_size({"width": width, "height": 720})
                a, b = theme.bounding_box(), logout.bounding_box()
                self.assertEqual(a["y"], b["y"])
                self.assertTrue(a["x"] + a["width"] <= b["x"] <= a["x"] + a["width"] + 12)
            page.set_viewport_size({"width": 320, "height": 720})
            password = page.get_by_label("Current password", exact=True)
            self.check_border_contrast(password)
            self.check_layout(page, "dark-settings-mobile")
            theme.click()
            theme.click()
            page.emulate_media(color_scheme="dark")
            expect(root).to_have_css("background-color", light)
            self.check_border_contrast(password)
            page.reload()
            expect(theme).to_have_accessible_name("Theme: light. Switch to dark.")
            theme.click()
            theme.click()
            self.assertIsNone(page.evaluate("localStorage.getItem('ccp_theme')"))
            expect(root).to_have_css("background-color", dark)
            page.get_by_role("button", name="Sign out").click()
            expect(theme).to_have_accessible_name("Theme: system. Switch to light.")
            page.emulate_media(color_scheme="light")
            expect(root).to_have_css("background-color", light)

    def test_save_routing_mode_and_account_priority(self):
        service = self.start_service()
        with sync_playwright() as pw, pw.chromium.launch(args=["--disable-dev-shm-usage"]) as browser:
            page = browser.new_page(viewport={"width":1240, "height":850})
            page.goto(f"http://127.0.0.1:{service.port}/#settings")
            page.get_by_label("Password", exact=True).fill(service.password)
            page.get_by_role("button", name="Sign in").click()
            mode = page.get_by_role("combobox", name="Routing mode", exact=True)
            expect(mode).to_have_value("earliest_expiry_most_remaining")
            expect(mode.get_by_role("option", name="Earliest expiry", exact=True)).to_have_count(0)
            for label in ("Earliest expiry, most quota remaining", "Earliest expiry, least quota remaining"):
                expect(mode.get_by_role("option", name=label, exact=True)).to_have_count(1)
            for value in ("earliest_expiry_least_remaining", "least_remaining", "priority", "round_robin", "most_remaining", "earliest_expiry_most_remaining"):
                mode.select_option(value)
                save = page.get_by_role("button", name="Save routing mode")
                with page.expect_response(lambda r: r.request.method == "PUT" and r.url.endswith("/settings")) as result:
                    save.press("Enter")
                self.assertEqual(result.value.status, 200)
                self.assertEqual(service.api("GET", "settings")[1]["routing_mode"], value)
                page.reload()
                expect(mode).to_have_value(value)
            self.check_layout(page, "settings")
            page.set_viewport_size({"width":320, "height":720})
            self.check_layout(page, "settings-mobile")
            page.set_viewport_size({"width":1240, "height":850})
            page.get_by_role("link", name="Accounts", exact=True).click()
            page.get_by_role("button", name="Add profile").click()
            priority = page.get_by_role("spinbutton", name="Priority", exact=True)
            expect(priority).to_have_value("0")
            for invalid in ("-1", "101", "2.5", ""):
                priority.fill(invalid)
                self.assertFalse(priority.evaluate("e => e.checkValidity()"))
            page.get_by_label("Label", exact=True).fill("Primary")
            priority.fill("100")
            page.get_by_role("dialog").get_by_role("button", name="Add profile").click()
            expect(page.locator(".detail").get_by_text("Priority", exact=True)).to_be_visible()
            expect(page.locator(".facts dd", has_text=re.compile("^100$"))).to_be_visible()
            self.assertEqual(service.api("GET","accounts?tool=claude")[1]["items"][0]["priority"], 100)
            page.get_by_role("button", name="Edit config").click()
            expect(priority).to_have_value("100")
            priority.fill("0")
            page.get_by_role("button", name="Save changes").click()
            expect(page.get_by_role("dialog")).to_be_hidden()
            page.reload()
            expect(page.locator(".facts dd", has_text=re.compile("^0$"))).to_be_visible()
            self.assertEqual(service.api("GET","accounts?tool=claude")[1]["items"][0]["priority"], 0)

    def test_change_panel_password(self):
        service = self.start_service()
        with sync_playwright() as pw, pw.chromium.launch(args=["--disable-dev-shm-usage"]) as browser:
            page = browser.new_page(viewport={"width":390, "height":844})
            page.goto(f"http://127.0.0.1:{service.port}/#sessions")
            page.get_by_label("Password", exact=True).fill(service.password)
            page.get_by_role("button", name="Sign in").click()
            refresh = page.get_by_role("button", name="Refresh", exact=True)
            expect(refresh).to_be_visible()
            # Send a read with the original token only after signing in with the new password.
            held = []
            page.route("**/api/v1/sessions*", lambda route: held.append(route), times=1)
            with page.expect_request("**/api/v1/sessions*"):
                refresh.click()
            page.get_by_role("link", name="Settings", exact=True).click()
            current = page.get_by_label("Current password", exact=True)
            new = page.get_by_label("New password", exact=True)
            confirm = page.get_by_label("Confirm new password", exact=True)
            change = page.get_by_role("button", name="Change password")
            error = page.locator("form").filter(has=current).get_by_role("alert")
            current.fill("incorrect")
            new.fill("abc")
            self.assertFalse(new.evaluate("e => e.checkValidity()"))
            new.fill("fixture-new")
            confirm.fill("mismatch")
            change.click()
            expect(error).to_have_text("Passwords do not match.")
            confirm.fill("fixture-new")
            with page.expect_response(lambda r: r.url.endswith("/password")) as wrong:
                change.click()
            self.assertEqual(wrong.value.status, 400)
            expect(error).to_have_text(wrong.value.json()["message"])
            current.fill(service.password)
            change.click()
            expect(page.get_by_role("heading", name="Sign in")).to_be_visible()
            self.assertIsNone(page.evaluate("sessionStorage.getItem('ccp_session')"))
            self.assertEqual(service.api("GET", "accounts?tool=claude")[0], 403)
            page.get_by_label("Password", exact=True).fill("fixture-new")
            page.get_by_role("button", name="Sign in").click()
            expect(page.get_by_role("main", name="Settings")).to_be_visible()
            with page.expect_response("**/api/v1/sessions*") as stale:
                held[0].continue_()
            self.assertEqual(stale.value.status, 403)
            expect(page.locator("#notice")).to_have_text(stale.value.json()["message"])
            expect(page.get_by_role("main", name="Settings")).to_be_visible()

    def test_delayed_password_success_preserves_new_session(self):
        service = self.start_service()
        with sync_playwright() as pw, pw.chromium.launch(args=["--disable-dev-shm-usage"]) as browser:
            page = browser.new_page()
            page.goto(f"http://127.0.0.1:{service.port}/#settings")
            page.get_by_label("Password", exact=True).fill(service.password)
            page.get_by_role("button", name="Sign in").click()
            page.get_by_label("Current password").fill(service.password)
            page.get_by_label("New password", exact=True).fill("fixture-new")
            page.get_by_label("Confirm new password").fill("fixture-new")
            change = page.get_by_role("button", name="Change password")
            original_button = change.element_handle()
            held = []
            page.route("**/api/v1/password", lambda route: held.append(route), times=1)
            with page.expect_request("**/api/v1/password"):
                change.click()
            success = held[0].fetch()
            self.assertEqual(success.status, 200)

            # The server rotated the password but the submitting tab hasn't seen its 200 yet.
            with page.expect_response("**/api/v1/sessions*") as rejected:
                page.get_by_role("link", name="Sessions", exact=True).click()
            self.assertEqual(rejected.value.status, 403)
            page.get_by_label("Password", exact=True).fill("fixture-new")
            page.get_by_role("button", name="Sign in").click()
            sessions = page.get_by_role("main", name="Sessions", exact=True)
            expect(sessions).to_be_visible()
            token = page.evaluate("sessionStorage.getItem('ccp_session')")
            self.assertTrue(token)

            held[0].fulfill(response=success)
            page.wait_for_function("button => !button.disabled", arg=original_button)
            self.assertTrue(page.evaluate("token => sessionStorage.getItem('ccp_session') === token", token))
            expect(sessions).to_be_visible()
            expect(page.get_by_role("status")).not_to_contain_text("Sign in again")

if __name__=="__main__":
    unittest.main()
