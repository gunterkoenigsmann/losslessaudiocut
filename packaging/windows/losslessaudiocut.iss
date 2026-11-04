; Inno Setup script for the Windows installer of losslessaudiocut.
;
; Compiled by packaging/windows/package.ps1, which passes:
;   /DAppVersion=1.2.0          full version (also used in the file name)
;   /DArch=x64 or /DArch=arm64  the architecture the binaries are built for
;   /DSourceDir=...             folder with the files of the zip release
;   /DOutputDir=...             where the installer goes
;
; Needs Inno Setup 6.3 or newer (the "x64compatible" and "arm64"
; architecture identifiers).

#ifndef AppVersion
  #error AppVersion must be defined (/DAppVersion=...)
#endif
#ifndef Arch
  #error Arch must be defined (/DArch=x64 or /DArch=arm64)
#endif

[Setup]
; Stays the same across versions, so that a new version replaces the old one
; instead of being installed next to it. Never change it.
AppId={{4D6F2A1C-7E3B-4C55-9B8E-1A2F6C0D5E43}
AppName=losslessaudiocut
AppVersion={#AppVersion}
AppVerName=losslessaudiocut {#AppVersion}
AppPublisher=losslessaudiocut contributors
AppPublisherURL=https://github.com/gunterkoenigsmann/losslessaudiocut
AppSupportURL=https://github.com/gunterkoenigsmann/losslessaudiocut/issues
DefaultDirName={autopf}\losslessaudiocut
DefaultGroupName=losslessaudiocut
DisableProgramGroupPage=yes
LicenseFile={#SourceDir}\LICENSE.txt
SetupIconFile={#SourceDir}\losslessaudiocut.ico
UninstallDisplayIcon={app}\losslessaudiocut.ico
OutputDir={#OutputDir}
OutputBaseFilename=losslessaudiocut-{#AppVersion}-windows-{#Arch}-setup
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
; Per-machine by default, but users without admin rights may choose to
; install for themselves only.
PrivilegesRequired=admin
PrivilegesRequiredOverridesAllowed=dialog
; 64-bit only. An x64 build also runs on Windows on Arm (emulated), but the
; arm64 build only there.
#if Arch == "arm64"
ArchitecturesAllowed=arm64
ArchitecturesInstallIn64BitMode=arm64
#else
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
#endif
; Windows 10 or newer.
MinVersion=10.0

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
; Everything of the zip release: both programs, the FFmpeg DLLs, licenses.
Source: "{#SourceDir}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{autoprograms}\losslessaudiocut"; Filename: "{app}\losslessaudiocut.exe"; IconFilename: "{app}\losslessaudiocut.ico"
Name: "{autodesktop}\losslessaudiocut"; Filename: "{app}\losslessaudiocut.exe"; IconFilename: "{app}\losslessaudiocut.ico"; Tasks: desktopicon

[Run]
Filename: "{app}\losslessaudiocut.exe"; Description: "{cm:LaunchProgram,losslessaudiocut}"; Flags: nowait postinstall skipifsilent
