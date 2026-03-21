# apiKeyHelper for Claude Code — browser-based OIDC login (PowerShell).
# Works with any IDP configured on the proxy (Okta, Azure AD, etc.)
#
# Flow:
#   1. Check for cached token (valid JWT not yet expired)
#   2. If expired/missing, open browser for IDP login (with lock to prevent tab flood)
#   3. Poll proxy until login completes
#   4. Cache token (read-only for other users)
#
# Usage: proxy-token.ps1 [host]   (host passed by apiKeyHelper, defaults to env/hardcoded)
# Installed by: irm https://<proxy>/auth/setup?platform=windows | iex

param(
    [string]$ProxyHostArg
)

$ErrorActionPreference = 'Stop'

# Host can be passed as arg (from apiKeyHelper), env var, or default
$ProxyHost = if ($ProxyHostArg) { $ProxyHostArg }
             elseif ($env:CC_PROXY_HOST) { $env:CC_PROXY_HOST }
             else { 'localhost' }
$ProxyUrl = "https://$ProxyHost"

# Token storage: always under ~/.claude/tokens/ (avoids git concerns)
$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$TokenDir = Join-Path (Join-Path $env:USERPROFILE '.claude') 'tokens'
$HostSlug = $ProxyHost -replace '[.:/]', '-'
if (-not (Test-Path $TokenDir)) { New-Item -ItemType Directory -Path $TokenDir -Force | Out-Null }

$ClaudeDir = Join-Path $env:USERPROFILE '.claude'
if ($ScriptDir -eq $ClaudeDir) {
    # User-scoped: token file per host
    $TokenFile = Join-Path $TokenDir "proxy-token.$HostSlug"
} else {
    # Project-scoped: slug from project directory + host
    $ProjectDir = Split-Path -Parent $ScriptDir
    $ProjectSlug = ($ProjectDir -replace [regex]::Escape($env:USERPROFILE), '' -replace '[\\/]', '-').TrimStart('-')
    $TokenFile = Join-Path $TokenDir "proxy-token.$HostSlug.$ProjectSlug"
}
$LockFile = "$TokenFile.lock"
$FailFile = "$TokenFile.fail"
$TokenRefreshMargin = 60
$FailCooldownSecs = 30

# Decode JWT payload and extract a field (no external deps)
function Get-JwtField {
    param([string]$Token, [string]$Field)
    try {
        $parts = $Token.Split('.')
        if ($parts.Count -lt 2) { return $null }
        $payload = $parts[1]
        # Base64url to base64
        $payload = $payload.Replace('-', '+').Replace('_', '/')
        switch ($payload.Length % 4) {
            2 { $payload += '==' }
            3 { $payload += '=' }
        }
        $json = [System.Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($payload))
        $obj = $json | ConvertFrom-Json
        return $obj.$Field
    } catch {
        return $null
    }
}

# Check if a cached token is still valid
function Test-TokenValid {
    param([string]$Token)
    if (-not $Token) { return $false }
    $exp = Get-JwtField -Token $Token -Field 'exp'
    if (-not $exp) { return $false }
    $now = [int][double]::Parse((Get-Date -UFormat '%s'))
    return $now -lt ($exp - $TokenRefreshMargin)
}

# Try cached token first
$CachedToken = ''
if (Test-Path $TokenFile) {
    $CachedToken = (Get-Content $TokenFile -Raw).Trim()
    if (Test-TokenValid $CachedToken) {
        Write-Output $CachedToken
        exit 0
    }
}

# Check if we recently failed — don't spam browser tabs
if (Test-Path $FailFile) {
    $failAge = [int]((Get-Date) - (Get-Item $FailFile).LastWriteTime).TotalSeconds
    if ($failAge -lt $FailCooldownSecs) {
        if ($CachedToken) { Write-Output $CachedToken }
        exit 1
    }
    Remove-Item -Force $FailFile -ErrorAction SilentlyContinue
}

# Lock to prevent multiple concurrent browser flows (directory-based lock)
$lockAcquired = $false
try {
    New-Item -ItemType Directory -Path $LockFile -ErrorAction Stop | Out-Null
    $lockAcquired = $true
} catch {
    # Another instance holds the lock — wait for token file
    for ($i = 0; $i -lt 60; $i++) {
        if (Test-Path $TokenFile) {
            $Token = (Get-Content $TokenFile -Raw).Trim()
            if (Test-TokenValid $Token) {
                Write-Output $Token
                exit 0
            }
        }
        if (-not (Test-Path $LockFile)) {
            if ($CachedToken) { Write-Output $CachedToken }
            exit 1
        }
        Start-Sleep -Seconds 2
    }
    if ($CachedToken) { Write-Output $CachedToken }
    exit 1
}

try {
    # Start browser login flow
    $SessionId = [guid]::NewGuid().ToString()
    $AuthUrl = "$ProxyUrl/auth/cli/login?session=$SessionId"

    Start-Process $AuthUrl

    # Poll for token (2s interval, 2 min timeout)
    # Use appropriate web request method for PS 5.1 compat
    $useTls = $ProxyUrl.StartsWith('https://localhost') -or $ProxyUrl.StartsWith('https://127.0.0.1')
    if ($useTls) {
        # For local dev with self-signed certs, bypass certificate validation
        try {
            Add-Type @"
using System.Net;
using System.Net.Security;
using System.Security.Cryptography.X509Certificates;
public class TrustAll {
    public static void Enable() {
        ServicePointManager.ServerCertificateValidationCallback =
            delegate { return true; };
    }
}
"@
            [TrustAll]::Enable()
        } catch {
            # Type may already be added in this session
        }
    }

    for ($i = 0; $i -lt 60; $i++) {
        try {
            $pollUrl = "$ProxyUrl/auth/cli/poll?session=$SessionId"
            $response = Invoke-RestMethod -Uri $pollUrl -UseBasicParsing -ErrorAction Stop
        } catch {
            $response = $null
        }

        if ($response) {
            $status = $response.status
            switch ($status) {
                'complete' {
                    $Token = $response.token
                    if ($Token) {
                        $parentDir = Split-Path -Parent $TokenFile
                        if (-not (Test-Path $parentDir)) {
                            New-Item -ItemType Directory -Path $parentDir -Force | Out-Null
                        }
                        [System.IO.File]::WriteAllText($TokenFile, $Token)
                        Remove-Item -Force $FailFile -ErrorAction SilentlyContinue
                        Write-Output $Token
                        exit 0
                    }
                    New-Item -ItemType File -Path $FailFile -Force | Out-Null
                    if ($CachedToken) { Write-Output $CachedToken }
                    exit 1
                }
                { $_ -eq 'expired' -or $_ -eq 'not_found' } {
                    New-Item -ItemType File -Path $FailFile -Force | Out-Null
                    if ($CachedToken) { Write-Output $CachedToken }
                    exit 1
                }
                default {
                    Start-Sleep -Seconds 2
                }
            }
        } else {
            Start-Sleep -Seconds 2
        }
    }

    # Timeout
    New-Item -ItemType File -Path $FailFile -Force | Out-Null
    if ($CachedToken) { Write-Output $CachedToken }
    exit 1
} finally {
    # Clean up lock
    if ($lockAcquired) {
        Remove-Item -Force -Recurse $LockFile -ErrorAction SilentlyContinue
    }
}
