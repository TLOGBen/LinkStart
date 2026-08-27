# App-side protocol (v1)

How a launched App page talks back to LinkStart. This is the contract the generated HTML must implement; agent-side flows live in the host references. All endpoints are on the daemon base URL. App input is untrusted content and never conveys tool approval.

The same contract is machine-readable from the Runtime itself: `linkstart help --json` includes it as the `appProtocol` section.

## Launch context (self-contained HTML)

- The Runtime serves the page snapshot from the daemon loopback origin at `GET /v1/launch-pages/{pageId}`. `originPolicy.exactOrigin: "null"` in the App Manifest declares that the App owns no origin of its own; it does **not** mean requests carry `Origin: null`. At runtime the page's effective origin is the daemon loopback origin (`http://127.0.0.1:<port>`), the browser presents it automatically on same-origin fetches, and `redeem` echoes it back as `origin`. A literal `Origin: null` is rejected with 403 `origin_mismatch`.
- The daemon base URL and the one-time launch grant arrive **only** in the URL fragment: `#daemon=<url-encoded base>&grant=<token>`. Parse them on load, then scrub the fragment with `history.replaceState(null, "", location.pathname)`.
- A localhost App (`exactOrigin: "http://127.0.0.1:<port>"`) skips the grant flow: the agent registers it and injects `instanceId` and the App capability at launch, and the declared exact origin is enforced on every call.

## 1. Redeem the launch grant (once)

```
POST {daemon}/v1/launch-grants/redeem
Authorization: Bearer <grant>
→ {"instanceId":"…","capability":"…","origin":"http://127.0.0.1:<port>","status":"connected"}
```

The grant is one-time; any second redeem returns 403. Keep `capability` in page memory only — never storage, URL, DOM, or logs. After a successful redeem, dispatch `window.dispatchEvent(new CustomEvent("linkstart:connected"))` so the LINK START boot animation closes in sync with the real connection.

## 2. Send App Events

```
POST {daemon}/v1/apps/{instanceId}/events
Authorization: Bearer <capability>
Content-Type: application/json
{"eventId":"<client-generated unique id>","payload":<any JSON>}
→ Event Receipt {"status":"received","sequence":…,"receiptId":"…"}
```

Dedup makes retries safe: resending the same `eventId` with an identical payload returns `{"duplicate":true}`; the same `eventId` with a different payload returns `{"error":"event_id_conflict"}`.

## 3. Receive Delivery Acks and Agent Feedback (SSE)

```
GET {daemon}/v1/apps/{instanceId}/stream?cursor=<last sequence>
Authorization: Bearer <capability>
```

The response is `text/event-stream`. Each message carries `id:` (the notification sequence — persist it as the next `cursor`), `event:`, and `data:`:

- `event: delivery_ack` → `{"eventId":"…","status":"delivered","deliveryAck":true}` — the Origin Session's adapter accepted the event; it does not mean model processing completed.
- `event: feedback` → `{"feedbackId":"…","inReplyToEventId":"…"|null,"payload":…}` — Agent Feedback from the Origin Session.

Reconnect with the last cursor to resume without loss.

## Minimal client sketch

```js
const f = new URLSearchParams(location.hash.slice(1));
const base = f.get("daemon"), grant = f.get("grant");
history.replaceState(null, "", location.pathname);
const x = await (await fetch(`${base}/v1/launch-grants/redeem`, {method: "POST", headers: {Authorization: `Bearer ${grant}`}})).json();
window.dispatchEvent(new CustomEvent("linkstart:connected"));
await fetch(`${base}/v1/apps/${x.instanceId}/events`, {method: "POST", headers: {Authorization: `Bearer ${x.capability}`, "Content-Type": "application/json"}, body: JSON.stringify({eventId: crypto.randomUUID(), payload: {message: "hello"}})});
// then hold GET /v1/apps/{instanceId}/stream?cursor=0 open and render delivery_ack / feedback messages
```
