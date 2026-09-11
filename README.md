<p align="center">
  <img src="docs/mark.svg" alt="Sideband" width="104" height="104">
</p>

<h1 align="center">Sideband</h1>

<p align="center">
  Stream one application to one person, with <strong>only that application's audio</strong>.<br>
  No call, no account, nothing installed on their end.
</p>

<p align="center">
  <img src="docs/strip.png" alt="The Sideband window: a thin strip with an application picker, session state, and the link to send" width="880">
</p>

---

You pick a window. They open a link and click Watch. They hear that window and
nothing else, not your other tabs, not your voice chat, not your notifications.

Link-based browser screen sharing already exists, and so does proper per-process
capture. What doesn't exist is both at once. Tools that share to a browser capture
through `getDisplayMedia`, which can give a tab's audio or the whole system's,
but never one process's. Tools that capture properly need an install and an account
on the viewing end. Sideband sits in both columns.

## Requirements

- Windows 10 version 2004 or newer, which is where the process-loopback audio API arrived
- An NVIDIA GPU, because the encoding is NVENC
- The viewer needs a browser. That's all.

## Install

```powershell
cargo build --release
powershell -ExecutionPolicy Bypass -File install.ps1
```

That copies the executable to `%LOCALAPPDATA%\Programs\Sideband` and makes a Start
menu and desktop shortcut. It has to be copied somewhere rather than run from
`target/release`, because that is a build directory. `cargo clean` empties it, and
anything pinned to the taskbar breaks when it does.

Run the script again after a rebuild to update the installed copy, or with
`-Uninstall` to take it back off. No administrator rights, nothing written outside
your own profile.

**The installer asks Windows for one permission, once.** The video arrives at
this machine as an inbound connection, and Windows silently drops inbound
traffic for programs it has no rule for. Viewers far away usually get through
anyway, because this end knows their public address and contacts them first.
Laptops and phones on the same network do not: browsers hide their local
address, so their attempt arrives unannounced and is dropped, and the window
waits for a viewer that never arrives. Measured, a laptop on the same network
could not connect to the installed copy and connected at once to an identical
copy that had the rule.

So the installer adds it, for your home network only, behind a single Windows
prompt. Everything else it does runs as you. Rules are per executable path, so
one made while running from `target/release` says nothing about the installed
copy, which is why installing can appear to break something that worked.
Nothing is ever needed on the viewer's side.

**A minimised window cannot be captured.** Windows.Graphics.Capture delivers
nothing at all while a window is minimised, so the viewer gets sound and no
picture. Sideband says so rather than leaving you guessing, and starts sending the
moment the window is restored.

## Use it

Open Sideband, pick an application, press start. You get a link, and a six-character
code if you're going through a relay. Send either one.

**The link is permanent.** Going through a relay, the link looks like
`https://your-relay/r/k6qcy68uka45` and it is the same every time you share. It is
on screen before you press start, so it can be sent ahead of time, and a page
left open on it starts the stream by itself whenever you go live, and goes back
to waiting when you stop. Bookmark it once and nobody has to send anybody a new
link again. The code still works on its own, for reading out over a call.

**qr** shows the link as a QR code, for a phone to open with its camera.

**viewers** lists who is watching, by device and address, with how long they have
been there. From there:

- **remove** disconnects somebody and keeps them out until you stop sharing.
- **lock** stops anyone new from joining. Everybody watching stays, and anyone
  opening the link is told you are not letting people in right now, and gets in
  by themselves when you unlock.
- **new link** replaces the link and the code, and the old ones stop working at
  once. For a link that has travelled further than you meant it to. Everybody
  already watching stays; remove is for them.
- **sounds** turns off the chime played when somebody joins or leaves.

**Hide** - **Ctrl+Alt+H**, or the **hide** button, covers the picture with a
pause card, for the moment a login box or a private message is about to be on
screen. Sound carries on. Press it again to show the picture. The viewer never
reconnects and the stream never stops: the card is encoded like any other frame,
so there is nothing to recover from when the picture comes back. A short sound
tells you which way it went, because you will be looking at a game rather than at
the window.

**On a phone** - the page keeps the screen awake while the stream is on it, and if
the phone refuses to start the video with sound, the picture starts anyway with a
**Tap for sound** button over it.

There is a command line too, driving the same engine:

```
sideband                      pick a window and share it
sideband share  [pid] [relay] pair by code through a relay
sideband stream [pid] [port]  serve the viewer page yourself
sideband router               check whether the router will let phones in
```

While sharing from a terminal, **h** hides the picture, **l** locks, **v** lists
who is watching, **x 2** removes viewer 2, **n** makes a new link, **q** stops, and
a number switches application. Enter on its own lists all of it.

**On a LAN or a tailnet you need no relay at all.** `sideband stream` serves its
own viewer page and nothing external is involved.

**Switching applications** - pick a different one at any point and the stream
follows it, picture and audio both. Nothing is torn down: same connection, same
code, same viewer, who simply starts seeing something else. In a terminal, press
Enter for the list and type a number.

**Auto admit** - off by default. With it on, whoever opens the link is let
straight in and nothing is asked on your end, which is what you want if you are
not sitting at the keyboard when they arrive. It also makes the code the only
thing between them and your screen, so it is a per-machine choice and it is
remembered.

**Microphone** - off by default. **Ctrl+Alt+M** toggles it globally, so it works
without leaving the game, with a short high or low tone for on and off. Pick which one with **mic device**; the level meter runs
whether or not the mic is live, so you can confirm you chose the right one without
going live first.

A meter reading 0% is not on its own a fault. A noise-gated microphone, which is
what the vendor mixer suites set up by default, reads exactly zero until you
speak and then jumps, and that is it working correctly. What the meter is for is
the difference between that and a device that stays at zero while you are
talking, which is the one you cannot hear by listening to yourself. `sideband
mics` lists the capture devices and `sideband mic` shows the level of one.

**Volume** - the captured application is amplified to something audible before it
is sent, and held there as you switch between applications. This is not optional
and there is no slider, because the level that reaches the viewer has almost
nothing to do with the level you hear: your master volume, the per-application
slider in the mixer and your headset's own amplifier are all downstream of what
gets captured, and none of them are in it. Measured here, a game sat 11 dB below
a chat application while both sounded normal in the room. Sent as captured, that
is a viewer at full volume still straining to hear. Your microphone is mixed in
afterwards at its own level, so a loud game never ducks your voice.

**Quality** - there is no quality setting, because the right one is a property of
the viewer's connection and neither of you knows it. A session opens at 2.5 Mbit/s
and 30 fps, low enough for almost any home link to carry from the first frame, and
climbs to 1080p60 at 10 Mbit/s over the next ten to twenty seconds if the viewer
keeps reporting a clean path. If they stop, it comes back down quickly.

Bitrate and frame rate move continuously. **The picture size never changes on
its own**: it is always the size of the window being shared. It used to shrink on
a slow connection, and a picture that resizes itself in the viewer's window turned
out to be far worse to watch than one that is a little soft.

**To send a smaller picture** anyway, on a connection that really cannot carry
full size, set `SIDEBAND_SCALE` to 2 or 4, for half or quarter. The viewer's
browser scales it back up to fill their window.

**If you want to cap what it uses**, set `SIDEBAND_MAX_BITRATE` to a number of
kbit/s. Everything above still applies underneath it; this only lowers the
ceiling. It is the one quality decision the software cannot make for you, because
"how much of my upload may this take" is a question about the rest of your house
rather than about the connection.

**"They cannot hear it"** - watch the `app` level while the stream runs. Process
loopback delivers evenly paced packets whether or not the application is making
a sound, and it accepts any process at all without complaint, so a packet count
alone never proves anything is being heard. The level does. If it sits at zero
while the application is plainly audible to you, the audio is being taken from
the wrong process; if it moves, the audio is leaving this machine and the
problem is at the other end. Note that many games mute themselves when they are
not the focused window, which reads as silence here and is the application doing
it, not Sideband.

When a stream misbehaves and you want to know why, `SIDEBAND_DEBUG_RATE=1` prints a
line a second with what the viewer actually reported and what was decided from it.

---

# Setting up a relay

**You only need this to share with someone outside your network.** On a LAN or a
tailnet, skip the whole section.

The relay is a Cloudflare Worker that holds an SDP offer and an SDP answer for five
minutes so two machines that cannot reach each other directly can swap them, then
gets out of the way. **It never carries video or audio.** Once the two ends have
exchanged those blobs they talk to each other directly, and the relay is done.

That is why it costs essentially nothing to run: about six requests per session, on
a free plan that allows 100,000 a day.

### One command

```bash
cd worker
./setup.sh
```

The script checks you are logged in, creates the Durable Objects the relay needs,
deploys it, and then probes the live URL to prove it works. It prints the address at
the end.

### Or by hand

```bash
cd worker
npx wrangler login
npx wrangler deploy
```

There is nothing to configure. State lives in Durable Objects, which need no
namespace ids in `wrangler.toml`, so the file in this repo works as it stands.

To run one locally instead while you poke at it:

```bash
npx wrangler dev --port 8787
```

### Point Sideband at it

Paste the URL into the relay box in the window. It is remembered, so this is
something you do once rather than once a session. It lives in
`%APPDATA%\Sideband\settings`, a plain text file you can open, edit or delete.

For the command line, either pass it explicitly or set it in the environment:

```bash
export SIDEBAND_RELAY=https://sideband.<your-subdomain>.workers.dev
```

A remembered relay beats `SIDEBAND_RELAY`: the variable seeds a machine that has
never been told one, but a box that ignored what you typed into it would be worse
than a box that never remembered anything.

### What it costs

| Resource | Per session | Free tier | Headroom |
|---|---|---|---|
| Worker requests | ~6 | 100,000/day | ~16,000 sessions/day |
| Durable Object writes | a handful | generous | not the limit |

The only line worth watching is TURN relay bandwidth, which applies when hole
punching fails, which usually means carrier-grade NAT. That is a separate Cloudflare Realtime
product with its own 1,000 GB/month allowance, and at roughly 4.5 GB/hour it covers
about 220 hours of relayed streaming in the worst case where every session relays.
Most home-to-home connections go direct and use none of it.

### Viewers your connection cannot reach directly

Most connections find each other on their own. Two that cannot need a relay of
last resort, and there is exactly one situation where this is not optional: a
viewer on mobile data. Carriers put every subscriber behind one shared address,
so there is nothing on the far side to punch a hole in, and no amount of
retrying will help. The same applies to a network that keeps its own devices
apart, which some routers do by default for anything on wireless.

**Set it on the relay, not on the machine sharing.** The relay is the piece of
the setup that is already shared, so configuring it there means every machine
pointed at it is configured too: a new laptop, a reinstall, somebody else
running it. Sending a link stays the whole of what anybody has to do, and
nothing has to be adjusted on the viewer's side ever.

Create a TURN key in the Cloudflare dashboard, under Realtime, then:

```bash
cd worker
npx wrangler secret put TURN_KEY_ID
npx wrangler secret put TURN_TOKEN
npx wrangler deploy
```

The host asks the relay for its route options at the start of every session and
says so in the read-out when there are none. Credentials are minted per session
and last a day, so nothing long lived is handed out or stored on any machine
doing the sharing.

Only connections that cannot go direct use it, and it carries the video rather
than just introducing the two ends, so it is the one part of this with a real
bandwidth cost: roughly 4.5 GB an hour against Cloudflare's 1,000 GB a month.

TURN can also be set per machine through the environment, which the relay
overrides when it has something to say:

```bash
export SIDEBAND_CF_TURN_KEY_ID=...      # Cloudflare Realtime
export SIDEBAND_CF_TURN_TOKEN=...
# or any provider
export SIDEBAND_TURN_URL=turn:...
export SIDEBAND_TURN_USER=...
export SIDEBAND_TURN_PASS=...
```

Without it, connections fall back to STUN only. Most work; the ones behind CGNAT
need the next section.

### Or let the router open the door

Most home routers let a program ask them to forward a port, through UPnP, and
Sideband asks. For each viewer it forwards the one port that viewer's connection
is listening on, for as long as they are watching, adds your router's public
address to the offer as one more place to connect, and closes the port again when
they leave. A phone on mobile data then has somewhere to connect to, which is the
whole of what it was missing. Nothing is configured by hand and nothing is paid
for, and when it works it makes TURN unnecessary.

What is behind the port is a WebRTC connection, which answers nothing that does
not carry the credentials from that viewer's offer.

`sideband router` checks whether yours will do it, without opening anything.
**viewers** in the window says the same while you share. It cannot help when your
internet provider puts your whole router behind a shared address, which some do;
that is detected and said. Turn it off with `open_ports = false` in
`%APPDATA%\Sideband\settings`, or `SIDEBAND_NO_UPNP=1` for one run.

See [worker/README.md](worker/README.md) for the protocol and the threat model.

---

## Who can watch

Whoever has the link or the code, while you are sharing. Both keep working for
everybody for as long as the session runs, so a second person, or the same person
on a second device, is let in the same way as the first.

Knowing either is not enough on its own unless you say so. The host is shown the
device and the address each request came from and has to allow it before any
video flows; turning somebody away keeps them out for the rest of the session.
**Auto admit** skips the prompt, for when you are the one at the other end.

After that it is yours to manage: **lock** to let nobody else in, **remove** for
somebody who should not be there, and **new link** when the link itself has got
out. The permanent link is twelve random characters, which is the part nobody
can guess, and the key that updates it never leaves your machine and the relay.

The link on a local network carries a 128-bit secret too. Being on the same Wi-Fi is
not consent to watch someone's screen, so every route checks it and anything without
it gets an identical `404`.

What cannot be designed away: **the relay operator is trusted**. Whoever runs it
could substitute SDP and put themselves in the middle. That is true of every
signalling server, and it is exactly what running your own answers.

## How it works

| Stage | What it uses |
|---|---|
| Source | `EnumWindows`, one entry per process, swappable mid-stream |
| Video | Windows.Graphics.Capture → D3D11 texture, follows window resizes |
| Audio | WASAPI process loopback, scoped to the target's process tree |
| Encode | NVENC, fed the texture directly with no CPU readback or colour conversion |
| Transport | WebRTC, pre-encoded H.264 + Opus |
| Rate control | The viewer's REMB and receiver reports, read off the RTCP stream |
| Recovery | NACK for what can be retransmitted, rolling intra refresh for the rest |
| Scaling | Mipmap generation on the GPU, powers of two, no readback |
| Pairing | Non-trickle SDP through a Worker, or served locally |

Three invariants worth knowing before changing anything:

**Silence and stillness are not "no data".** Process loopback delivers no packets
while an app is quiet, and window capture delivers no frames while a window is still.
Both are filled against a wall clock with synthesised silence and repeated frames,
because an encoder fed irregularly produces a stream whose tracks drift apart within
a minute.

**The GPU texture must stay on the GPU.** Window capture hands out BGRA textures and
NVENC's `ARGB` input format is the same byte order, so capture-to-encode is genuinely
zero-copy. Any CPU readback added between them costs more latency than the entire
network hop.

**`get_stats` cannot see incoming RTCP.** `rtc` 0.20.4 defines
`process_read_rtcp_for_stats` and never calls it, and the innermost interceptor in
the chain drops RTCP rather than forwarding it, so the library reports zero loss,
zero NACKs and zero picture-loss requests however badly a connection is doing. Rate
control therefore reads the packets itself, from inside the interceptor chain. Do not
"simplify" it back onto `get_stats`: it will compile, run, and quietly believe every
connection is perfect.

## The mark

A sideband is the band of frequencies beside a carrier wave, and single-sideband
radio transmits one of them and suppresses the rest. It is a narrow, point-to-point
signal with everything else left out. That is what this does with a desktop, so that
is what the icon draws: a tall carrier, two bands falling away to the right, and the
pair on the left faded almost to nothing.

It is geometry, not a file. `src/mark.rs` holds the bars; `build.rs` includes that
same source to rasterise the `.ico` at every size Windows asks for, and the window
icon and the strip's own lockup are drawn from it at runtime. Below 48 pixels it
switches to a heavier three-bar cut, because the outer pair would be under two pixels
wide and would fringe rather than read.

## Test modes

Each isolates one stage, which is how most of the bugs in this were found:

```
sideband router               whether the router will open ports, opening none
sideband mics                 list the capture devices it can use
sideband mic  [secs] [n]      listen to one and watch the level
sideband audio  <pid> <secs>  that app's audio to a WAV, plus Opus stats
sideband window <pid> <secs>  one captured frame to a BMP
sideband encode <pid> <secs>  capture → pace → NVENC → out.h264
```

`audio` deliberately records what the application produced, with no amplification,
because the point of it is to show what actually came out of the capture.

`encode` also retunes the encoder halfway through and reports each half separately,
because a driver that accepts a rate change and ignores it would leave adaptive
quality doing nothing and nothing else would say so.

```bash
cargo test
```

## Known limits

- **TURN has never fired.** Every connection so far has gone direct. The fallback is
  configured but unexercised, so carrier-grade NAT is still an open question.
- **No periodic keyframes.** Forcing an IDR mid-stream against `rtc-rtp` 0.20.4
  breaks decoding outright. That is measured, not assumed. Loss recovery relies on
  NACK retransmission instead.
- **Several people can watch the same link.** Each viewer gets its own encoder
  session, which is why: a viewer arriving late has no reference frame, the only
  thing that gives them one is a keyframe, and a forced keyframe does not survive
  this pipeline. Measured with two watching, sending one for the newcomer took
  *both* of them to zero frames a second. A fresh encoder opens with a keyframe of
  its own, which is the one case known to work, so everybody is a first viewer.
  The cost is an encode pass each; the gain is that each viewer also gets a rate
  fitted to their own connection rather than the worst one in the room.
- **No keyframes after the first one.** Forcing one mid-stream does not survive
  this pipeline, so recovery is rolling intra refresh instead: a band of the
  picture is re-encoded from scratch every couple of seconds, and a decoder in
  any state converges within one cycle. Measured with 15% of frames discarded on
  purpose, sustained: every surviving frame decoded, no freezes, and no picture
  requests. The visible cost is a faint band sweeping the picture once after
  heavy loss.
- **Resolution is the window's own, unless you set it.** `SIDEBAND_SCALE` gives
  half or quarter; anything in between would need a scaler with a shader in it
  rather than mipmap generation, for a difference nobody watching would notice.
- **Removing somebody is by browser.** Their page keeps a random name in its
  browser storage, and that is what is kept out. A private window is a new name,
  which is what **new link** is for.
- **Packaged ("Store") applications may have no capturable audio.** Their window
  belongs to `ApplicationFrameHost.exe` while the application runs in a separate
  process that is not below it, so audio scoped to the window's process tree
  gets nothing. Sideband marks those entries in the list. Where the application
  also has a window of its own, pick that one instead. The picture is never
  affected.
- **Windows only, and NVIDIA only.**
