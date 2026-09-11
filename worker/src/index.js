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
//   DELETE /api/session/:code            host purges     (token)
//   GET    /:code                        viewer page, code pre-filled

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

/// Per-IP budgets. Creating sessions is cheap to do and expensive to absorb;
/// looking up codes is the enumeration path and is held much tighter.
const LIMITS = {
  create: { tokens: 20, refillMs: 60_000 },
  lookup: { tokens: 30, refillMs: 60_000 },
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

      case "get-offer": {
        // A claimed session is finished. Refusing here is what makes a code
        // worthless once it has been used, rather than merely stale.
        if (await store.get("claimed")) return json({ error: "already claimed" }, 410);
        const offer = await store.get("offer");
        return offer ? text(offer, 200, "application/sdp") : json({ error: "not ready" }, 404);
      }

      case "answer": {
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

  if (!action && request.method === "DELETE") {
    return forward(object, "destroy", request);
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

function page(prefill) {
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
    <p id="status">Enter the code you were given.</p>
    <input id="code" maxlength="6" autocomplete="off" spellcheck="false" value="${prefill}">
    <button id="go">Watch</button>
  </div>
  <video id="v" autoplay playsinline controls></video>
</div>

<script>
const statusEl = document.getElementById('status');
const button = document.getElementById('go');
const input = document.getElementById('code');
const video = document.getElementById('v');
const panel = document.getElementById('panel');
const say = (t) => { statusEl.textContent = t; };

input.addEventListener('keydown', (e) => { if (e.key === 'Enter') button.click(); });

button.onclick = async () => {
  const code = input.value.trim().toUpperCase();
  if (!/^[A-Z2-9]{6}$/.test(code)) { say('That code does not look right.'); return; }
  button.disabled = true;
  input.disabled = true;

  const reset = (msg) => {
    say(msg);
    panel.style.display = '';
    video.style.display = 'none';
    button.disabled = false;
    input.disabled = false;
  };

  try {
    const pc = await connectWithRetry(code, say, reset);
    panel.style.display = 'none';
    video.style.display = 'block';
    pc.onconnectionstatechange = () => {
      if (pc.connectionState === 'failed' || pc.connectionState === 'disconnected') {
        reset('Disconnected.');
      }
    };
  } catch (e) {
    reset(e.message);
  }
};

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
    await new Promise((r) => setTimeout(r, 100));
  }

  if (count === 0) throw new Error('No network route found. A firewall or VPN may be blocking it.');
}

async function attempt(code, onStatus) {
  onStatus('Looking up the stream…');
  const res = await fetch('/api/session/' + code + '/offer', { cache: 'no-store' });
  // Somebody is on this code right now. That is no longer final: when a
  // viewer leaves, the host publishes a new offer under the same code within a
  // few seconds, so this is worth waiting through rather than giving up on.
  if (res.status === 410) throw new Error('Someone is watching. Waiting…');
  if (res.status === 429) throw new Error('Too many attempts. Wait a moment.');
  if (res.status === 404) throw new Error('No stream with that code. It may have expired.');
  if (!res.ok) throw new Error('Could not reach the server.');
  const offer = await res.text();

  const pc = new RTCPeerConnection({
    iceServers: [
      { urls: 'stun:stun.cloudflare.com:3478' },
      { urls: 'stun:stun.l.google.com:19302' },
    ],
  });
  window.pc = pc;

  // Attached before setRemoteDescription: a track can arrive the moment the
  // description is applied, and a listener added afterwards would miss it.
  pc.ontrack = (e) => {
    if (video.srcObject !== e.streams[0]) video.srcObject = e.streams[0];
  };

  const connected = new Promise((resolve, reject) => {
    pc.addEventListener('connectionstatechange', () => {
      if (pc.connectionState === 'connected') resolve();
      if (pc.connectionState === 'failed') reject(new Error('Connection failed.'));
    });
    setTimeout(() => reject(new Error('Timed out connecting.')), 25000);
  });

  await pc.setRemoteDescription({ type: 'offer', sdp: offer });
  await pc.setLocalDescription(await pc.createAnswer());

  onStatus('Finding a route…');
  await gather(pc);

  onStatus('Connecting…');
  const post = await fetch('/api/session/' + code + '/answer', {
    method: 'POST',
    body: pc.localDescription.sdp,
  });
  if (post.status === 409) throw new Error('Someone else is already watching with that code.');
  if (!post.ok) throw new Error('Could not send the reply.');

  await connected;
  return pc;
}

// One retry only, and only for failures that happen after the code was
// accepted, a claimed or expired code will not become valid by asking again.
async function connectWithRetry(code, onStatus, onFailure) {
  // Bounded, and bounded is the point.
  //
  // Two states look like failure and are not: the host rebuilding its offer
  // after a viewer left, and a connection that simply did not take. Both clear
  // within seconds. An earlier version waited through them with no limit and
  // sat repeating itself for ever when they did not clear, which is a worse
  // way to fail than saying so. Anything that cannot come right by waiting,
  // a code that never existed or a rate limit, is not waited on at all.
  const deadline = Date.now() + 25000;
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
      onStatus(/watching/.test(e.message)
        ? 'Waiting for the stream to free up…'
        : 'That did not take - trying again…');
      await new Promise((r) => setTimeout(r, 1500));
    }
  }
}
</script>`;
}
