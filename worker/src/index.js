// Sideband signalling relay.
//
// This Worker never sees a video frame. Its entire job is to hold two blobs of
// text, an SDP offer and an SDP answer, long enough for two machines that
// cannot reach each other to swap them, after which the peers talk directly
// and this is out of the picture.
//
// The security model, and why it is shaped this way:
//
//   * The code is the only secret a viewer needs, because it is the only thing
//     that can be read aloud. Everything else is built around that constraint.
//
//   * The relay issues the code, not the host. A host that picked its own
//     could squat or collide, and per-IP limits would have nothing to attach
//     to.
//
//   * A session can be claimed exactly once. The first valid answer wins and
//     the offer becomes unreadable, so a code that has already been used is
//     worthless to anyone who later learns it.
//
//   * Only the host can read the answer. An answer contains the viewer's ICE
//     candidates, which include their public IP, leaving that world-readable
//     hands anyone who guesses a code the viewer's address, which they never
//     agreed to.
//
//   * State lives in a Durable Object rather than KV. Claiming is a
//     read-then-write, and KV cannot do that atomically: two viewers racing
//     would both see "unclaimed" and the attacker could overwrite the real
//     answer. A Durable Object serialises them.
//
// Endpoints:
//
//   POST   /api/session                  create; returns { code, token }
//   PUT    /api/session/:code/offer      host publishes  (token)
//   GET    /api/session/:code/offer      viewer fetches  (unclaimed only)
//   POST   /api/session/:code/answer     viewer claims
//   GET    /api/session/:code/answer      host collects   (token)
//   PUT    /api/session/:code/lock       host stops new viewers, "1" or "0" (token)
//   DELETE /api/session/:code            host purges     (token)
//   GET    /:code                        viewer page, code pre-filled
//
//   PUT    /api/room/:room               host says which code is live (room key)
//   GET    /api/room/:room               viewer asks whether anything is live
//   DELETE /api/room/:room/live          host finished sharing (room key)
//   DELETE /api/room/:room               host retires the link (room key)
//   GET    /r/:room                      viewer page for a permanent link
//
// A room is a link that does not change between sessions. The code is still
// what admits a viewer; the room only says which code is current, so a
// bookmarked link finds tonight's session without anybody sending a new one.
// Its name is the secret a viewer holds, twelve characters nobody can guess,
// and the key that updates it is held only by the host that first used it.

/// Long enough to read a code out over a call, short enough that a leaked one
/// is dead before it is useful.
// Twenty minutes, not five.
//
// Five was chosen for a code read aloud to somebody already waiting. The
// ordinary case is nothing like that: a code is generated on one machine and
// used on another, and the walk between them, or the message sent and not yet
// read, is easily longer than five minutes. A code that dies before it is used
// is not a security property, it is an errand.
const TTL_MS = 20 * 60 * 1000;

/// Excludes O/0/I/1/L, the characters people mishear and mistype. 31^6 is
/// about 8.9e8, which is only meaningful alongside the rate limits below.
const ALPHABET = "ABCDEFGHJKMNPQRSTUVWXYZ23456789";
const CODE_LENGTH = 6;
const CODE_RE = /^[A-Z2-9]{6}$/;

/// An SDP with a full candidate set is a few kilobytes.
const MAX_SDP = 64 * 1024;

/// Room names: the code alphabet in lower case, twelve long. 31^12 is about
/// 7.9e17, which nobody is sweeping at any rate this relay would allow.
const ROOM_RE = /^[a-hjkmnp-z2-9]{12}$/;

/// A room nobody has shared to in this long is forgotten. Long enough that a
/// link used once a month keeps working, short enough that abandoned ones do
/// not pile up for ever.
const ROOM_IDLE_MS = 180 * 24 * 60 * 60 * 1000;

/// Per-IP budgets. Creating sessions is cheap to do and expensive to absorb;
/// looking up codes is the enumeration path and is held much tighter.
///
/// Rooms get their own, looser, budget: a page left open on a bookmarked link
/// asks every few seconds whether anything is live, and a room name is far
/// too long to be worth sweeping for.
const LIMITS = {
  create: { tokens: 20, refillMs: 60_000 },
  lookup: { tokens: 30, refillMs: 60_000 },
  room: { tokens: 40, refillMs: 60_000 },
};

// ---------------------------------------------------------------------------
// Durable Objects
// ---------------------------------------------------------------------------

/// One signalling session. Single-threaded by construction, which is what
/// makes "claimed exactly once" true rather than merely likely.
export class SignallingSession {
  constructor(state) {
    this.state = state;
  }

  async fetch(request) {
    const url = new URL(request.url);
    const action = url.pathname.slice(1);
    const store = this.state.storage;

    if (action === "init") {
      if (await store.get("created")) {
        return json({ error: "taken" }, 409);
      }
      const { tokenHash } = await request.json();
      await store.put({ created: Date.now(), tokenHash, claimed: false });
      // Everything is torn down on the alarm even if the host never returns.
      await store.setAlarm(Date.now() + TTL_MS);
      return json({ ok: true });
    }

    const created = await store.get("created");
    if (!created) {
      return json({ error: "not found" }, 404);
    }

    switch (action) {
      case "put-offer": {
        if (!(await this.authorised(request))) return json({ error: "denied" }, 403);
        await store.put("offer", await request.text());

        // A fresh offer re-arms the code, and only the host can publish one,
        // because only the host holds the token. So the code works for as long
        // as this machine is sharing and stops the moment it is not.
        //
        // It was spent on first use before. That is the right rule for a code
        // handed to somebody else and the wrong one for the commonest case
        // there is: one person, two devices, where every reconnect meant
        // walking back to the first machine for a new one.
        await store.put("claimed", false);

        // And the clock restarts, so a stream outlasting the expiry does not
        // have the relay delete the session out from under it.
        await store.setAlarm(Date.now() + TTL_MS);
        return json({ ok: true });
      }

      case "lock": {
        if (!(await this.authorised(request))) return json({ error: "denied" }, 403);
        await store.put("locked", (await request.text()).trim() === "1");
        return json({ ok: true });
      }

      case "get-offer": {
        // Said before anything else, and said as its own status, so the
        // viewer can tell "not letting anyone in right now" from "no such
        // stream" and wait rather than give up.
        if (await store.get("locked")) return json({ error: "locked" }, 423);
        // A claimed session is finished. Refusing here is what makes a code
        // worthless once it has been used, rather than merely stale.
        if (await store.get("claimed")) return json({ error: "already claimed" }, 410);
        const offer = await store.get("offer");
        return offer ? text(offer, 200, "application/sdp") : json({ error: "not ready" }, 404);
      }

      case "answer": {
        if (await store.get("locked")) return json({ error: "locked" }, 423);
        // The claim and the write happen together, so a second answer cannot
        // land between another viewer's check and their write.
        if (await store.get("claimed")) return json({ error: "already claimed" }, 409);
        const body = await request.text();
        if (body.length > MAX_SDP) return json({ error: "too large" }, 413);
        await store.put({ claimed: true, answer: body, claimedAt: Date.now() });
        return json({ ok: true });
      }

      case "get-answer": {
        if (!(await this.authorised(request))) return json({ error: "denied" }, 403);
        const answer = await store.get("answer");
        if (!answer) return json({ error: "not yet" }, 404);
        // Handed over once. The host has it now, and nothing is served from
        // here again.
        await store.delete("answer");
        return text(answer, 200, "application/sdp");
      }

      case "destroy": {
        if (!(await this.authorised(request))) return json({ error: "denied" }, 403);
        await store.deleteAll();
        await store.deleteAlarm();
        return json({ ok: true });
      }

      default:
        return json({ error: "not found" }, 404);
    }
  }

  /// Compares the presented token against the stored hash. The plaintext token
  /// is never written down, so reading this object's storage does not yield
  /// something that can be replayed.
  async authorised(request) {
    const presented = (request.headers.get("authorization") || "").replace(/^Bearer\s+/i, "");
    if (!presented) return false;
    const expected = await this.state.storage.get("tokenHash");
    if (!expected) return false;
    return timingSafeEqual(await sha256Hex(presented), expected);
  }

  async alarm() {
    await this.state.storage.deleteAll();
  }
}

/// A permanent link: which code, if any, is live under it right now.
///
/// Trust on first use. The first host to publish to a room sets its key, and
/// from then on only that key can change it. Nobody else can get there first,
/// because the name is generated on the host and nobody else knows it until
/// the host has already used it.
export class Room {
  constructor(state) {
    this.state = state;
  }

  async fetch(request) {
    const url = new URL(request.url);
    const action = url.pathname.slice(1);
    const store = this.state.storage;

    if (action === "get") {
      const code = await store.get("code");
      return json(code ? { live: true, code } : { live: false });
    }

    const presented = (request.headers.get("authorization") || "").replace(/^Bearer\s+/i, "");
    if (!presented) return json({ error: "denied" }, 403);
    const hash = await sha256Hex(presented);
    const known = await store.get("keyHash");

    if (action === "publish") {
      if (known && !timingSafeEqual(hash, known)) return json({ error: "denied" }, 403);
      let code = "";
      try {
        code = String((JSON.parse(await request.text()) || {}).code || "");
      } catch (_) {}
      if (!CODE_RE.test(code)) return json({ error: "bad code" }, 400);
      await store.put({ keyHash: hash, code, liveAt: Date.now() });
      await store.setAlarm(Date.now() + ROOM_IDLE_MS);
      return json({ ok: true });
    }

    // Everything else needs a room that exists and the key that made it.
    if (!known || !timingSafeEqual(hash, known)) return json({ error: "denied" }, 403);

    if (action === "clear") {
      await store.delete("code");
      return json({ ok: true });
    }

    if (action === "forget") {
      await store.deleteAll();
      await store.deleteAlarm();
      return json({ ok: true });
    }

    return json({ error: "not found" }, 404);
  }

  async alarm() {
    await this.state.storage.deleteAll();
  }
}

/// A token bucket per client address. Without this the 404-vs-200 difference
/// on a code lookup is a clean oracle: an attacker cannot guess a *specific*
/// code, but they can scan for any live one, and unmetered scanning is what
/// makes that practical.
export class RateLimiter {
  constructor(state) {
    this.state = state;
  }

  async fetch(request) {
    const { tokens: capacity, refillMs } = await request.json();
    const now = Date.now();

    const bucket = (await this.state.storage.get("bucket")) || {
      tokens: capacity,
      updated: now,
    };

    // Continuous refill, so a client that waits is not punished for a burst
    // it made a minute ago.
    const gained = ((now - bucket.updated) / refillMs) * capacity;
    const tokens = Math.min(capacity, bucket.tokens + gained);

    if (tokens < 1) {
      await this.state.storage.put("bucket", { tokens, updated: now });
      return json({ allowed: false, retryAfter: Math.ceil(refillMs / capacity / 1000) }, 200);
    }

    await this.state.storage.put("bucket", { tokens: tokens - 1, updated: now });
    return json({ allowed: true }, 200);
  }
}

// ---------------------------------------------------------------------------
// Worker
// ---------------------------------------------------------------------------

export default {
  async fetch(request, env) {
    const url = new URL(request.url);
    const parts = url.pathname.split("/").filter(Boolean);

    if (parts[0] === "api" && parts[1] === "session") {
      return api(request, env, parts.slice(2));
    }

    if (parts[0] === "api" && parts[1] === "room") {
      return roomApi(request, env, parts.slice(2));
    }

    // A permanent link. Anything that is not a well formed room name gets the
    // ordinary page rather than an error, the same as a mistyped code does.
    if (request.method === "GET" && parts[0] === "r") {
      const room = (parts[1] || "").toLowerCase();
      return html(page("", ROOM_RE.test(room) ? room : ""));
    }

    // Where to find a route, asked by the host at the start of a session.
    //
    // This exists so that nothing has to be configured on the machine doing
    // the sharing. Set the TURN credentials once, here, as Worker secrets, and
    // every machine that points at this relay picks them up: a new laptop, a
    // reinstall, somebody else's computer. Sending a link stays the whole of
    // what anybody has to do.
    if (parts[0] === "api" && parts[1] === "ice") {
      return json(await iceServers(env));
    }

    if (request.method === "GET") {
      const pre = (parts[0] || "").toUpperCase();
      return html(page(CODE_RE.test(pre) ? pre : ""));
    }

    return json({ error: "not found" }, 404);
  },
};

/// STUN always, and TURN as well when this relay has been given credentials.
///
/// TURN matters for the viewers hole punching cannot reach: anybody on mobile
/// data, where the carrier puts every subscriber behind one shared address and
/// there is nothing on the far side to punch through, and anybody on a network
/// that keeps its own devices apart.
///
/// Credentials are minted per session and last a day, so nothing long lived is
/// handed out and nothing long lived is stored anywhere but here.
async function iceServers(env) {
  const stun = {
    urls: ["stun:stun.cloudflare.com:3478", "stun:stun.l.google.com:19302"],
  };

  if (!env.TURN_KEY_ID || !env.TURN_TOKEN) {
    return { iceServers: [stun], turn: false };
  }

  try {
    const res = await fetch(
      `https://rtc.live.cloudflare.com/v1/turn/keys/${env.TURN_KEY_ID}/credentials/generate-ice-servers`,
      {
        method: "POST",
        headers: {
          Authorization: `Bearer ${env.TURN_TOKEN}`,
          "content-type": "application/json",
        },
        body: JSON.stringify({ ttl: 86400 }),
      },
    );
    if (!res.ok) return { iceServers: [stun], turn: false };

    const body = await res.json();
    const got = Array.isArray(body.iceServers) ? body.iceServers : [body.iceServers];
    return { iceServers: [stun, ...got.filter(Boolean)], turn: true };
  } catch (_) {
    // A relay that cannot mint credentials is still a working relay for
    // everybody who did not need them.
    return { iceServers: [stun], turn: false };
  }
}

async function api(request, env, rest) {
  // Create.
  if (rest.length === 0) {
    if (request.method !== "POST") return json({ error: "method not allowed" }, 405);
    if (!(await allow(request, env, "create"))) return json({ error: "slow down" }, 429);
    return createSession(env);
  }

  const code = (rest[0] || "").toUpperCase();
  if (!CODE_RE.test(code)) return json({ error: "bad code" }, 400);

  const object = env.SESSIONS.get(env.SESSIONS.idFromName(code));
  const action = rest[1];

  if (action === "offer" && request.method === "PUT") {
    const body = await request.text();
    if (body.length > MAX_SDP) return json({ error: "too large" }, 413);
    return forward(object, "put-offer", request, body);
  }

  if (action === "offer" && request.method === "GET") {
    // Rate limited: this is the endpoint an attacker would sweep.
    if (!(await allow(request, env, "lookup"))) return json({ error: "slow down" }, 429);
    return forward(object, "get-offer", request);
  }

  if (action === "answer" && request.method === "POST") {
    if (!(await allow(request, env, "lookup"))) return json({ error: "slow down" }, 429);
    return forward(object, "answer", request, await request.text());
  }

  if (action === "answer" && request.method === "GET") {
    return forward(object, "get-answer", request);
  }

  if (action === "lock" && request.method === "PUT") {
    return forward(object, "lock", request, (await request.text()).slice(0, 8));
  }

  if (!action && request.method === "DELETE") {
    return forward(object, "destroy", request);
  }

  return json({ error: "not found" }, 404);
}

async function roomApi(request, env, rest) {
  const room = (rest[0] || "").toLowerCase();
  if (!ROOM_RE.test(room)) return json({ error: "bad room" }, 400);

  const object = env.ROOMS.get(env.ROOMS.idFromName(room));
  const action = rest[1];

  if (!action && request.method === "GET") {
    if (!(await allow(request, env, "room"))) return json({ error: "slow down" }, 429);
    return forward(object, "get", request);
  }

  if (!action && request.method === "PUT") {
    // Metered like creating a session, because the first publish to a name
    // is what brings a room into existence.
    if (!(await allow(request, env, "create"))) return json({ error: "slow down" }, 429);
    const body = await request.text();
    if (body.length > 256) return json({ error: "too large" }, 413);
    return forward(object, "publish", request, body);
  }

  if (action === "live" && request.method === "DELETE") {
    return forward(object, "clear", request);
  }

  if (!action && request.method === "DELETE") {
    return forward(object, "forget", request);
  }

  return json({ error: "not found" }, 404);
}

async function createSession(env) {
  const token = randomToken();
  const tokenHash = await sha256Hex(token);

  // A collision means someone else holds that code right now, not that it is
  // unusable forever; a few attempts is plenty at this code space.
  for (let attempt = 0; attempt < 5; attempt++) {
    const code = randomCode();
    const object = env.SESSIONS.get(env.SESSIONS.idFromName(code));
    const created = await object.fetch("https://do/init", {
      method: "POST",
      body: JSON.stringify({ tokenHash }),
    });

    if (created.ok) {
      return json({ code, token, expiresInSeconds: TTL_MS / 1000 });
    }
    if (created.status !== 409) {
      return json({ error: "could not create a session" }, 500);
    }
  }

  return json({ error: "could not allocate a code" }, 503);
}

function forward(object, action, request, body) {
  return object.fetch(`https://do/${action}`, {
    method: "POST",
    headers: { authorization: request.headers.get("authorization") || "" },
    body,
  });
}

async function allow(request, env, kind) {
  const ip = request.headers.get("cf-connecting-ip") || "unknown";
  const limiter = env.LIMITS.get(env.LIMITS.idFromName(`${kind}:${ip}`));
  const verdict = await limiter.fetch("https://do/check", {
    method: "POST",
    body: JSON.stringify(LIMITS[kind]),
  });
  const { allowed } = await verdict.json();
  return allowed;
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Rejection sampling rather than a modulo, so every character is equally
/// likely. The bias would be tiny either way, but "tiny" is not a property
/// worth defending when uniform is this cheap.
function randomCode() {
  const out = [];
  const limit = 256 - (256 % ALPHABET.length);
  while (out.length < CODE_LENGTH) {
    const bytes = crypto.getRandomValues(new Uint8Array(CODE_LENGTH * 2));
    for (const b of bytes) {
      if (b < limit && out.length < CODE_LENGTH) {
        out.push(ALPHABET[b % ALPHABET.length]);
      }
    }
  }
  return out.join("");
}

/// 256 bits. This is never spoken aloud, so there is no reason to be short.
function randomToken() {
  const bytes = crypto.getRandomValues(new Uint8Array(32));
  return [...bytes].map((b) => b.toString(16).padStart(2, "0")).join("");
}

async function sha256Hex(value) {
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(value));
  return [...new Uint8Array(digest)].map((b) => b.toString(16).padStart(2, "0")).join("");
}

/// Compares without an early exit. Both values here are hex digests of the
/// same length, so this is belt and braces, but a comparison that leaks its
/// progress is never the one you want guarding an authorisation check.
function timingSafeEqual(a, b) {
  if (a.length !== b.length) return false;
  let diff = 0;
  for (let i = 0; i < a.length; i++) {
    diff |= a.charCodeAt(i) ^ b.charCodeAt(i);
  }
  return diff === 0;
}

function json(body, status = 200) {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json", "cache-control": "no-store" },
  });
}

function text(body, status = 200, type = "text/plain") {
  return new Response(body, {
    status,
    headers: { "content-type": type, "cache-control": "no-store" },
  });
}

function html(body) {
  return new Response(body, {
    headers: {
      "content-type": "text/html; charset=utf-8",
      "cache-control": "no-store",
      // The page loads nothing external and is never framed.
      "content-security-policy":
        "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src data:; media-src blob: mediastream:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'",
      "referrer-policy": "no-referrer",
      "x-content-type-options": "nosniff",
    },
  });
}

function page(prefill, room = "") {
  return `<!doctype html>
<meta charset="utf-8">
<title>Sideband</title>
<meta name="viewport" content="width=device-width,initial-scale=1">
<link rel="icon" href="data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 512 512'%3E%3Crect width='512' height='512' rx='114' fill='%2314181d'/%3E%3Cg fill='%23f0a93b'%3E%3Crect x='214' y='86' width='84' height='340' rx='42'/%3E%3Crect x='332' y='146' width='84' height='220' rx='42'/%3E%3Crect x='96' y='146' width='84' height='220' rx='42' opacity='.3'/%3E%3C/g%3E%3C/svg%3E">
<style>
  :root { color-scheme: dark; }
  * { box-sizing: border-box; }
  body { margin:0; height:100vh; background:#14181d; color:#e4e9ee;
    font:15px/1.5 ui-sans-serif,system-ui,"Segoe UI",sans-serif; display:grid; place-items:center; }
  #stage { width:100%; height:100%; display:grid; place-items:center; }
  /* An explicit width as well as a maximum, and that is not a detail.
     With only a maximum, a video is laid out at its own pixel size, so a
     stream sent at a reduced resolution arrived as a postage stamp in the
     middle of a black page rather than as a slightly soft full sized
     picture. Every argument for sending a smaller picture assumed the
     browser would scale it back up, and it never had any reason to. */
  video { width:100%; height:100vh; object-fit:contain;
          display:none; background:#000; }
  #panel { text-align:center; }
  h1 { font-size:15px; font-weight:600; letter-spacing:.14em; text-transform:uppercase;
    color:#f0a93b; margin:0 0 6px; }
  /* Geometry from src/mark.rs, the window icon and the executable's own
     icon are rasterised from the same numbers. */
  .mark { width:38px; height:38px; display:block; margin:0 auto 10px; }
  p { color:#98a4b1; margin:0 0 18px; }
  input { font:inherit; font-size:22px; font-weight:600; letter-spacing:.3em; text-align:center;
    text-transform:uppercase; width:220px; padding:10px; border-radius:4px; border:1px solid #3d4652;
    background:#1b2027; color:#e4e9ee; margin-bottom:14px; }
  input:focus-visible, button:focus-visible { outline:2px solid #f0a93b; outline-offset:3px; }
  button { display:block; margin:0 auto; font:inherit; font-weight:600; color:#14181d;
    background:#f0a93b; border:0; border-radius:4px; padding:11px 26px; cursor:pointer; }
  button:disabled { background:#3d4652; color:#98a4b1; cursor:default; }
  /* Over the picture, for the one case where a phone refused to start it
     with sound: tapping this is the gesture it was waiting for. */
  #unmute { position:fixed; left:50%; bottom:72px; transform:translateX(-50%); z-index:2; }
  /* The numbers, for when somebody far away says it is not working and
     nobody is going to talk them through opening developer tools. */
  #stats { position:fixed; top:10px; left:10px; z-index:3; margin:0; padding:10px 12px;
    background:rgba(20,24,29,.86); border:1px solid #2c333c; border-radius:4px;
    color:#e4e9ee; font:11px/1.55 ui-monospace,Consolas,monospace; white-space:pre; }
  /* The attribute has to win over the display rules above, or a hidden
     button is still a visible one. */
  [hidden] { display:none !important; }
</style>

<div id="stage">
  <div id="panel">
    <svg class="mark" viewBox="0 0 512 512" aria-hidden="true">
      <g fill="#f0a93b">
        <rect x="226" y="96" width="60" height="320" rx="30"/>
        <rect x="314" y="156" width="60" height="200" rx="30"/>
        <rect x="402" y="198" width="60" height="116" rx="30"/>
        <rect x="138" y="156" width="60" height="200" rx="30" opacity=".28"/>
        <rect x="50" y="198" width="60" height="116" rx="30" opacity=".28"/>
      </g>
    </svg>
    <h1>Sideband</h1>
    <p id="status">${room ? "Tap Watch. It starts by itself whenever they are sharing." : "Enter the code you were given."}</p>
    <input id="code" maxlength="6" autocomplete="off" spellcheck="false" value="${prefill}"${room ? " hidden" : ""}>
    <button id="go">Watch</button>
  </div>
  <video id="v" autoplay playsinline controls></video>
  <button id="unmute" hidden>Tap for sound</button>
  <pre id="stats" hidden></pre>
</div>

<script>
// A permanent link names a room rather than a code. The room says which code
// is live right now, so the same bookmark finds every session.
const ROOM = '${room}';

const statusEl = document.getElementById('status');
const button = document.getElementById('go');
const input = document.getElementById('code');
const video = document.getElementById('v');
const panel = document.getElementById('panel');
const unmute = document.getElementById('unmute');
const say = (t) => { statusEl.textContent = t; };
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// Who this is, for the host's list of people watching. A random name kept in
// this browser, so somebody the host removed is recognised if they come
// straight back, and a rough description of the device, so the list reads
// "Android, Chrome" rather than an address nobody recognises. Nothing here
// identifies a person, and nothing leaves this page except to the host.
function randomId() {
  const b = crypto.getRandomValues(new Uint8Array(12));
  return Array.from(b, (x) => x.toString(16).padStart(2, '0')).join('');
}
const VIEWER = (() => {
  try {
    let id = localStorage.getItem('sideband-viewer');
    if (!id) { id = randomId(); localStorage.setItem('sideband-viewer', id); }
    return id;
  } catch (_) {
    return randomId();
  }
})();
function device() {
  const ua = navigator.userAgent;
  const os = /iPhone/.test(ua) ? 'iPhone' : /iPad/.test(ua) ? 'iPad'
    : /Android/.test(ua) ? 'Android' : /Windows/.test(ua) ? 'Windows'
    : /CrOS/.test(ua) ? 'Chromebook' : /Mac OS X/.test(ua) ? 'Mac'
    : /Linux/.test(ua) ? 'Linux' : 'Device';
  const browser = /Edg[/]/.test(ua) ? 'Edge' : /OPR[/]|Opera/.test(ua) ? 'Opera'
    : /Firefox[/]|FxiOS/.test(ua) ? 'Firefox' : /SamsungBrowser/.test(ua) ? 'Samsung Internet'
    : /Chrome[/]|CriOS/.test(ua) ? 'Chrome' : /Safari[/]/.test(ua) ? 'Safari' : 'browser';
  return os + ', ' + browser;
}

// A phone that goes to sleep in the middle of a stream takes the stream with
// it. Held only while something is actually on screen, and asked for again
// when the page comes back to the front, because the browser lets go of it
// whenever the page is hidden.
let wake = null;
let watching = false;
async function keepAwake() {
  try {
    if (!wake && 'wakeLock' in navigator) {
      wake = await navigator.wakeLock.request('screen');
      wake.addEventListener('release', () => { wake = null; });
    }
  } catch (_) {
    // Refused, or not supported. The stream works either way.
  }
}
function letSleep() {
  if (wake) { wake.release().catch(() => {}); wake = null; }
}
document.addEventListener('visibilitychange', () => {
  if (document.visibilityState === 'visible' && watching) keepAwake();
});

function showVideo() {
  watching = true;
  panel.style.display = 'none';
  video.style.display = 'block';
  keepAwake();
}
function showPanel(msg) {
  watching = false;
  say(msg);
  panel.style.display = '';
  video.style.display = 'none';
  unmute.hidden = true;
  letSleep();
}

// Phones in particular refuse to start a video with sound unless it happens
// in answer to a tap. Muted playback is always allowed, so the picture starts
// regardless and the sound is one tap away rather than the whole thing
// sitting on a black frame with no explanation.
function play() {
  const started = video.play();
  if (started && started.catch) {
    started.catch(() => {
      video.muted = true;
      video.play().catch(() => {});
      unmute.hidden = false;
    });
  }
}
unmute.onclick = () => {
  video.muted = false;
  video.play().catch(() => {});
  unmute.hidden = true;
};

// Press s for the numbers. What they mean, when a picture freezes:
//   keyframes climbing and frames decoded stuck: it is waiting for a
//     keyframe that is not arriving.
//   lost and repairs climbing: the network is dropping packets.
//   dropped climbing with a software decoder: this device cannot keep up.
const statsEl = document.getElementById('stats');
let showStats = false;
let statsTimer = null;

document.addEventListener('keydown', (e) => {
  if (e.key !== 's' && e.key !== 'S') return;
  if (e.target === input) return;
  showStats = !showStats;
  statsEl.hidden = !showStats;
  clearInterval(statsTimer);
  if (showStats) {
    readStats();
    statsTimer = setInterval(readStats, 1000);
  }
});

let lastStats = null;
async function readStats() {
  if (!window.pc) { statsEl.textContent = 'not connected'; return; }
  let video = null, candidate = null;
  try {
    const report = await window.pc.getStats();
    report.forEach((s) => {
      if (s.type === 'inbound-rtp' && s.kind === 'video') video = s;
      if (s.type === 'candidate-pair' && s.nominated) candidate = s;
    });
  } catch (_) {
    return;
  }
  if (!video) { statsEl.textContent = 'no video yet'; return; }

  const since = lastStats && video.timestamp > lastStats.timestamp
    ? (video.timestamp - lastStats.timestamp) / 1000
    : 0;
  const per = (now, before) => (since ? ((now - before) / since).toFixed(1) : '-');
  const kbps = since && lastStats
    ? (((video.bytesReceived - lastStats.bytesReceived) * 8) / since / 1000).toFixed(0)
    : '-';

  const rows = [
    ['picture', (video.frameWidth || 0) + 'x' + (video.frameHeight || 0) + '  ' + (video.framesPerSecond || 0) + ' fps'],
    ['bitrate', kbps + ' kbit/s'],
    ['decoded', video.framesDecoded + '  (+' + (lastStats ? per(video.framesDecoded, lastStats.framesDecoded) : '-') + '/s)'],
    ['keyframes', video.keyFramesDecoded],
    ['lost', video.packetsLost],
    ['repairs asked', 'nack ' + video.nackCount + '  picture ' + video.pliCount],
    ['dropped', video.framesDropped],
    ['freezes', (video.freezeCount === undefined ? '-' : video.freezeCount) + '  ' + Math.round((video.totalFreezesDuration || 0) * 10) / 10 + 's'],
    ['jitter', Math.round((video.jitter || 0) * 1000) + ' ms  buffer ' + Math.round((video.jitterBufferDelay / Math.max(video.jitterBufferEmittedCount, 1)) * 1000) + ' ms'],
    ['decoder', video.decoderImplementation || '-'],
    ['route', candidate ? (candidate.currentRoundTripTime * 1000).toFixed(0) + ' ms round trip' : '-'],
  ];
  lastStats = video;
  statsEl.textContent = rows.map((r) => r[0].padEnd(15) + r[1]).join('\\n');
}

input.addEventListener('keydown', (e) => { if (e.key === 'Enter') button.click(); });
button.onclick = () => {
  // Blesses the element while there is still a tap to bless it with, so the
  // stream that arrives later, after all the waiting, may play with sound.
  video.muted = false;
  video.play().catch(() => {});
  return ROOM ? watchRoom() : watchCode();
};

// Resolves once the connection is over for good. Disconnected is given a few
// seconds first: it is what a brief blip looks like, and connections come
// back from it all the time.
function ended(pc) {
  return new Promise((resolve) => {
    let timer = null;
    const check = () => {
      const s = pc.connectionState;
      if (s === 'failed' || s === 'closed') {
        clearTimeout(timer);
        resolve();
      } else if (s === 'disconnected') {
        if (!timer) {
          timer = setTimeout(() => {
            if (pc.connectionState !== 'connected') resolve();
          }, 5000);
        }
      } else if (s === 'connected') {
        clearTimeout(timer);
        timer = null;
      }
    };
    pc.addEventListener('connectionstatechange', check);
    check();
  });
}

async function watchCode() {
  const code = input.value.trim().toUpperCase();
  if (!/^[A-Z2-9]{6}$/.test(code)) { say('That code does not look right.'); return; }
  button.disabled = true;
  input.disabled = true;

  try {
    const pc = await connectWithRetry(code, say, 25000);
    showVideo();
    await ended(pc);
    try { pc.close(); } catch (_) {}
    showPanel('Disconnected.');
  } catch (e) {
    showPanel(e.message);
  }
  button.disabled = false;
  input.disabled = false;
}

// Watches for as long as the page is open. Asks the room what is live, joins
// it, and when it ends, whether they stopped sharing or the connection
// dropped, goes back to asking. Nobody has to send a new link or press
// anything again.
async function watchRoom() {
  button.hidden = true;
  for (;;) {
    let code = '';
    try {
      const res = await fetch('/api/room/' + ROOM, { cache: 'no-store' });
      if (res.status === 429) {
        say('Checking too often. Waiting a moment…');
        await sleep(15000);
        continue;
      }
      if (res.ok) {
        const body = await res.json();
        if (body.live) code = body.code;
      }
    } catch (_) {
      say('Could not reach the server. Trying again…');
      await sleep(5000);
      continue;
    }

    if (!code) {
      say('Not live right now. This starts by itself when they are.');
      await sleep(5000);
      continue;
    }

    try {
      const pc = await connectWithRetry(code, say, 25000);
      showVideo();
      await ended(pc);
      try { pc.close(); } catch (_) {}
      showPanel('The stream stopped. Waiting for it to come back…');
    } catch (e) {
      say(e.message);
      await sleep(4000);
    }
  }
}

// Waits for ICE gathering on evidence rather than a fixed deadline: as soon as
// a usable set of candidates exists we go, and we only give up early if the
// browser produced nothing at all. A fixed timeout would post an answer with
// too few candidates on a slower network, which fails to connect and looks
// like "it needs a retry".
async function gather(pc, { minCandidates = 2, settle = 700, hardCap = 12000 } = {}) {
  if (pc.iceGatheringState === 'complete') return;

  let count = 0;
  let lastAt = 0;
  const started = Date.now();
  pc.addEventListener('icecandidate', (e) => {
    if (e.candidate) { count++; lastAt = Date.now(); }
  });

  while (Date.now() - started < hardCap) {
    if (pc.iceGatheringState === 'complete') return;
    if (count >= minCandidates && lastAt && Date.now() - lastAt > settle) return;
    await sleep(100);
  }

  if (count === 0) throw new Error('No network route found. A firewall or VPN may be blocking it.');
}

const LOCKED = 'They are not letting anyone new in right now.';

async function attempt(code, onStatus) {
  onStatus('Looking up the stream…');
  const res = await fetch('/api/session/' + code + '/offer', { cache: 'no-store' });
  // Somebody is on this code right now. That is no longer final: when a
  // viewer leaves, the host publishes a new offer under the same code within a
  // few seconds, so this is worth waiting through rather than giving up on.
  if (res.status === 410) throw new Error('Someone is watching. Waiting…');
  if (res.status === 423) throw new Error(LOCKED);
  if (res.status === 429) throw new Error('Too many attempts. Wait a moment.');
  if (res.status === 404) throw new Error('No stream with that code. It may have expired.');
  if (!res.ok) throw new Error('Could not reach the server.');
  const offer = await res.text();

  // The same route options the host was given, from the same place, so a
  // relay of last resort set up once on this Worker helps both ends. Without
  // this the viewer would only ever try STUN, and a phone on mobile data, which
  // is exactly who a relay is for, would be the one end not using it.
  let iceServers = [
    { urls: 'stun:stun.cloudflare.com:3478' },
    { urls: 'stun:stun.l.google.com:19302' },
  ];
  try {
    const ice = await fetch('/api/ice', { cache: 'no-store' });
    if (ice.ok) {
      const body = await ice.json();
      if (Array.isArray(body.iceServers) && body.iceServers.length) iceServers = body.iceServers;
    }
  } catch (_) {
    // STUN alone is still a working default for most connections.
  }

  const pc = new RTCPeerConnection({ iceServers });
  window.pc = pc;

  // Attached before setRemoteDescription: a track can arrive the moment the
  // description is applied, and a listener added afterwards would miss it.
  pc.ontrack = (e) => {
    if (video.srcObject !== e.streams[0]) {
      video.srcObject = e.streams[0];
      play();
    }
    // A little slack, for Wi-Fi.
    //
    // The browser holds arriving packets for a few tens of milliseconds
    // before it has to decode them, and a retransmitted packet that arrives
    // after that window is wasted: the frame is already late, so the picture
    // breaks anyway. A target of 150 ms is a delay nobody watching a game
    // over the internet will notice, and it is long enough for a lost packet
    // to be asked for and arrive before the frame it belongs to is due.
    try {
      for (const receiver of pc.getReceivers()) {
        if ('jitterBufferTarget' in receiver) receiver.jitterBufferTarget = 150;
      }
    } catch (_) {
      // Older browsers decide for themselves.
    }
  };

  // Seventy seconds, not twenty five. The host may have to press "allow",
  // and it asks for a minute before giving up on an answer; a page that gave
  // up first was a viewer let in to a connection they had already abandoned.
  // A route that genuinely fails says so long before this, as "failed".
  const connected = new Promise((resolve, reject) => {
    pc.addEventListener('connectionstatechange', () => {
      if (pc.connectionState === 'connected') resolve();
      if (pc.connectionState === 'failed') reject(new Error('Connection failed.'));
    });
    setTimeout(() => reject(new Error('Timed out connecting.')), 70000);
  });

  await pc.setRemoteDescription({ type: 'offer', sdp: offer });
  await pc.setLocalDescription(await pc.createAnswer());

  onStatus('Finding a route…');
  await gather(pc);

  onStatus('Connecting…');
  // Two lines ahead of the answer itself, for the host's list of people
  // watching. The host takes them off again before the answer is used.
  const about = 'x-sideband-viewer:' + VIEWER + '\\r\\n' + 'x-sideband-device:' + device() + '\\r\\n';
  const post = await fetch('/api/session/' + code + '/answer', {
    method: 'POST',
    body: about + pc.localDescription.sdp,
  });
  if (post.status === 409) throw new Error('Someone else is already watching with that code.');
  if (post.status === 423) throw new Error(LOCKED);
  if (!post.ok) throw new Error('Could not send the reply.');

  onStatus('Connecting… If they have to let you in, this waits for them.');
  await connected;
  return pc;
}

// Bounded, and bounded is the point.
//
// Some states look like failure and are not: the host rebuilding its offer
// after a viewer left, a connection that simply did not take, and a host that
// has stopped letting people in for a moment. All of them clear by waiting.
// An earlier version waited through them with no limit and sat repeating
// itself for ever when they did not clear, which is a worse way to fail than
// saying so. Anything that cannot come right by waiting, a code that never
// existed or a rate limit, is not waited on at all.
async function connectWithRetry(code, onStatus, patience) {
  const deadline = Date.now() + patience;
  for (;;) {
    try {
      return await attempt(code, onStatus);
    } catch (e) {
      if (window.pc) { try { window.pc.close(); } catch (_) {} }
      if (/expired|Too many|not valid|No stream/.test(e.message)) throw e;
      if (Date.now() > deadline) {
        throw new Error(/watching/.test(e.message)
          ? 'Someone else is watching this one.'
          : e.message);
      }
      const locked = e.message === LOCKED;
      onStatus(/watching/.test(e.message)
        ? 'Waiting for the stream to free up…'
        : locked ? LOCKED + ' Waiting…'
        : 'That did not take - trying again…');
      await sleep(locked ? 4000 : 1500);
    }
  }
}
</script>`;
}
