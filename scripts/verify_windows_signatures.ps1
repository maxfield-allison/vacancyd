# Inspect final files, including publisher identity and a trusted timestamp.
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string[]]$Files,
    [Parameter(Mandatory = $true)][string]$ExpectedSubject,
    [Parameter(Mandatory = $true)][string]$ReportPath
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function Resolve-WindowsSignatureReportPath([string]$Path) {
    if ([string]::IsNullOrWhiteSpace($Path)) { throw 'Signature report path is required' }
    # System.IO resolves relative paths against the process working directory. On
    # Windows PowerShell that can differ from the current PowerShell location used
    # by GitLab's shell executor. Resolve through the provider instead, including
    # output files which do not exist yet.
    return $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($Path)
}

function Invoke-WindowsSignatureVerification {
    param(
        [Parameter(Mandatory = $true)][string[]]$CandidateFiles,
        [Parameter(Mandatory = $true)][string]$PublisherSubject,
        [Parameter(Mandatory = $true)][string]$OutputPath
    )
    if ([string]::IsNullOrWhiteSpace($PublisherSubject)) { throw 'Expected publisher subject is required' }
    if ($CandidateFiles.Count -eq 0) { throw 'No files supplied for signature verification' }
    $signtool = Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\bin\*\x64\signtool.exe" |
        Sort-Object FullName -Descending | Select-Object -First 1
    if ($null -eq $signtool) { throw 'Windows SDK signtool is required' }
    $receipts = @()
    foreach ($file in $CandidateFiles) {
        $path = (Resolve-Path -LiteralPath $file).Path
        if ([IO.Path]::GetExtension($path) -notin @('.exe', '.msi')) { throw 'Only EXE/MSI files are expected' }
        $signature = Get-AuthenticodeSignature -LiteralPath $path
        if ($signature.Status -ne 'Valid' -or $null -eq $signature.SignerCertificate -or
            $signature.SignerCertificate.Subject -cne $PublisherSubject -or
            $null -eq $signature.TimeStamperCertificate) {
            throw "Missing, invalid, untimestamped or unexpected publisher signature: $([IO.Path]::GetFileName($path))"
        }
        & $signtool.FullName verify /pa /all /tw $path
        if ($LASTEXITCODE -ne 0) { throw 'Windows signature policy verification failed' }
        $receipts += [ordered]@{
            file = [IO.Path]::GetFileName($path)
            sha256 = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant()
            subject = $signature.SignerCertificate.Subject
            signer_thumbprint = $signature.SignerCertificate.Thumbprint
            timestamp_subject = $signature.TimeStamperCertificate.Subject
            status = $signature.Status.ToString()
        }
    }
    $resolvedOutputPath = Resolve-WindowsSignatureReportPath $OutputPath
    [IO.File]::WriteAllText($resolvedOutputPath,
        (ConvertTo-Json -InputObject @($receipts) -Depth 5) + "`n", [Text.UTF8Encoding]::new($false))
}

if ($MyInvocation.InvocationName -ne '.') {
    Invoke-WindowsSignatureVerification -CandidateFiles $Files -PublisherSubject $ExpectedSubject -OutputPath $ReportPath
}
