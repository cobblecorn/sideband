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

""
"Installed:  $installed"
"Start menu: search for 'Sideband'"
"Desktop:    Sideband"
""
"Run this again after any rebuild to update the installed copy."
