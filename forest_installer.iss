[Setup]
AppName=Forest Package Manager
AppVersion=0.1.0
PrivilegesRequired=lowest
DefaultDirName={userpf}\Forest Package Manager
ChangesEnvironment=yes

[Files]
Source: "target\release\forest.exe"; DestDir: "{app}"; Flags: ignoreversion

[Registry]
Root: HKCU; Subkey: "Environment"; \
    ValueType: expandsz; ValueName: "Path"; ValueData: "{olddata};{app}"