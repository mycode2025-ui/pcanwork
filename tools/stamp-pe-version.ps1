[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string]$Executable,
    [Parameter(Mandatory)]
    [ValidatePattern('^\d+\.\d+\.\d+$')]
    [string]$Version,
    [Parameter(Mandatory)]
    [string]$ProductName,
    [Parameter(Mandatory)]
    [string]$Description,
    [string]$CompanyName = 'XCharge'
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$executablePath = (Resolve-Path -LiteralPath $Executable).Path

if (-not ('PcanWorkBuild.PeVersionStamper' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.ComponentModel;
using System.IO;
using System.Runtime.InteropServices;
using System.Text;

namespace PcanWorkBuild {
    public static class PeVersionStamper {
        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        static extern IntPtr BeginUpdateResource(string fileName, bool deleteExistingResources);

        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        static extern bool UpdateResource(IntPtr update, IntPtr type, IntPtr name,
            ushort language, byte[] data, uint dataSize);

        [DllImport("kernel32.dll", SetLastError = true)]
        static extern bool EndUpdateResource(IntPtr update, bool discard);

        static void Align4(BinaryWriter writer) {
            while ((writer.BaseStream.Position & 3) != 0) writer.Write((byte)0);
        }

        static byte[] Block(string key, ushort valueLength, ushort type,
            byte[] value, params byte[][] children) {
            using (var stream = new MemoryStream())
            using (var writer = new BinaryWriter(stream, Encoding.Unicode)) {
                writer.Write((ushort)0);
                writer.Write(valueLength);
                writer.Write(type);
                writer.Write(Encoding.Unicode.GetBytes(key + "\0"));
                Align4(writer);
                if (value != null && value.Length != 0) writer.Write(value);
                Align4(writer);
                foreach (var child in children) {
                    writer.Write(child);
                    Align4(writer);
                }
                var result = stream.ToArray();
                var size = checked((ushort)result.Length);
                result[0] = (byte)(size & 0xFF);
                result[1] = (byte)(size >> 8);
                return result;
            }
        }

        static byte[] TextValue(string key, string value) {
            var bytes = Encoding.Unicode.GetBytes(value + "\0");
            return Block(key, checked((ushort)(value.Length + 1)), 1, bytes);
        }

        static byte[] FixedInfo(ushort major, ushort minor, ushort patch, ushort revision) {
            using (var stream = new MemoryStream())
            using (var writer = new BinaryWriter(stream)) {
                uint ms = ((uint)major << 16) | minor;
                uint ls = ((uint)patch << 16) | revision;
                writer.Write(0xFEEF04BDu);
                writer.Write(0x00010000u);
                writer.Write(ms);
                writer.Write(ls);
                writer.Write(ms);
                writer.Write(ls);
                writer.Write(0x0000003Fu);
                writer.Write(0u);
                writer.Write(0x00040004u);
                writer.Write(1u);
                writer.Write(0u);
                writer.Write(0u);
                writer.Write(0u);
                return stream.ToArray();
            }
        }

        static byte[] VersionResource(string version, string productName,
            string description, string companyName, string fileName) {
            var parts = version.Split('.');
            ushort major = UInt16.Parse(parts[0]);
            ushort minor = UInt16.Parse(parts[1]);
            ushort patch = UInt16.Parse(parts[2]);
            var strings = Block("040904B0", 0, 1, null,
                TextValue("CompanyName", companyName),
                TextValue("FileDescription", description),
                TextValue("FileVersion", version),
                TextValue("InternalName", fileName),
                TextValue("OriginalFilename", fileName),
                TextValue("ProductName", productName),
                TextValue("ProductVersion", version));
            var stringInfo = Block("StringFileInfo", 0, 1, null, strings);
            var translation = new byte[] { 0x09, 0x04, 0xB0, 0x04 };
            var varEntry = Block("Translation", 4, 0, translation);
            var varInfo = Block("VarFileInfo", 0, 1, null, varEntry);
            var fixedInfo = FixedInfo(major, minor, patch, 0);
            return Block("VS_VERSION_INFO", checked((ushort)fixedInfo.Length), 0,
                fixedInfo, stringInfo, varInfo);
        }

        public static void Stamp(string file, string version, string productName,
            string description, string companyName) {
            var data = VersionResource(version, productName, description, companyName,
                Path.GetFileName(file));
            var update = BeginUpdateResource(file, false);
            if (update == IntPtr.Zero) throw new Win32Exception(Marshal.GetLastWin32Error());
            bool committed = false;
            try {
                foreach (ushort language in new ushort[] { 0, 0x0409 }) {
                    if (!UpdateResource(update, new IntPtr(16), new IntPtr(1), language,
                        data, (uint)data.Length))
                        throw new Win32Exception(Marshal.GetLastWin32Error());
                }
                if (!EndUpdateResource(update, false))
                    throw new Win32Exception(Marshal.GetLastWin32Error());
                committed = true;
            } finally {
                if (!committed) EndUpdateResource(update, true);
            }
        }
    }
}
'@
}

[PcanWorkBuild.PeVersionStamper]::Stamp(
    $executablePath, $Version, $ProductName, $Description, $CompanyName
)

$actual = (Get-Item -LiteralPath $executablePath).VersionInfo.FileVersion
if ($actual -ne $Version) {
    throw "PE version stamp verification failed: expected $Version, got $actual ($executablePath)"
}
Write-Output $executablePath
