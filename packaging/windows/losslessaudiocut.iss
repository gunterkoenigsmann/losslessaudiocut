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
; Tell Explorer that the "Open with" entries below changed.
ChangesAssociations=yes

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked
Name: "openwith"; Description: "Offer losslessaudiocut in ""Open with"" for audio files (MP3, Ogg, Opus, Speex, FLAC, WAV, MKA)"; GroupDescription: "File associations:"

[Files]
; Everything of the zip release: both programs, the FFmpeg DLLs, licenses.
Source: "{#SourceDir}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs

[Registry]
; "Open with" entries for audio files. They never make losslessaudiocut the
; default program: OpenWithProgids only adds it to the list. HKA is
; HKLM for an all-users installation and HKCU for a per-user one.
; The extensions must equal the ones lac_app::file_types associates by
; extension; crates/lac-app/tests/packaging.rs checks it.
Root: HKA; Subkey: "Software\Classes\losslessaudiocut.audio"; ValueType: string; ValueName: ""; ValueData: "Audio recording"; Flags: uninsdeletekey; Tasks: openwith
Root: HKA; Subkey: "Software\Classes\losslessaudiocut.audio\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\losslessaudiocut.ico"; Tasks: openwith
Root: HKA; Subkey: "Software\Classes\losslessaudiocut.audio\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\losslessaudiocut.exe"" ""%1"""; Tasks: openwith
Root: HKA; Subkey: "Software\Classes\Applications\losslessaudiocut.exe"; ValueType: string; ValueName: "FriendlyAppName"; ValueData: "losslessaudiocut"; Flags: uninsdeletekey; Tasks: openwith
Root: HKA; Subkey: "Software\Classes\.mp3\OpenWithProgids"; ValueType: string; ValueName: "losslessaudiocut.audio"; ValueData: ""; Flags: uninsdeletevalue; Tasks: openwith
Root: HKA; Subkey: "Software\Classes\.m4a\OpenWithProgids"; ValueType: string; ValueName: "losslessaudiocut.audio"; ValueData: ""; Flags: uninsdeletevalue; Tasks: openwith
Root: HKA; Subkey: "Software\Classes\.m4b\OpenWithProgids"; ValueType: string; ValueName: "losslessaudiocut.audio"; ValueData: ""; Flags: uninsdeletevalue; Tasks: openwith
Root: HKA; Subkey: "Software\Classes\.ogg\OpenWithProgids"; ValueType: string; ValueName: "losslessaudiocut.audio"; ValueData: ""; Flags: uninsdeletevalue; Tasks: openwith
Root: HKA; Subkey: "Software\Classes\.oga\OpenWithProgids"; ValueType: string; ValueName: "losslessaudiocut.audio"; ValueData: ""; Flags: uninsdeletevalue; Tasks: openwith
Root: HKA; Subkey: "Software\Classes\.opus\OpenWithProgids"; ValueType: string; ValueName: "losslessaudiocut.audio"; ValueData: ""; Flags: uninsdeletevalue; Tasks: openwith
Root: HKA; Subkey: "Software\Classes\.spx\OpenWithProgids"; ValueType: string; ValueName: "losslessaudiocut.audio"; ValueData: ""; Flags: uninsdeletevalue; Tasks: openwith
Root: HKA; Subkey: "Software\Classes\.flac\OpenWithProgids"; ValueType: string; ValueName: "losslessaudiocut.audio"; ValueData: ""; Flags: uninsdeletevalue; Tasks: openwith
Root: HKA; Subkey: "Software\Classes\.wav\OpenWithProgids"; ValueType: string; ValueName: "losslessaudiocut.audio"; ValueData: ""; Flags: uninsdeletevalue; Tasks: openwith
Root: HKA; Subkey: "Software\Classes\.aiff\OpenWithProgids"; ValueType: string; ValueName: "losslessaudiocut.audio"; ValueData: ""; Flags: uninsdeletevalue; Tasks: openwith
Root: HKA; Subkey: "Software\Classes\.aif\OpenWithProgids"; ValueType: string; ValueName: "losslessaudiocut.audio"; ValueData: ""; Flags: uninsdeletevalue; Tasks: openwith
Root: HKA; Subkey: "Software\Classes\.aifc\OpenWithProgids"; ValueType: string; ValueName: "losslessaudiocut.audio"; ValueData: ""; Flags: uninsdeletevalue; Tasks: openwith
Root: HKA; Subkey: "Software\Classes\.mka\OpenWithProgids"; ValueType: string; ValueName: "losslessaudiocut.audio"; ValueData: ""; Flags: uninsdeletevalue; Tasks: openwith
Root: HKA; Subkey: "Software\Classes\Applications\losslessaudiocut.exe\SupportedTypes"; ValueType: string; ValueName: ".mp3"; ValueData: ""; Tasks: openwith
Root: HKA; Subkey: "Software\Classes\Applications\losslessaudiocut.exe\SupportedTypes"; ValueType: string; ValueName: ".m4a"; ValueData: ""; Tasks: openwith
Root: HKA; Subkey: "Software\Classes\Applications\losslessaudiocut.exe\SupportedTypes"; ValueType: string; ValueName: ".m4b"; ValueData: ""; Tasks: openwith
Root: HKA; Subkey: "Software\Classes\Applications\losslessaudiocut.exe\SupportedTypes"; ValueType: string; ValueName: ".ogg"; ValueData: ""; Tasks: openwith
Root: HKA; Subkey: "Software\Classes\Applications\losslessaudiocut.exe\SupportedTypes"; ValueType: string; ValueName: ".oga"; ValueData: ""; Tasks: openwith
Root: HKA; Subkey: "Software\Classes\Applications\losslessaudiocut.exe\SupportedTypes"; ValueType: string; ValueName: ".opus"; ValueData: ""; Tasks: openwith
Root: HKA; Subkey: "Software\Classes\Applications\losslessaudiocut.exe\SupportedTypes"; ValueType: string; ValueName: ".spx"; ValueData: ""; Tasks: openwith
Root: HKA; Subkey: "Software\Classes\Applications\losslessaudiocut.exe\SupportedTypes"; ValueType: string; ValueName: ".flac"; ValueData: ""; Tasks: openwith
Root: HKA; Subkey: "Software\Classes\Applications\losslessaudiocut.exe\SupportedTypes"; ValueType: string; ValueName: ".wav"; ValueData: ""; Tasks: openwith
Root: HKA; Subkey: "Software\Classes\Applications\losslessaudiocut.exe\SupportedTypes"; ValueType: string; ValueName: ".aiff"; ValueData: ""; Tasks: openwith
Root: HKA; Subkey: "Software\Classes\Applications\losslessaudiocut.exe\SupportedTypes"; ValueType: string; ValueName: ".aif"; ValueData: ""; Tasks: openwith
Root: HKA; Subkey: "Software\Classes\Applications\losslessaudiocut.exe\SupportedTypes"; ValueType: string; ValueName: ".aifc"; ValueData: ""; Tasks: openwith
Root: HKA; Subkey: "Software\Classes\Applications\losslessaudiocut.exe\SupportedTypes"; ValueType: string; ValueName: ".mka"; ValueData: ""; Tasks: openwith

[Icons]
Name: "{autoprograms}\losslessaudiocut"; Filename: "{app}\losslessaudiocut.exe"; IconFilename: "{app}\losslessaudiocut.ico"
Name: "{autodesktop}\losslessaudiocut"; Filename: "{app}\losslessaudiocut.exe"; IconFilename: "{app}\losslessaudiocut.ico"; Tasks: desktopicon

[Run]
Filename: "{app}\losslessaudiocut.exe"; Description: "{cm:LaunchProgram,losslessaudiocut}"; Flags: nowait postinstall skipifsilent
