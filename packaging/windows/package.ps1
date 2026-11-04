# Packs the Windows release: a zip with everything needed to run
# losslessaudiocut without installing, and an Inno Setup installer with the
# same files.
#
#   packaging/windows/package.ps1 -Version 1.2.0 -Arch x64|arm64 -FfmpegDir DIR -OutDir DIR
#
# Expects `cargo build --release -p lac-gui -p lac-cli` to have run with
# `-C target-feature=+crt-static` (see .github/workflows/release.yml): the
# programs then contain the C runtime themselves and need no Visual C++
# redistributable. The FFmpeg DLLs (from FfmpegDir\bin) are built with
# MinGW and bring their own runtime too.
#
# Writes OutDir\losslessaudiocut-VERSION-windows-ARCH.zip and
# OutDir\losslessaudiocut-VERSION-windows-ARCH-setup.exe.
param(
    [Parameter(Mandatory)] [string] $Version,
    [Parameter(Mandatory)] [ValidateSet('x64', 'arm64')] [string] $Arch,
    [Parameter(Mandatory)] [string] $FfmpegDir,
    [Parameter(Mandatory)] [string] $OutDir
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$root = Resolve-Path (Join-Path $PSScriptRoot '..\..')
$targetDir = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $root 'target' }
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$OutDir = Resolve-Path $OutDir

# --- Folder of the zip release ---------------------------------------------
$name = "losslessaudiocut-$Version-windows-$Arch"
$temp = if ($env:RUNNER_TEMP) { $env:RUNNER_TEMP } else { [IO.Path]::GetTempPath() }
$stage = Join-Path $temp $name
if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
New-Item -ItemType Directory -Path $stage | Out-Null

Copy-Item (Join-Path $targetDir 'release\losslessaudiocut.exe') $stage
Copy-Item (Join-Path $targetDir 'release\lac.exe') $stage
# All of FFmpeg's DLLs: avformat and avcodec load a few of the others
# (avutil, swresample), and the whole set is small next to avcodec itself.
Copy-Item (Join-Path $FfmpegDir 'bin\*.dll') $stage
Copy-Item (Join-Path $root 'packaging\icons\losslessaudiocut.ico') $stage
# Licenses: ours (GPL-3.0-or-later), and FFmpeg's, whose build is GPL as
# well; the FFmpeg source is linked in THIRD-PARTY.txt.
Copy-Item (Join-Path $root 'LICENSE') (Join-Path $stage 'LICENSE.txt')
$ffmpegLicense = Get-ChildItem $FfmpegDir -Filter 'LICENSE*' | Select-Object -First 1
if ($ffmpegLicense) { Copy-Item $ffmpegLicense.FullName (Join-Path $stage 'FFMPEG-LICENSE.txt') }
@"
losslessaudiocut $Version for Windows ($Arch)

losslessaudiocut.exe   the program
lac.exe                the command line tool (run "lac --help")

Third-party software in this folder:

- FFmpeg (av*.dll, sw*.dll): https://ffmpeg.org, license in
  FFMPEG-LICENSE.txt. Build: $(Split-Path -Leaf $FfmpegDir) from
  https://github.com/BtbN/FFmpeg-Builds, which also links the source code.
- wxWidgets (built into losslessaudiocut.exe): https://wxwidgets.org,
  wxWindows Library Licence.

Source code of losslessaudiocut:
https://github.com/gunterkoenigsmann/losslessaudiocut
"@ | Set-Content -Encoding utf8 (Join-Path $stage 'THIRD-PARTY.txt')

# --- Zip ------------------------------------------------------------------------
# The folder itself goes into the zip, so unpacking gives one folder.
$zip = Join-Path $OutDir "$name.zip"
if (Test-Path $zip) { Remove-Item $zip }
Compress-Archive -Path $stage -DestinationPath $zip
Get-Item $zip | Format-Table Name, Length

# --- Installer ------------------------------------------------------------------
$iscc = @(
    "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
    "$env:ProgramFiles\Inno Setup 6\ISCC.exe",
    "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe"
) | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $iscc) { throw 'Inno Setup 6 (ISCC.exe) not found' }
& $iscc `
    "/DAppVersion=$Version" "/DArch=$Arch" "/DSourceDir=$stage" "/DOutputDir=$OutDir" `
    (Join-Path $root 'packaging\windows\losslessaudiocut.iss')
if ($LASTEXITCODE -ne 0) { throw "ISCC failed with exit code $LASTEXITCODE" }
Get-Item (Join-Path $OutDir "$name-setup.exe") | Format-Table Name, Length
