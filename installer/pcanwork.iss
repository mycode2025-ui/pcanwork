; PcanWork 闁诲海鎳撻ˇ鎶剿夋繝鍥х闁告稒婢樻竟鏍煛?(Inno Setup 6)
; 闁诲海鎳撻ˇ鎶剿夋繝鍥х?C:\Program Files\PcanWork闂佹寧绋戦悧鎾炽€掗崜浣轰笉闁挎稑瀚崐鐐烘煕濞戞鎴濐焽閸儲鈷旈柟鏉垮缁€澶愭煟?PrivilegesRequired=admin 闁荤喐鐟辩粻鎴ｃ亹?UAC 闂佸湱绮崝鏍ь焽閸儲鏅璺虹墐閸?; 闂佺懓鐏氶幐鍝モ偓鍨戠粙澶嬪緞婢舵劕娈濋梺缁樼懐閸撴盯鎮?exe闂佹寧绋戝绌媋nwork / 婵炴垶鎸昏ぐ鍐亹濞戞﹩鍟呴柕澶堝劚瀵?/ Modbus 閻庤鎮堕崕閬嶅矗閸ф鏅? 闂佺绻堥崝鎴﹀磿閹绢喖鍌ㄩ柛灞剧濞?CAN 婵＄偟鎳撳畷顒佹叏?DLL + 闁哄鏅滈崝姗€銆侀幋锕€绫嶉柤绋跨仛濞堝爼鏌熺拠鈥虫灁闁?; 缂傚倸鍊归悧鐐烘儊? ISCC.exe installer\pcanwork.iss

#define Root SourcePath + "\.."
#define AppVer "0.4.0"

[Setup]
AppId={{2BD0E569-4F8D-4B31-A43A-5332CE87A30A}
AppName=PcanWork
AppVersion={#AppVer}
AppPublisher=XCharge
VersionInfoVersion={#AppVer}
VersionInfoProductVersion={#AppVer}
VersionInfoCompany=XCharge
VersionInfoDescription=PcanWork CAN, Serial and Modbus Engineering Tools
VersionInfoProductName=PcanWork
DefaultDirName={autopf}\PcanWork
DefaultGroupName=PcanWork
; 闂佹眹鍨归悿鍥敋椤掑倻涓嶉柨娑樺閸婄偤鏌涘☉娅虫垵顭囬崼銉︹挃闁规澘澧庣粈鍕倵閻熼偊妲兼い銉ワ躬瀹?C:\Program Files 闂婎偄娲ら幊妯恒€掗崼鏇熸櫖閻忕偛鈧喎澧?UAC 闂佸湱绮崝鏍ь焽閸儲鏅?PrivilegesRequired=admin
PrivilegesRequired=admin
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir={#Root}\installer\dist
OutputBaseFilename=PcanWork-Setup-{#AppVer}
SetupIconFile={#Root}\app.ico
UninstallDisplayIcon={app}\pcanwork.exe
#ifdef FastPackage
Compression=lzma2/fast
SolidCompression=no
#else
Compression=lzma2/max
SolidCompression=yes
#endif
WizardStyle=modern
ChangesAssociations=yes
SetupLogging=yes
; PcanWork owns the upgrade shutdown sequence below. Restart Manager cannot
; reliably close Slint/winit windows that have auxiliary tool windows open.
CloseApplications=no
RestartApplications=no
#ifndef SkipSigning
SignTool=release
SignedUninstaller=yes
#endif

[Languages]
; 濠?Inno Setup 闂佸搫鐗滄禍锝夊吹濠婂懏鏆滈柨鏂垮⒔閺嗗棗霉閿濆棙灏柤鍨灴瀵剟宕堕宥呮闁荤喎鐨濋崑鎾绘煛閸屾碍鐭楁繛鍡愬灲閺佸秶浠﹂悾灞炬闁荤喍绀侀幊搴ㄥ箖濠婂應鍋撴担鍐ㄤ汗闁轰降鍊濋幊鐔告綇閵婏妇鈧?app 闂佸搫鐗滈崜婵嬫閳哄倻顩风€广儱妫欑瑧婵炴垶鎼╅崢浠嬪几?闂?Name: "en"; MessagesFile: "compiler:Default.isl"
Name: "en"; MessagesFile: "compiler:Default.isl"

[Dirs]
Name: "{app}\kerneldlls\devices_property"; Permissions: users-modify

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"

[Files]
; ---- 婵炴垶鎸搁ˇ顕€鏌屽鍕枖闁煎鍊愰弻銈夊箹?----
#ifdef FastPackage
Source: "{#Root}\target\release-fast\pcanwork.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Root}\target\release-fast\pcanwork.exe.integrity"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Root}\target\release-fast\serial-tool.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Root}\target\release-fast\modbus-tools.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Root}\target\release-fast\modbus-tools.exe.integrity"; DestDir: "{app}"; Flags: ignoreversion
#else
Source: "{#Root}\target\release\pcanwork.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Root}\target\release\pcanwork.exe.integrity"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Root}\target\release\serial-tool.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Root}\target\release\modbus-tools.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Root}\target\release\modbus-tools.exe.integrity"; DestDir: "{app}"; Flags: ignoreversion
#endif
; ---- 闁哄鏅滈崝姗€銆侀幋锕€绫嶉柤绋跨仛濞堝爼鏌?----
Source: "{#Root}\pcanwork.py"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Root}\aaaaa.dbc"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Root}\app.ico"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Root}\assets\project.ico"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Root}\templates\*.py"; DestDir: "{app}\templates"; Flags: ignoreversion
; ---- ZLG (USBCANFD / 闂佸搫鍊规竟鍡樻櫠? 婵＄偟鎳撳畷顒佹叏?+ 闂佸憡鍔曢幊蹇涙偋?DLL ----
Source: "{#Root}\zlgcan_x64\zlgcan.dll"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Root}\zlgcan_x64\kerneldlls\*"; DestDir: "{app}\kerneldlls"; Flags: ignoreversion recursesubdirs
; ---- ZLG USBCAN-E-U / USBCAN-2E-U 闁诲氦顫夎摫闁哄瞼鍠撶划鐢稿箚瑜庨崐鍐差渻閻熸澘鑸瑰┑顔规櫇閳ь剛鎳撻ˇ鎶剿夋繝鍥х?----
Source: "{#Root}\drivers\zlg-usbcan-e-u\*"; DestDir: "{app}\drivers\zlg-usbcan-e-u"; Flags: ignoreversion recursesubdirs
; ---- GCAN (濡ょ姷鍋炵€笛囧垂? 婵＄偟鎳撳畷顒佹叏?----
Source: "{#Root}\GCAN\x64\ECanVci64.dll"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Root}\GCAN\x64\CHUSBDLL64.dll"; DestDir: "{app}"; Flags: ignoreversion
; ---- ZHCX 婵＄偟鎳撳畷顒佹叏?----
Source: "{#Root}\zhcxCAN\x64\ControlCAN.dll"; DestDir: "{app}"; Flags: ignoreversion
; 濠? PCAN(PEAK) 闂?PCANBasic.dll 闂?PEAK 闁诲氦顫夎摫闁哄瞼鍠愰妵娆撳础閻愭畫妤呮煕閺嵮勫櫣闁伙絻鍔庨幉妤呭川椤撶偟鍘掔紓渚囧灥瀹曠數鍒掑ú顏呭剮妞ゆ棁鍋愮粔鍧楁煥濞戞ê顨欑紒妤€顦靛鎹愮疀閺冣偓閹烽亶鏌涢弽褎鍣归柛銊ラ叄瀹曪綁骞嬪▎灞戒壕?

[Registry]
Root: HKCR; Subkey: ".pcprj"; ValueType: string; ValueName: ""; ValueData: "PcanWork.Project"; Flags: uninsdeletevalue
Root: HKCR; Subkey: ".pcprj"; ValueType: string; ValueName: "Content Type"; ValueData: "application/x-pcanwork-project"; Flags: uninsdeletevalue
Root: HKCR; Subkey: "PcanWork.Project"; ValueType: string; ValueName: ""; ValueData: "PcanWork Project"; Flags: uninsdeletekey
Root: HKCR; Subkey: "PcanWork.Project\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\project.ico,0"
Root: HKCR; Subkey: "PcanWork.Project\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\pcanwork.exe"" ""%1"""

[Icons]
Name: "{group}\PcanWork"; Filename: "{app}\pcanwork.exe"; WorkingDir: "{app}"; IconFilename: "{app}\app.ico"
Name: "{group}\Modbus Tools"; Filename: "{app}\modbus-tools.exe"; WorkingDir: "{app}"; IconFilename: "{app}\modbus-tools.exe"
Name: "{group}\Serial Tool"; Filename: "{app}\serial-tool.exe"; WorkingDir: "{app}"; IconFilename: "{app}\serial-tool.exe"
Name: "{group}\卸载 PcanWork"; Filename: "{uninstallexe}"
Name: "{autodesktop}\PcanWork"; Filename: "{app}\pcanwork.exe"; WorkingDir: "{app}"; IconFilename: "{app}\app.ico"; Tasks: desktopicon
Name: "{autodesktop}\Modbus Tools"; Filename: "{app}\modbus-tools.exe"; WorkingDir: "{app}"; IconFilename: "{app}\modbus-tools.exe"; Tasks: desktopicon
Name: "{autodesktop}\Serial Tool"; Filename: "{app}\serial-tool.exe"; WorkingDir: "{app}"; IconFilename: "{app}\serial-tool.exe"; Tasks: desktopicon

[Run]
Filename: "{sys}\pnputil.exe"; Parameters: "/add-driver ""{app}\drivers\zlg-usbcan-e-u\usbcan_e_u_x64.inf"" /install"; StatusMsg: "Installing ZLG USBCAN-E-U driver..."; Flags: runhidden waituntilterminated
Filename: "{sys}\pnputil.exe"; Parameters: "/restart-device /deviceid ""USB\VID_0471&PID_1260"""; StatusMsg: "Restarting ZLG USBCAN-E-U device..."; Flags: runhidden waituntilterminated
Filename: "{app}\pcanwork.exe"; Description: "{cm:LaunchProgram,PcanWork}"; Flags: nowait postinstall skipifsilent

[Code]
procedure StopProductProcess(const ImageName: String);
var
  ResultCode: Integer;
begin
  { First request a normal GUI shutdown so projects/settings can be saved. }
  Exec(ExpandConstant('{sys}\taskkill.exe'),
    '/IM "' + ImageName + '" /T', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
  Sleep(800);
  { A hung/background process must not leave installed binaries locked. }
  Exec(ExpandConstant('{sys}\taskkill.exe'),
    '/F /IM "' + ImageName + '" /T', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  StopProductProcess('pcanwork.exe');
  StopProductProcess('serial-tool.exe');
  StopProductProcess('modbus-tools.exe');
  Result := '';
end;

procedure DeleteLegacyTestIdentity(const FileNamePart, ExpectedSha256: String);
var
  FileName: String;
  ActualSha256: String;
begin
  FileName := ExpandConstant('{app}\certs\') + FileNamePart;
  if not FileExists(FileName) then
    Exit;

  try
    ActualSha256 := Uppercase(GetSHA256OfFile(FileName));
    if ActualSha256 = ExpectedSha256 then
    begin
      if DeleteFile(FileName) then
        Log('Removed legacy bundled TLS test identity: ' + FileName)
      else
        Log('Failed to remove legacy bundled TLS test identity: ' + FileName);
    end
    else
      Log('Preserved user-managed TLS file with a non-fixture hash: ' + FileName);
  except
    Log('Could not inspect legacy TLS file; preserving it: ' + FileName +
      ' (' + GetExceptionMessage + ')');
  end;
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssInstall then
  begin
    DeleteLegacyTestIdentity('ca.crt',
      '7F732E67D7ABD1D314864B1D3A499808BEA3A69A9B9883F7A884EDF3CF0C4295');
    DeleteLegacyTestIdentity('ca.key',
      'EB1070AEB04E5345FB0D42425DB883D46CD25D4ED9E4E28707B6C04540401AED');
    DeleteLegacyTestIdentity('client.crt',
      '699B35A797BE929B18E43396373D2D42147CC82C008067D21CD4752AB3E33E57');
    DeleteLegacyTestIdentity('client.key',
      '29CF0383161F0106614894688210294737219B888A9D468A3C8F418895FFDA8B');
    DeleteLegacyTestIdentity('server.crt',
      'F58E6B8548703BCEE9C8DC64983A2C473D4A42834C456473950FE5BD2245A910');
    DeleteLegacyTestIdentity('server.key',
      '8CBF86766F65449C2C116D5E744A91395D8DDB9E9107A67D1EDD52B7B723B3C7');
    RemoveDir(ExpandConstant('{app}\certs'));
  end;
end;
