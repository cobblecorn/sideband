# Gives Sideband somewhere to live.
#
# A cargo build leaves the executable in `target\release`, which is a build
# directory: nothing in the Start menu points at it, `cargo clean` deletes it,
# and anything pinned to the taskbar breaks when it does. This copies it
# somewhere permanent and makes the shortcuts, so the program can be found the
# way every other program is.
#
# Per-user, under %LOCALAPPDATA%. Nothing here needs administrator rights and
# nothing is written outside your own profile.
#
#   powershell -ExecutionPolicy Bypass -File install.ps1
#   powershell -ExecutionPolicy Bypass -File install.ps1 -Uninstall

param([switch]$Uninstall)

$ErrorActionPreference = "Stop"

$home_dir  = Join-Path $env:LOCALAPPDATA "Programs\Sideband"
$installed = Join-Path $home_dir "sideband.exe"
$start     = Join-Path $env:APPDATA "Microsoft\Windows\Start Menu\Programs\Sideband.lnk"
$desktop   = Join-Path ([Environment]::GetFolderPath("Desktop")) "Sideband.lnk"

if ($Uninstall) {
    foreach ($p in @($start, $desktop)) {
        if (Test-Path $p) { Remove-Item $p -Force; "removed $p" }
    }
    if (Test-Path $home_dir) { Remove-Item $home_dir -Recurse -Force; "removed $home_dir" }

    # Leaving a rule behind for a program that no longer exists is untidy at
    # best. Best effort: it needs administrator rights, and not having them is
    # not a reason to fail an uninstall that otherwise worked.
    $stale = Get-NetFirewallApplicationFilter -ErrorAction SilentlyContinue |
        Where-Object { $_.Program -eq $installed }
    if ($stale) {
        try {
            $stale | Get-NetFirewallRule | Remove-NetFirewallRule -ErrorAction Stop
            "removed the firewall rule"
        } catch {
            "left the firewall rule in place (needs an administrator shell)"
        }
    }
    ""
    "Your relay is still remembered in $env:APPDATA\Sideband\settings."
    "Delete that too if you want nothing left behind."
    return
}

$built = Join-Path $PSScriptRoot "target\release\sideband.exe"
if (-not (Test-Path $built)) {
    throw "No release build found. Run 'cargo build --release' first, then this again."
}

# Copying over a running program fails with a locked-file error that does not
# explain itself, so say what is actually wrong.
if (Get-Process -Name sideband -ErrorAction SilentlyContinue) {
    throw "Sideband is running. Close it and run this again."
}

New-Item -ItemType Directory -Force -Path $home_dir | Out-Null
Copy-Item $built $installed -Force

# The shortcuts carry no icon of their own: the executable has the mark built
# into it, so the shell reads it straight from there and the two can never
# disagree.
$shell = New-Object -ComObject WScript.Shell
foreach ($path in @($start, $desktop)) {
    $link = $shell.CreateShortcut($path)
    $link.TargetPath       = $installed
    $link.WorkingDirectory = $home_dir
    $link.Description      = "Stream one application, and only that application's audio"
    $link.Save()
}

# Windows Firewall, and the reason this section exists at all.
#
# The video itself arrives at this machine as an inbound connection, whichever
# way the two ends were introduced, by code or by local link. Windows drops
# inbound traffic for any program it has no rule for, and it does so silently:
# the code is issued, the viewer opens the link, and then nothing, with this
# end waiting for a viewer who cannot reach it.
#
# It depends on the viewer. Somebody far away usually gets through without a
# rule, because this end knows their real public address and contacts them
# first, and Windows lets the reply in. A laptop or phone on the same network
# is different: browsers hide their local address behind a random .local name,
# so this end cannot contact them first, their attempt arrives unannounced,
# and it is dropped. Measured: a laptop on the same network could not connect
# to the installed copy, and connected at once to an identical copy that had a
# rule.
#
# Rules are per executable path, which is the trap. A rule created while
# running from `target\release` says nothing about the copy installed here, so
# moving from one to the other loses it without any sign that anything changed.
#
# Private networks only. That is your home network; a cafe's Wi-Fi is Public,
# and a screen sharing program has no business accepting connections there
# unless you decide otherwise.
$rule_name = "Sideband"
$has_rule = {
    [bool](Get-NetFirewallApplicationFilter -ErrorAction SilentlyContinue |
        Where-Object { $_.Program -eq $installed })
}

$admin = ([Security.Principal.WindowsPrincipal] `
    [Security.Principal.WindowsIdentity]::GetCurrent()
).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)

$add_rule = "New-NetFirewallRule -DisplayName '$rule_name' -Direction Inbound " +
    "-Action Allow -Program '$installed' -Profile Private " +
    "-Description 'Lets viewers on your home network reach Sideband' | Out-Null"

if (& $has_rule) {
    $firewall = "already allowed on your home network"
} elseif ($admin) {
    try {
        Invoke-Expression $add_rule
        $firewall = "allowed on your home network"
    } catch {
        $firewall = "NOT allowed: could not add the rule ($_)"
    }
} else {
    # Everything else in this installer runs as you. This one step cannot:
    # changing the firewall needs administrator rights, so Windows asks once,
    # for this and nothing else. Declining leaves a working install that only
    # viewers far away can reach.
    ""
    "  One Windows prompt is about to appear, asking to let Sideband through"
    "  the firewall on your home network. Without it, laptops and phones on"
    "  the same network as this machine cannot connect."
    ""
    # Sent encoded, not as text. A command handed to a second PowerShell as
    # plain arguments is split on spaces and has its quoting reinterpreted,
    # and a rule whose program path was mangled on the way fails in a window
    # nobody sees. Any error is written somewhere this installer can read it
    # back, so a failure is reported as what it was rather than guessed at.
    $log = Join-Path $env:TEMP "sideband-firewall.txt"
    Remove-Item $log -ErrorAction SilentlyContinue
    $elevated = "try { $add_rule } catch { `$_ | Out-File -Encoding utf8 '$log' }"
    $encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($elevated))
    try {
        Start-Process powershell -Verb RunAs -Wait -WindowStyle Hidden `
            -ArgumentList "-NoProfile", "-EncodedCommand", $encoded
    } catch {
        # Declining the prompt lands here.
    }
    if (& $has_rule) {
        $firewall = "allowed on your home network"
    } elseif (Test-Path $log) {
        $firewall = "NOT allowed: " + (Get-Content $log -Raw).Trim()
    } else {
        $firewall = "NOT allowed: the Windows prompt was declined or did not appear"
    }
}

""
"Installed:  $installed"
"Start menu: search for 'Sideband'"
"Desktop:    Sideband"
"Firewall:   $firewall"
""

if ($firewall -like "NOT*") {
    "  Viewers far away will still connect. Laptops and phones on the same"
    "  network as this machine will not, until you run this installer again"
    "  and say yes to the Windows prompt."
    ""
}

"Run this again after any rebuild to update the installed copy."
