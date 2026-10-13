# Native Windows installer. Downloads are always checked before extraction.
[CmdletBinding()]
param(
    [string]$Version = $env:LOOP_VERSION,
    [string]$InstallDir = $env:LOOP_INSTALL_DIR,
    [string]$Binary,
    [string]$Checksum,
    [switch]$Force,
    [switch]$NoModifyPath
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
if (-not $InstallDir) { $InstallDir = Join-Path $HOME '.loop\bin' }
$InstallDir = [IO.Path]::GetFullPath($InstallDir)
$stateDir = Join-Path $InstallDir '.loop-update'
$lockDir = Join-Path $stateDir 'operation.lock'
$destination = Join-Path $InstallDir 'loop.exe'
$stageDir = $null
$lockHeld = $false

function Get-LoopVersion([string]$Path) {
    $output = & $Path --version 2>$null
    if ($LASTEXITCODE -ne 0) { throw "Binary version check failed: $Path" }
    $value = (($output -join "`n").Trim() -replace '^loop ', '')
    if ($value -notmatch '^\d+\.\d+\.\d+([+-][A-Za-z0-9.-]+)?$') { throw 'Invalid binary version' }
    return $value
}
function Assert-Regular([string]$Path) {
    $item = Get-Item -LiteralPath $Path -Force
    if ($item.PSIsContainer -or ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
        throw "Expected a regular file: $Path"
    }
}
function Get-Sha256([string]$Path) {
    return (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}
function Add-LoopToPath {
    if ($NoModifyPath) { return }
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if ($InstallDir -notin ($userPath -split ';')) {
        [Environment]::SetEnvironmentVariable('Path', ($InstallDir + ';' + $userPath), 'User')
    }
    if ($InstallDir -notin ($env:Path -split ';')) { $env:Path = "$InstallDir;$env:Path" }
}

try {
    if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) { throw 'Use install.sh on Linux and macOS' }
    if ([Environment]::GetEnvironmentVariable('PROCESSOR_ARCHITECTURE') -ne 'AMD64' -and
        [Environment]::GetEnvironmentVariable('PROCESSOR_ARCHITEW6432') -ne 'AMD64') {
        throw 'Native Windows releases support x86_64 only'
    }
    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    if (Test-Path -LiteralPath $stateDir) {
        $state = Get-Item -LiteralPath $stateDir -Force
        if (-not $state.PSIsContainer -or ($state.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
            throw 'Update state directory must be a real directory'
        }
    } else { New-Item -ItemType Directory -Path $stateDir | Out-Null }
    # CreateDirectoryW fails if the directory exists. Directory.CreateDirectory
    # and New-Item's existence check cannot establish exclusive ownership.
    if (-not ('Soket.LoopInstallLock' -as [type])) {
        Add-Type -Namespace Soket -Name LoopInstallLock -MemberDefinition @'
[System.Runtime.InteropServices.DllImport("kernel32.dll", CharSet = System.Runtime.InteropServices.CharSet.Unicode, SetLastError = true)]
[return: System.Runtime.InteropServices.MarshalAs(System.Runtime.InteropServices.UnmanagedType.Bool)]
public static extern bool CreateDirectoryW(string path, System.IntPtr attributes);
'@
    }
    if (-not [Soket.LoopInstallLock]::CreateDirectoryW($lockDir, [IntPtr]::Zero)) {
        $code = [Runtime.InteropServices.Marshal]::GetLastWin32Error()
        throw "Cannot acquire installation lock (Windows error $code). If a prior operation crashed, remove $lockDir only after confirming no installer or updater is running."
    }
    $lockHeld = $true
    $stageDir = Join-Path $stateDir ('stage-' + [Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $stageDir | Out-Null
    $candidate = Join-Path $stageDir 'loop.exe'
    $tag = $null

    if ($Binary) {
        Assert-Regular $Binary
        if ($Checksum -notmatch '^[a-fA-F0-9]{64}$') { throw '-Binary requires -Checksum with a SHA-256 hash' }
        if ((Get-Sha256 $Binary) -ne $Checksum.ToLowerInvariant()) { throw 'Local binary checksum mismatch' }
        Copy-Item -LiteralPath $Binary -Destination $candidate
    } else {
        if ($Version) {
            $Version = $Version -replace '^v', ''
            if ($Version -notmatch '^\d+\.\d+\.\d+([+-][A-Za-z0-9.-]+)?$') { throw 'Invalid release version' }
            $tag = "v$Version"
        } else {
            $headers = @{ Accept = 'application/vnd.github+json'; 'User-Agent' = 'loop-installer' }
            if ($env:GITHUB_TOKEN) { $headers.Authorization = "Bearer $env:GITHUB_TOKEN" }
            $release = Invoke-RestMethod -Uri 'https://api.github.com/repos/soketlabs/loop/releases/latest' -Headers $headers -TimeoutSec 20
            if ($release.draft -or $release.prerelease) { throw 'Latest release is not stable' }
            $tag = $release.tag_name
            if ($tag -notmatch '^v\d+\.\d+\.\d+([+-][A-Za-z0-9.-]+)?$') { throw 'Invalid release version' }
            $Version = $tag.Substring(1)
        }
        if ((Test-Path -LiteralPath $destination) -and -not $Force) {
            Assert-Regular $destination
            if ((Get-LoopVersion $destination) -eq $Version) {
                Write-Host "Loop $Version is already installed. Use -Force to reinstall."
                Add-LoopToPath
                return
            }
        }
        $asset = 'loop-x86_64-pc-windows-msvc.zip'
        $url = "https://github.com/soketlabs/loop/releases/download/$tag/$asset"
        $archive = Join-Path $stageDir $asset
        $hashFile = Join-Path $stageDir "$asset.sha256"
        Write-Host "Downloading Loop $Version"
        Invoke-WebRequest -Uri "$url.sha256" -OutFile $hashFile -UseBasicParsing -TimeoutSec 30
        $fields = ((Get-Content -LiteralPath $hashFile -Raw).Trim() -split '\s+')
        if ($fields.Count -lt 1 -or $fields.Count -gt 2 -or $fields[0] -notmatch '^[a-fA-F0-9]{64}$' -or
            ($fields.Count -eq 2 -and $fields[1].TrimStart('*') -ne $asset)) {
            throw 'Missing or invalid SHA-256 checksum'
        }
        Invoke-WebRequest -Uri $url -OutFile $archive -UseBasicParsing -TimeoutSec 300
        if ((Get-Sha256 $archive) -ne $fields[0].ToLowerInvariant()) { throw 'Release archive checksum mismatch' }
        Add-Type -AssemblyName System.IO.Compression.FileSystem
        $zip = [IO.Compression.ZipFile]::OpenRead($archive)
        try {
            $entries = @($zip.Entries | Where-Object { $_.FullName -eq 'loop-x86_64-pc-windows-msvc/loop.exe' })
            if ($entries.Count -ne 1) { throw 'Archive must contain one expected executable' }
            if (($entries[0].ExternalAttributes -shr 16 -band 0xF000) -eq 0xA000) { throw 'Executable must not be a symlink' }
            [IO.Compression.ZipFileExtensions]::ExtractToFile($entries[0], $candidate, $false)
        } finally { $zip.Dispose() }
    }

    $candidateVersion = Get-LoopVersion $candidate
    if ($tag -and $candidateVersion -ne $Version) { throw 'Staged binary version does not match the release' }
    $recordPath = Join-Path $stateDir 'rollback.json'
    $savedRecord = $null
    $retired = $null
    if (Test-Path -LiteralPath $destination) {
        Assert-Regular $destination
        $oldVersion = Get-LoopVersion $destination
        $oldHash = Get-Sha256 $destination
        $backup = Join-Path $stateDir "previous-$oldHash.exe"
        Copy-Item -LiteralPath $destination -Destination $backup -Force
        if (Test-Path -LiteralPath $recordPath) { $savedRecord = [IO.File]::ReadAllBytes($recordPath) }
        $record = @{ schema = 1; current_sha256 = (Get-Sha256 $candidate); previous_sha256 = $oldHash; previous_version = $oldVersion }
        $record | ConvertTo-Json -Compress | Set-Content -LiteralPath "$recordPath.new" -Encoding ascii
        Move-Item -LiteralPath "$recordPath.new" -Destination $recordPath -Force
        $retired = Join-Path $stateDir ('retired-' + [Guid]::NewGuid().ToString('N') + '.exe')
        try { Move-Item -LiteralPath $destination -Destination $retired } catch {
            if ($savedRecord) { [IO.File]::WriteAllBytes($recordPath, $savedRecord) }
            else { Remove-Item -LiteralPath $recordPath -Force }
            throw
        }
    }
    try { Move-Item -LiteralPath $candidate -Destination $destination } catch {
        if ($retired) { Move-Item -LiteralPath $retired -Destination $destination }
        if ($savedRecord) { [IO.File]::WriteAllBytes($recordPath, $savedRecord) }
        elseif (Test-Path -LiteralPath $recordPath) { Remove-Item -LiteralPath $recordPath -Force }
        throw
    }
    if ($retired) { Remove-Item -LiteralPath $retired -Force -ErrorAction SilentlyContinue }

    Add-LoopToPath
    Write-Host "Installed Loop $candidateVersion in $InstallDir. Restart your terminal and run loop."
} finally {
    if ($stageDir -and (Test-Path -LiteralPath $stageDir)) { Remove-Item -LiteralPath $stageDir -Recurse -Force }
    if ($lockHeld) { Remove-Item -LiteralPath $lockDir -Force }
}
