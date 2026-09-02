# Sideband relay

A signalling relay for pairing over the internet. It holds an SDP offer and an
SDP answer for five minutes so two machines that cannot reach each other can
swap them, then gets out of the way. **It never carries video or audio**, once
the peers have exchanged SDP they talk directly.

You only need this to replace Tailscale. On a LAN or a tailnet, `sideband
stream` serves its own viewer page and no relay is involved at all.

## Deploy

```bash
cd worker
npx wrangler kv namespace create SESSIONS
```

Paste the id it prints into `wrangler.toml`, then:

```bash
npx wrangler deploy
```

Run it locally instead with `npx wrangler dev --port 8787`, any string works
as the namespace id in local mode.

## Use

Set the relay once and the code becomes the default way in:

```bash
export SIDEBAND_RELAY=https://sideband.<your-subdomain>.workers.dev
sideband
```

That lists your windows, you pick one, and it prints a fresh six-character code
and a link each time. Whoever you send it to opens the link and clicks Watch.
The code is generated per session and expires after five minutes, so sharing
again means a new one.

You can also be explicit:

```bash
sideband share <pid> https://sideband.<your-subdomain>.workers.dev
```

## What it costs

Signalling is effectively free because the Worker is out of the path once the
call is up. Per session:

| Resource | Per session | Free tier | Headroom |
|---|---|---|---|
| Worker requests | ~6 | 100,000/day | ~16,000 sessions/day |
| KV writes | 2 | 1,000/day | 500 sessions/day, the real ceiling |

The only line worth watching is TURN relay bandwidth, which applies when
hole punching fails (usually CGNAT). That is a separate Cloudflare Realtime
product with its own 1,000 GB/month allowance, and at ~4.5 GB/hour it covers
roughly 220 hours of relayed streaming, in the worst case where every session
relays. Most home-to-home connections go direct and use none of it.

## Protocol

Six routes. The relay holds two blobs of text and gets out of the way.

| Route | Who | Auth |
|---|---|---|
| `POST /api/session` | host |, |
| `PUT /api/session/:code/offer` | host | token |
| `GET /api/session/:code/offer` | viewer | code only |
| `POST /api/session/:code/answer` | viewer | code only |
| `GET /api/session/:code/answer` | host | token |
| `DELETE /api/session/:code` | host | token |

`GET /` serves the viewer page; `GET /:code` serves it with the code filled in.

Both ends gather ICE fully before publishing (non-trickle), which keeps the
exchange to plain request/response, no WebSocket, no long-lived connection.

## Security

The code is the only secret a viewer needs, because it is the only thing that
can be read aloud. Everything else is built around that constraint.

**The relay issues the code**, not the host. A host that picked its own could
squat one, and per-client limits would have nothing to attach to.

**A session is claimed exactly once.** The first valid answer wins; the offer
then returns `410` and further answers return `409`. A code that has been used
is worthless to anyone who later learns it.

**Only the host can read the answer.** An answer carries the viewer's ICE
candidates, which include their public IP. Leaving that readable to anyone
holding the code would hand out the address of the person watching, something
they never agreed to. The host proves itself with a 256-bit token that is never
displayed, spoken, or put in a URL; the relay stores only its SHA-256.

**The host deletes the session on connect**, so nothing lingers for the full
TTL. Whatever happens, an alarm tears it down after five minutes.

**State lives in Durable Objects, not KV.** Claiming is a read-then-write, and
KV cannot do that atomically: two viewers racing would both see "unclaimed",
and an attacker could overwrite the real answer. A Durable Object serialises
them, which makes single-claim a property of the system rather than a hope
about timing.

**Rate limited per IP** - 20 session creations and 30 code lookups a minute.
Without this the 404-vs-200 difference on a lookup is a clean oracle: an
attacker cannot guess a specific code, but they can sweep for any live one, and
unmetered sweeping is what makes that practical.

Codes are drawn with rejection sampling from a 31-character alphabet, so every
character is equally likely and there is no modulo bias.

What remains, and cannot be designed away: **the relay operator is trusted**.
Whoever runs it could substitute SDP and place themselves in the middle. That
is true of every signalling server, and it is exactly what running your own
answers.
