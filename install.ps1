# Installs open-live-ingest on Windows (x86_64), and the Strom release it is built
# against, from the GitHub releases. The PowerShell counterpart of install.sh:
#
#   irm https://raw.githubusercontent.com/Eyevinn/open-live-ingest/main/install.ps1 | iex
#
# Runs in Windows PowerShell 5.1 and PowerShell 7. Nothing is asked, except the
# Windows elevation prompt when Strom has to be installed (see below).
#
# Environment:
#   INSTALL_DIR     where the gateway goes (default: %LOCALAPPDATA%\Programs\open-live-ingest)
#   VERSION         open-live-ingest release tag to install (default: latest)
#   SKIP_STROM      set to "true" to leave Strom alone
#   INSTALL_STROM   set to "always" to (re)install Strom even if one is already present
#   GH_TOKEN        optional GitHub token for the Strom release lookup (GITHUB_TOKEN works
#                   too), for when the unauthenticated API rate limit is used up
#
# The gateway goes in a per-user folder that is added to the user PATH, so it needs
# no administrator rights. Strom comes as an MSI that installs per machine, under
# %ProgramFiles%\Strom, with GStreamer bundled; that needs administrator rights, so
# the script asks Windows to elevate msiexec when the session is not elevated.
# The MSI is only run once it matches the SHA-256 digest GitHub records for it; if
# that digest cannot be read the script stops (SKIP_STROM=true and a manual Strom
# install is the way past that).
#
# Strom is pinned to $StromVersion below. That tag must match the strom-types pin in
# Cargo.toml; a test in tests/pins.rs checks it.

& {
$StromVersion = "v0.8.2"

$ErrorActionPreference = 'Stop'
# Invoke-WebRequest's progress bar slows downloads down by an order of magnitude in 5.1.
$ProgressPreference = 'SilentlyContinue'
# Windows PowerShell 5.1 may not offer TLS 1.2 by default; GitHub requires it.
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

$Repo = 'Eyevinn/open-live-ingest'
$Bin = 'open-live-ingest'
$Target = 'x86_64-pc-windows-msvc'
$StromRepo = 'Eyevinn/strom'

function Say([string]$Message) { Write-Host "==> $Message" }
# `throw`, not `exit`: under `irm | iex` an exit would close the user's window.
function Die([string]$Message) { throw "error: $Message" }

function Get-Setting([string]$Name, [string]$Default) {
    $value = [Environment]::GetEnvironmentVariable($Name)
    if ([string]::IsNullOrEmpty($value)) { $Default } else { $value }
}

function Get-File([string]$Url, [string]$OutFile) {
    Invoke-WebRequest -UseBasicParsing -Uri $Url -OutFile $OutFile
}

function Get-Sha256([string]$Path) {
    (Get-FileHash -Algorithm SHA256 -LiteralPath $Path).Hash.ToLowerInvariant()
}

# Dies unless the file at $Path matches $Digest, a GitHub `sha256:<hex>` asset digest.
function Assert-Digest([string]$Path, [string]$Digest) {
    $have = Get-Sha256 $Path
    if ("sha256:$have" -ne $Digest) {
        Die "$(Split-Path -Leaf $Path): SHA-256 $have does not match the release ($Digest)"
    }
}

function Test-Elevated {
    $principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
    $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

function Assert-Platform {
    if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) {
        Die "install.ps1 is for Windows; on Linux and macOS use install.sh"
    }
    # PROCESSOR_ARCHITEW6432 is set when a 32-bit PowerShell runs on 64-bit Windows.
    $arch = $env:PROCESSOR_ARCHITEW6432
    if (-not $arch) { $arch = $env:PROCESSOR_ARCHITECTURE }
    if ($arch -ne 'AMD64') {
        Die "unsupported architecture: $arch (only x86_64 Windows has a release)"
    }
}

# Adds $Dir to the user PATH, unless it is there already. The value is read
# unexpanded so entries such as %USERPROFILE%\bin survive the rewrite.
function Add-UserPath([string]$Dir) {
    $key = Get-Item -LiteralPath 'HKCU:\Environment'
    $raw = $key.GetValue('Path', '', 'DoNotExpandEnvironmentNames')
    $entries = @($raw -split ';' | Where-Object { $_ })
    $want = $Dir.TrimEnd('\')
    foreach ($entry in $entries) {
        if ([Environment]::ExpandEnvironmentVariables($entry).TrimEnd('\') -ieq $want) { return $false }
    }
    $new = (@($entries) + $Dir) -join ';'
    if ($new -notmatch '%') {
        [Environment]::SetEnvironmentVariable('Path', $new, 'User')
    } else {
        # SetEnvironmentVariable would store a plain string, so %VAR% entries would stop
        # expanding. Write the value as an expandable string, then announce the change
        # (setting and clearing a scratch variable broadcasts WM_SETTINGCHANGE).
        Set-ItemProperty -LiteralPath 'HKCU:\Environment' -Name 'Path' -Value $new -Type ExpandString
        [Environment]::SetEnvironmentVariable('OLI_INSTALL_PATH_CHANGED', '1', 'User')
        [Environment]::SetEnvironmentVariable('OLI_INSTALL_PATH_CHANGED', $null, 'User')
    }
    return $true
}

function Install-Gateway([string]$Tmp) {
    $version = Get-Setting 'VERSION' 'latest'
    $asset = "$Bin-$Target.zip"
    if ($version -eq 'latest') {
        $base = "https://github.com/$Repo/releases/latest/download"
    } else {
        $base = "https://github.com/$Repo/releases/download/$version"
    }

    Say "Downloading $asset ($version)"
    try { Get-File "$base/$asset" (Join-Path $Tmp $asset) }
    catch { Die "no release asset $asset at $base. Is there a release for Windows? ($($_.Exception.Message))" }
    try { Get-File "$base/SHA256SUMS" (Join-Path $Tmp 'SHA256SUMS') }
    catch { Die "could not download SHA256SUMS ($($_.Exception.Message))" }

    # Lines read `<hash>  <name>` (sha256sum), or `<hash> *<name>` in binary mode.
    $want = $null
    foreach ($line in Get-Content -LiteralPath (Join-Path $Tmp 'SHA256SUMS')) {
        $fields = $line.Trim() -split '\s+', 2
        if ($fields.Count -eq 2 -and $fields[1].TrimStart('*') -ceq $asset) {
            $want = $fields[0].ToLowerInvariant()
            break
        }
    }
    if (-not $want) { Die "$asset is not listed in SHA256SUMS" }
    $have = Get-Sha256 (Join-Path $Tmp $asset)
    if ($want -ne $have) { Die "checksum mismatch for $asset" }

    $unpacked = Join-Path $Tmp 'unpacked'
    Expand-Archive -LiteralPath (Join-Path $Tmp $asset) -DestinationPath $unpacked
    # The archive holds a folder named after the asset, like the tarballs; finding
    # the binary rather than assuming the layout keeps a flat archive working too.
    $exe = Get-ChildItem -LiteralPath $unpacked -Recurse -File -Filter "$Bin.exe" | Select-Object -First 1
    if (-not $exe) { Die "$asset does not contain $Bin.exe" }

    $dir = Get-Setting 'INSTALL_DIR' (Join-Path $env:LOCALAPPDATA "Programs\$Bin")
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    $dest = Join-Path $dir "$Bin.exe"
    try { Copy-Item -LiteralPath $exe.FullName -Destination $dest -Force }
    catch { Die "could not write $dest. Is $Bin still running? Stop it (``$Bin down``) and rerun. ($($_.Exception.Message))" }
    Say "Installed $dest ($(& $dest --version))"

    if (Add-UserPath $dir) {
        Say "Added $dir to your user PATH; consoles opened from now on find $Bin"
    }
    # This console too, so the next command works without opening a new one.
    if (-not (($env:Path -split ';') | Where-Object { $_.TrimEnd('\') -ieq $dir.TrimEnd('\') })) {
        $env:Path = "$env:Path;$dir"
    }
}

# The 64-bit Program Files folder, where the x64 Strom MSI installs. A 32-bit
# PowerShell on 64-bit Windows sees `C:\Program Files (x86)` as ProgramFiles;
# ProgramW6432 holds the 64-bit one in both 32- and 64-bit processes.
function Get-ProgramFiles64 {
    if ($env:ProgramW6432) { $env:ProgramW6432 } else { $env:ProgramFiles }
}

function Find-Strom {
    $cmd = Get-Command strom.exe -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($cmd) { return $cmd.Source }
    $msi = Join-Path (Get-ProgramFiles64) 'Strom\bin\strom.exe'
    if (Test-Path -LiteralPath $msi) { return $msi }
    return $null
}

function Install-Strom([string]$Tmp) {
    if ((Get-Setting 'SKIP_STROM' 'false') -eq 'true') {
        Say "Skipping Strom (SKIP_STROM=true)"
        return
    }
    $found = Find-Strom
    if ($found -and (Get-Setting 'INSTALL_STROM' 'missing') -ne 'always') {
        Say "Strom already installed at $found; leaving it alone (INSTALL_STROM=always to replace)"
        return
    }

    $asset = "strom-$($StromVersion.TrimStart('v'))-windows-x86_64.msi"
    $msi = Join-Path $Tmp $asset
    Say "Downloading Strom $StromVersion ($asset)"
    try { Get-File "https://github.com/$StromRepo/releases/download/$StromVersion/$asset" $msi }
    catch { Die "could not download $asset from the Strom $StromVersion release ($($_.Exception.Message))" }

    # Strom ships no checksum file of its own; the check is against the SHA-256 digest
    # GitHub records for each release asset. Like build-macos-app.sh, this fails closed:
    # no digest, no install, because the MSI is about to run with administrator rights.
    $wayOut = "Set SKIP_STROM=true to install only the gateway, then install Strom $StromVersion yourself from https://github.com/$StromRepo/releases/tag/$StromVersion (the MSI is $asset)"
    $headers = @{}
    $token = Get-Setting 'GH_TOKEN' (Get-Setting 'GITHUB_TOKEN' '')
    if ($token) { $headers['Authorization'] = "Bearer $token" }
    try {
        $release = Invoke-RestMethod -UseBasicParsing -Headers $headers -Uri "https://api.github.com/repos/$StromRepo/releases/tags/$StromVersion"
    } catch {
        Die "cannot read the Strom $StromVersion release from the GitHub API to get the SHA-256 digest of $asset, so it is not installed ($($_.Exception.Message)). $wayOut"
    }
    $digest = [string]($release.assets | Where-Object { $_.name -ceq $asset } | Select-Object -First 1).digest
    if ($digest -notlike 'sha256:*') {
        Die "GitHub records no SHA-256 digest for $asset, so it is not installed. $wayOut"
    }
    $digest = $digest.ToLowerInvariant()

    $programFiles = Get-ProgramFiles64
    # Outside $Tmp, which is removed on the way out, so the log outlives a failure.
    $log = Join-Path ([IO.Path]::GetTempPath()) 'open-live-ingest-strom-msi.log'
    $msiArgs = "/i `"$msi`" /passive /norestart /l*v `"$log`""
    # Hashed here, after the API round trip and right before msiexec, so the MSI in
    # %TEMP% has as little time as possible to change between the check and the run.
    Assert-Digest $msi $digest
    Say "Checked $asset against the SHA-256 digest GitHub records for it"
    if (Test-Elevated) {
        Say "Installing Strom $StromVersion (GStreamer included) under $programFiles\Strom"
        $proc = Start-Process msiexec.exe -ArgumentList $msiArgs -Wait -PassThru
    } else {
        Say "Strom installs for all users under $programFiles\Strom, which needs administrator rights."
        Say "Windows will ask to allow msiexec to make changes; the gateway itself was installed without them."
        try {
            $proc = Start-Process msiexec.exe -ArgumentList $msiArgs -Verb RunAs -Wait -PassThru
        } catch {
            Die "Strom was not installed: elevation was refused ($($_.Exception.Message)). Rerun from an administrator PowerShell, or set SKIP_STROM=true and install Strom yourself."
        }
    }
    # 3010: installed, a restart is needed to finish.
    if ($proc.ExitCode -eq 3010) {
        Say "Strom installed; Windows asks for a restart to finish"
    } elseif ($proc.ExitCode -ne 0) {
        Die "msiexec exited with $($proc.ExitCode) installing $asset; its log is $log"
    }
    $installed = Join-Path $programFiles 'Strom\bin\strom.exe'
    if (-not (Test-Path -LiteralPath $installed)) { Die "msiexec succeeded but $installed is missing; its log is $log" }
    Say "Installed $installed"
    Say "Strom's installer set the system PATH and GST_PLUGIN_PATH; open a new console to pick them up"
}

Assert-Platform
$tmp = Join-Path ([IO.Path]::GetTempPath()) ("open-live-ingest-install-" + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
    Install-Gateway $tmp
    Install-Strom $tmp
} finally {
    Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue
}
Say "Done. Next: $Bin setup, then $Bin up"
}
