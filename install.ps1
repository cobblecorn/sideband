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
# Sharing on your own network means a browser on another device opening a page
# this program serves. That is an inbound connection, and Windows blocks
# inbound connections to programs it has no rule for. It does so silently: the
# server starts, the link is generated and looks perfectly normal, and the
# other device simply cannot reach it. What you see is "waiting for a viewer"
# for ever.
#
# Rules are per executable path, which is the trap. A rule created while
# running from `target\release` says nothing about the copy installed here, so
# moving from one to the other loses it without any sign that anything changed.
$rule_name = "Sideband"
$existing = Get-NetFirewallApplicationFilter -ErrorAction SilentlyContinue |
    Where-Object { $_.Program -eq $installed }

if ($existing) {
    $firewall = "already allowed through the firewall"
} else {
    $admin = ([Security.Principal.WindowsPrincipal] `
        [Security.Principal.WindowsIdentity]::GetCurrent()
    ).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)

    if ($admin) {
        try {
            New-NetFirewallRule -DisplayName $rule_name -Direction Inbound `
                -Action Allow -Program $installed -Profile Private `
                -Description "Sharing to a browser on your own network" | Out-Null
            $firewall = "allowed through the firewall"
        } catch {
            $firewall = "could not add a firewall rule: $_"
        }
    } else {
        # Not fatal, and not something to elevate for on its own: sharing
        # through a relay needs no inbound rule at all, so an install without
        # this is still a working install for anyone not on your own network.
        $firewall = "NOT allowed through the firewall yet, see below"
    }
}

""
"Installed:  $installed"
"Start menu: search for 'Sideband'"
"Desktop:    Sideband"
"Firewall:   $firewall"
""

if ($firewall -like "NOT*") {
    "  Sharing on your own network needs one firewall rule, and adding it"
    "  needs administrator rights, which this installer does not ask for."
    ""
    "  Either run this installer again from an administrator PowerShell, or"
    "  run this one line in one:"
    ""
    "    New-NetFirewallRule -DisplayName 'Sideband' -Direction Inbound ``"
    "      -Action Allow -Program '$installed' -Profile Private"
    ""
    "  Without it, a browser on another device on your network cannot reach"
    "  this machine and the window will wait for a viewer that never arrives."
    "  Sharing by code through a relay is unaffected."
    ""
}

"Run this again after any rebuild to update the installed copy."
