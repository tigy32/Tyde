param(
    [Parameter(Mandatory = $true)][string]$Binary,
    [Parameter(Mandatory = $true)][string]$ExpectedVersion,
    [Parameter(Mandatory = $true)][string]$LogDirectory
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$Binary = (Resolve-Path $Binary).Path
New-Item -ItemType Directory -Path $LogDirectory -Force | Out-Null
$LogDirectory = (Resolve-Path $LogDirectory).Path
$storeHome = Join-Path ([IO.Path]::GetTempPath()) ('tyde-startup-' + [Guid]::NewGuid())
New-Item -ItemType Directory -Path $storeHome | Out-Null
$environment = @{}
foreach ($name in @('HOME', 'TYDE_TRACING_DIR_PATH', 'RUST_LOG', 'RUST_BACKTRACE')) {
    $environment[$name] = [Environment]::GetEnvironmentVariable($name)
}
$process = [Diagnostics.Process]::new()
$stdout = [Collections.Generic.List[string]]::new()
$stderr = $null
$started = $false
$passed = $false
try {
    $env:HOME = $storeHome
    $env:TYDE_TRACING_DIR_PATH = Join-Path $storeHome 'tracing'
    $env:RUST_LOG = 'info'
    $env:RUST_BACKTRACE = 'full'
    $version = & $Binary --version
    if ($LASTEXITCODE -ne 0 -or $version.Trim() -ne $ExpectedVersion.TrimStart('v')) {
        throw 'The Windows desktop binary could not report the expected release version'
    }
    $process.StartInfo.FileName = $Binary
    $process.StartInfo.WorkingDirectory = Split-Path $Binary
    $process.StartInfo.UseShellExecute = $false
    $process.StartInfo.CreateNoWindow = $true
    $process.StartInfo.RedirectStandardOutput = $true
    $process.StartInfo.RedirectStandardError = $true
    if (-not $process.Start()) { throw 'Could not launch the Windows desktop binary' }
    $started = $true
    $stderr = $process.StandardError.ReadToEndAsync()
    $elapsed = [Diagnostics.Stopwatch]::StartNew()
    $frontendReady = $false
    $hostReady = $false
    while (-not ($frontendReady -and $hostReady)) {
        $remaining = 600000 - [int]$elapsed.ElapsedMilliseconds
        $line = $process.StandardOutput.ReadLineAsync()
        if ($remaining -le 0 -or -not $line.Wait($remaining)) {
            throw 'Windows startup test hung for ten minutes'
        }
        $text = $line.GetAwaiter().GetResult()
        if ($null -eq $text) {
            $process.WaitForExit()
            throw "Windows desktop exited before frontend and local host readiness (exit $($process.ExitCode))"
        }
        $stdout.Add($text)
        $frontendReady = $frontendReady -or $text.Contains('webview.lifecycle event=frontend_ready label=main ')
        $hostReady = $hostReady -or $text.Contains('proxy router received line from host')
    }
    $lock = Join-Path $storeHome '.tyde/tychat-secrets.lock'
    $acl = Get-Acl $lock
    $owner = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
    $rules = @($acl.Access)
    if (-not $acl.AreAccessRulesProtected -or $rules.Count -ne 1 -or
        $acl.GetOwner([Security.Principal.SecurityIdentifier]).Value -ne $owner -or
        $rules[0].IdentityReference.Translate([Security.Principal.SecurityIdentifier]).Value -ne $owner -or
        $rules[0].IsInherited -or
        $rules[0].AccessControlType -ne [Security.AccessControl.AccessControlType]::Allow -or
        $rules[0].FileSystemRights -ne [Security.AccessControl.FileSystemRights]::FullControl) {
        throw 'The Tychat process lock is not protected by an owner-only Windows ACL'
    }
    if ($process.WaitForExit(5000)) {
        throw "Windows desktop exited after frontend readiness (exit $($process.ExitCode))"
    }
    Write-Host 'PASS Windows desktop startup, frontend and local host readiness, and private store ACL'
    $passed = $true
} finally {
    if ($started -and -not $process.HasExited) { $process.Kill($true) }
    if ($stderr) {
        $process.WaitForExit()
        $stdout.Add($process.StandardOutput.ReadToEnd())
        $stdout | Set-Content (Join-Path $LogDirectory 'stdout.log')
        $stderrText = $stderr.GetAwaiter().GetResult()
        $stderrText | Set-Content (Join-Path $LogDirectory 'stderr.log')
        if (-not $passed) {
            $stdout | ForEach-Object { Write-Host $_ }
            Write-Host $stderrText
        }
    }
    $process.Dispose()
    foreach ($name in $environment.Keys) {
        [Environment]::SetEnvironmentVariable($name, $environment[$name])
    }
    Remove-Item $storeHome -Recurse -Force
}
