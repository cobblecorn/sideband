#!/usr/bin/env bash
# Deploys the Sideband relay to your own Cloudflare account.
#
# Everything here is scriptable except signing in, which deliberately is not:
# it opens a browser and you approve it yourself. Run this from worker/.
set -u

cd "$(dirname "$0")" || exit 1

say() { printf '\n  %s\n' "$*"; }
die() { printf '\n  %s\n\n' "$*" >&2; exit 1; }

# ---------------------------------------------------------------------------
# 1. Signed in?
# ---------------------------------------------------------------------------
say "Checking your Cloudflare login…"
if ! npx --yes wrangler whoami >/dev/null 2>&1; then
  die "Not signed in. Run this first, approve it in the browser, then re-run me:

    npx wrangler login"
fi

account=$(npx --yes wrangler whoami 2>/dev/null | grep -oE '[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+' | head -1)
say "Signed in${account:+ as $account}."

# ---------------------------------------------------------------------------
# 2. KV namespace, the relay's only storage. Two writes per session.
# ---------------------------------------------------------------------------
current_id=$(grep -oE '^id = "([^"]*)"' wrangler.toml | sed 's/id = "//; s/"//')

if [ "$current_id" = "PASTE_NAMESPACE_ID_HERE" ] || [ -z "$current_id" ]; then
  say "Creating the SESSIONS namespace…"
  output=$(npx --yes wrangler kv namespace create SESSIONS 2>&1)

  # Wrangler has moved this line around between versions, so take the first
  # thing that looks like a namespace id rather than trusting the layout.
  new_id=$(printf '%s' "$output" | grep -oE '"?id"?[[:space:]]*[:=][[:space:]]*"[0-9a-f]{32}"' \
           | grep -oE '[0-9a-f]{32}' | head -1)
  if [ -z "$new_id" ]; then
    new_id=$(printf '%s' "$output" | grep -oE '[0-9a-f]{32}' | head -1)
  fi

  if [ -z "$new_id" ]; then
    printf '%s\n' "$output" >&2
    die "Could not find a namespace id in that output. Copy the id from above
  into wrangler.toml by hand, then re-run me."
  fi

  # Portable in-place edit: -i differs between GNU and BSD sed.
  sed "s/^id = \".*\"/id = \"$new_id\"/" wrangler.toml > wrangler.toml.new \
    && mv wrangler.toml.new wrangler.toml
  say "Namespace created and written to wrangler.toml."
else
  say "Namespace already configured ($current_id), leaving it alone."
fi

# ---------------------------------------------------------------------------
# 3. Deploy
# ---------------------------------------------------------------------------
say "Deploying…"
deploy=$(npx --yes wrangler deploy 2>&1) || { printf '%s\n' "$deploy" >&2; die "Deploy failed."; }
printf '%s\n' "$deploy" | tail -6

url=$(printf '%s' "$deploy" | grep -oE 'https://[A-Za-z0-9.-]+\.workers\.dev' | head -1)
[ -z "$url" ] && die "Deployed, but I could not read the URL from the output.
  Take it from the lines above."

# ---------------------------------------------------------------------------
# 4. Prove it actually works, rather than assuming
# ---------------------------------------------------------------------------
say "Testing the relay…"
code="ZZ9TST"
put=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$url/s/$code" -d 'v=0 setup-probe')
got=$(curl -s "$url/s/$code")
missing=$(curl -s -o /dev/null -w '%{http_code}' "$url/s/$code/a")

if [ "$put" = "200" ] && [ "$got" = "v=0 setup-probe" ] && [ "$missing" = "404" ]; then
  say "Relay is live and answering correctly."
else
  die "Relay deployed but did not behave as expected
  (publish=$put, fetch='$got', unanswered=$missing)."
fi

cat <<EOF

  Done. Point Sideband at it:

    export SIDEBAND_RELAY=$url

  Or paste that URL into the Relay box in the app. Either way you will get a
  fresh code and a link each time you press start.

EOF
