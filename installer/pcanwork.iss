; PcanWork 安装包脚本 (Inno Setup 6)
; 安装到 C:\Program Files\PcanWork（需管理员权限，由 PrivilegesRequired=admin 触发 UAC 提权）。
; 打包三个独立 exe（pcanwork / 串口工具 / Modbus 工具）+ 全部厂商 CAN 驱动 DLL + 运行时数据。
; 编译: "C:\Users\XCHARGE-2026Q1-LT08\AppData\Local\Programs\Inno Setup 6\ISCC.exe" installer\pcanwork.iss

#define Root "D:\_Xcharge\Pcanwork"
#define AppVer "0.1.11"

[Setup]
AppName=PcanWork
AppVersion={#AppVer}
AppPublisher=XCharge
DefaultDirName={autopf}\PcanWork
DefaultGroupName=PcanWork
; 申请管理员权限（安装到 C:\Program Files 必需，弹 UAC 提权）
PrivilegesRequired=admin
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir={#Root}\installer\dist
OutputBaseFilename=PcanWork-Setup-{#AppVer}
SetupIconFile={#Root}\app.ico
UninstallDisplayIcon={app}\pcanwork.exe
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
ChangesAssociations=yes

[Languages]
; 此 Inno Setup 未自带简体中文语言文件，安装向导用英文(app 本身仍是中文)。
Name: "en"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"

[Files]
; ---- 三个主程序 ----
Source: "{#Root}\target\release\pcanwork.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Root}\target\release\serial-tool.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Root}\target\release\modbus-tools.exe"; DestDir: "{app}"; Flags: ignoreversion
; ---- 运行时数据 ----
Source: "{#Root}\pcanwork.py"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Root}\aaaaa.dbc"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Root}\app.ico"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Root}\templates\*.py"; DestDir: "{app}\templates"; Flags: ignoreversion
; ---- Modbus 自签名测试证书 ----
Source: "{#Root}\modbus\certs\*"; DestDir: "{app}\certs"; Flags: ignoreversion
; ---- ZLG (USBCANFD / 新版) 驱动 + 内核 DLL ----
Source: "{#Root}\zlgcan_x64\zlgcan.dll"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Root}\zlgcan_x64\kerneldlls\*"; DestDir: "{app}\kerneldlls"; Flags: ignoreversion recursesubdirs
; ---- GCAN (广成) 驱动 ----
Source: "{#Root}\GCAN\x64\ECanVci64.dll"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Root}\GCAN\x64\CHUSBDLL64.dll"; DestDir: "{app}"; Flags: ignoreversion
; ---- ZHCX 驱动 ----
Source: "{#Root}\zhcxCAN\x64\ControlCAN.dll"; DestDir: "{app}"; Flags: ignoreversion
; 注: PCAN(PEAK) 的 PCANBasic.dll 由 PEAK 官方驱动包安装到系统目录，不随本包分发。


[Registry]
Root: HKCR; Subkey: ".pcprj"; ValueType: string; ValueName: ""; ValueData: "PcanWork.Project"; Flags: uninsdeletevalue
Root: HKCR; Subkey: ".pcprj"; ValueType: string; ValueName: "Content Type"; ValueData: "application/x-pcanwork-project"; Flags: uninsdeletevalue
Root: HKCR; Subkey: "PcanWork.Project"; ValueType: string; ValueName: ""; ValueData: "PcanWork Project"; Flags: uninsdeletekey
Root: HKCR; Subkey: "PcanWork.Project\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\pcanwork.exe,0"
Root: HKCR; Subkey: "PcanWork.Project\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\pcanwork.exe"" ""%1"""

[Icons]
Name: "{group}\PcanWork"; Filename: "{app}\pcanwork.exe"; WorkingDir: "{app}"; IconFilename: "{app}\app.ico"
Name: "{group}\卸载 PcanWork"; Filename: "{uninstallexe}"
Name: "{autodesktop}\PcanWork"; Filename: "{app}\pcanwork.exe"; WorkingDir: "{app}"; IconFilename: "{app}\app.ico"; Tasks: desktopicon

[Run]
Filename: "{app}\pcanwork.exe"; Description: "{cm:LaunchProgram,PcanWork}"; Flags: nowait postinstall skipifsilent
