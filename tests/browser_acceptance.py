"""Real-browser MOP for the self-contained LinkStart core PoC."""
import json, os, socket, subprocess, tempfile, time, urllib.error, urllib.parse, urllib.request
from pathlib import Path
from playwright.sync_api import sync_playwright

ROOT = Path(__file__).resolve().parents[1]
BIN = Path(os.environ.get("LINKSTART_BIN", ROOT / "target" / "debug" / "linkstart"))

def port():
    sock = socket.socket(); sock.bind(("127.0.0.1", 0)); value = sock.getsockname()[1]; sock.close(); return value

def call(url, body=None, headers=None):
    request = urllib.request.Request(url, data=None if body is None else json.dumps(body).encode(), headers=headers or {}, method="POST")
    with urllib.request.urlopen(request, timeout=5) as response:
        return json.loads(response.read())

def cli(*args):
    return subprocess.run([str(BIN), *args], check=True, capture_output=True, text=True).stdout

def main():
    assert BIN.exists(), "build first: cargo build"
    with tempfile.TemporaryDirectory(prefix="linkstart-browser-") as state:
        p = port(); daemon = subprocess.Popen([str(BIN), "daemon", "run", "--state-dir", state, "--port", str(p)], stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        base = f"http://127.0.0.1:{p}"
        try:
            for _ in range(50):
                try:
                    urllib.request.urlopen(base + "/v1/health", timeout=.2).read(); break
                except urllib.error.URLError: time.sleep(.1)
            else: raise AssertionError("daemon did not start")
            connection = call(base + "/v1/connections", {"protocolMajor":"v1", "callsign":"browser"}, {"Content-Type":"application/json"})
            cid, ccap = connection["connectionId"], connection["capability"]
            grant = call(base + "/v1/launch-grants", {"protocolMajor":"v1", "connectionId":cid, "manifest":{"appId":"self-contained","displayName":"Browser PoC","originPolicy":{"exactOrigin":"null"}}}, {"Content-Type":"application/json", "Authorization":"Bearer " + ccap})["grant"]
            page_url = (ROOT / "examples" / "self-contained.html").as_uri() + "#" + urllib.parse.urlencode({"daemon":base,"grant":grant})
            with sync_playwright() as play:
                browser = play.chromium.launch(headless=True)
                page = browser.new_page()
                errors=[]; page.on("console", lambda m: errors.append(m.text) if m.type == "error" else None); page.goto(page_url); page.wait_for_selector("#send:not([disabled])")
                assert "grant=" not in page.url
                try: call(base + "/v1/launch-grants/redeem", None, {"Authorization":"Bearer " + grant, "Origin":"null"}); raise AssertionError("launch grant redeemed twice")
                except urllib.error.HTTPError as error: assert error.code == 403
                page.click("#send"); page.wait_for_function("document.querySelector('#events').innerText.includes('receipt: received')")
                monitor = json.loads(cli("monitor","wait","--json","--state-dir",state,"--connection-id",cid,"--capability",ccap,"--timeout-seconds","2"))
                assert monitor["status"] == "received" and monitor["payload"]["message"] == "使用者互動"
                cli("monitor","ack","--json","--state-dir",state,"--connection-id",cid,"--capability",ccap,"--event-id",monitor["eventId"])
                page.evaluate("window.linkstartPoC.disconnect()")
                cli("feedback","send","--json","--state-dir",state,"--connection-id",cid,"--capability",ccap,"--app-instance-id",monitor["appInstanceId"],"--feedback-id","browser-feedback","--in-reply-to-event-id",monitor["eventId"],"--payload",json.dumps({"message":"replayed feedback"}))
                page.evaluate("() => { window.linkstartPoC.reconnect(); return true; }")
                page.wait_for_function("document.querySelector('#events').innerText.includes('feedback: replayed feedback')")
                assert "delivery_ack: delivered" in page.locator("#events").inner_text()
                assert not errors, errors
                browser.close()
            smoke = "".join([cli("help","--json"), cli("status","--json","--state-dir",state), cli("ps","--json","--state-dir",state), cli("doctor","--json","--state-dir",state)])
            assert ccap not in smoke and grant not in smoke
            print("browser_acceptance: PASS")
        finally:
            daemon.terminate(); daemon.wait(timeout=5)

if __name__ == "__main__": main()
