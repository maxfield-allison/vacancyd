<#
.SYNOPSIS
  Measure lendable capacity in one availability tier.

.DESCRIPTION
  vacancyd's per-model thresholds are only as good as the measurements behind
  them, and the honest answer is not one number — it is one per *tier*, because
  what the owner is doing changes the budget by many gigabytes.

  Tiers (see docs/tiers.md):
    1  gaming      - owner playing; the hard floor
    2  idle        - logged in, unlocked, ordinary apps; locked is equivalent
    3  logged-out  - nobody logged in; the ceiling

  Run this once per tier and post the CSVs together. It samples free VRAM on
  every NVIDIA GPU, system memory, CPU load, and (when vacancyd is running)
  its session-state view, so a row is self-describing about which tier it
  belongs to rather than relying on the operator's memory.

  Read-only. Allocates nothing on any GPU. Does not lock, log out, or change
  any power or display setting — those transitions are the operator's to make.

.EXAMPLE
  .\measure-tiers.ps1 -Tier idle -Minutes 10
  .\measure-tiers.ps1 -Tier gaming -Minutes 15   # start it, then launch the game
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidateSet('gaming', 'idle', 'locked', 'logged-out')]
    [string]$Tier,

    [int]$Minutes = 10,
    [int]$IntervalSec = 5,
    [string]$OutDir = '.',
    [string]$VacancydUrl = 'http://127.0.0.1:9811'
)

$ErrorActionPreference = 'Stop'
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$out = Join-Path $OutDir "tier-$Tier-$stamp.csv"
$end = (Get-Date).AddMinutes($Minutes)

'timestamp,tier,gpu_index,gpu_name,mem_total_mib,mem_free_mib,util_gpu_pct,temp_c,power_w,sys_mem_free_mib,cpu_pct,session_state,veto_active,foreground' |
    Out-File -FilePath $out -Encoding utf8

Write-Host "Tier '$Tier': sampling every ${IntervalSec}s for $Minutes min -> $out"
if ($Tier -eq 'gaming') { Write-Host '  Launch the game now; leave this running.' }
if ($Tier -eq 'logged-out') { Write-Host '  Run this as a scheduled task or service - a console session ends at logout.' }

$cpuCounter = '\Processor(_Total)\% Processor Time'

while ((Get-Date) -lt $end) {
    $ts = (Get-Date).ToString('o')

    # vacancyd's own view, when it is up. Optional by design: the script must
    # still work before the daemon is installed.
    $session = 'n/a'; $veto = 'n/a'; $fg = 'n/a'
    try {
        $s = Invoke-RestMethod "$VacancydUrl/status" -TimeoutSec 2
        $session = $s.presence.session
        $veto = $s.owner_veto.active
        if ($s.presence.foreground) { $fg = $s.presence.foreground }
    } catch { }

    $sysFree = [math]::Round((Get-CimInstance Win32_OperatingSystem).FreePhysicalMemory / 1KB, 0)
    $cpu = try { [math]::Round((Get-Counter $cpuCounter -ErrorAction Stop).CounterSamples[0].CookedValue, 0) } catch { -1 }

    $rows = nvidia-smi --query-gpu=index,name,memory.total,memory.free,utilization.gpu,temperature.gpu,power.draw `
        --format=csv,noheader,nounits
    foreach ($r in $rows) {
        $f = $r -split '\s*,\s*'
        "$ts,$Tier,$($f[0]),$($f[1]),$($f[2]),$($f[3]),$($f[4]),$($f[5]),$($f[6]),$sysFree,$cpu,$session,$veto,$fg" |
            Out-File -FilePath $out -Append -Encoding utf8
    }
    Start-Sleep -Seconds $IntervalSec
}

Write-Host "Done: $out"

# Summary: the numbers that actually go in the tier table.
$rows = Import-Csv $out
$rows | Group-Object gpu_index | ForEach-Object {
    $g = $_.Group
    $free = $g | ForEach-Object { [int]$_.mem_free_mib } | Sort-Object
    [PSCustomObject]@{
        Tier        = $Tier
        GPU         = $g[0].gpu_name
        Samples     = $g.Count
        MinFreeMiB  = $free[0]
        MedFreeMiB  = $free[[int]($free.Count / 2)]
        MaxFreeMiB  = $free[-1]
        MaxUtilPct  = ($g | ForEach-Object { [int]$_.util_gpu_pct } | Measure-Object -Maximum).Maximum
    }
} | Format-Table -AutoSize
