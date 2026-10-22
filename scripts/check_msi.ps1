# Inspect the built database read-only. This never installs the package.
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$MsiPath,
    [switch]$AcceptanceFixture
)

$ErrorActionPreference = 'Stop'
$installer = $null
$database = $null

function Read-Pairs([string]$Query) {
    $view = $database.OpenView($Query)
    try {
        [void]$view.Execute()
        $rows = @{}
        while ($record = $view.Fetch()) {
            try { $rows[$record.StringData(1)] = $record.StringData(2) }
            finally { [void][Runtime.InteropServices.Marshal]::FinalReleaseComObject($record) }
        }
        return $rows
    } finally {
        [void]$view.Close()
        [void][Runtime.InteropServices.Marshal]::FinalReleaseComObject($view)
    }
}

try {
    $installer = New-Object -ComObject WindowsInstaller.Installer
    $database = $installer.OpenDatabase((Resolve-Path -LiteralPath $MsiPath).Path, 0)
    $properties = Read-Pairs 'SELECT `Property`, `Value` FROM `Property`'
    if ($properties['ALLUSERS'] -ne '1') { throw 'MSI must be per-machine (ALLUSERS=1)' }
    if ($properties['DISABLEADVTSHORTCUTS'] -ne '1') {
        throw 'MSI must create regular shortcuts so standard-user launch does not request configuration repair'
    }
    if ($properties['REINSTALLMODE'] -cne 'emus') {
        throw 'A same-version rebuild must overwrite equal-version files (REINSTALLMODE=emus)'
    }
    $fileVersions = Read-Pairs 'SELECT `File`, `Version` FROM `File`'
    $packageVersion = [Version]$properties['ProductVersion']
    foreach ($file in @('VacancydExe', 'SessionAgentExe', 'OwnerConsoleExe')) {
        $version = $null
        if (-not [Version]::TryParse($fileVersions[$file], [ref]$version) -or
            $version.Major -ne $packageVersion.Major -or
            $version.Minor -ne $packageVersion.Minor -or
            $version.Build -ne $packageVersion.Build -or
            $version.Revision -gt 0) {
            throw "MSI executable must carry the package version: $file"
        }
    }

    # Inspect the actual sequence, not the decompiler's inferred schedule.
    $sequence = Read-Pairs 'SELECT `Action`, `Sequence` FROM `InstallExecuteSequence`'
    foreach ($action in @('InstallInitialize', 'InstallExecute', 'RemoveExistingProducts', 'InstallFinalize',
                          'CreateShortcuts', 'RemoveShortcuts')) {
        if (-not $sequence.ContainsKey($action)) { throw "Missing MSI action: $action" }
    }
    if ([int]$sequence['RemoveExistingProducts'] -le [int]$sequence['InstallExecute'] -or
        [int]$sequence['RemoveExistingProducts'] -ge [int]$sequence['InstallFinalize']) {
        throw 'Old-product removal must follow InstallExecute and precede commit to retain the shared service'
    }

    # A same-version rebuild must be detected and removed as an upgrade
    # (msidbUpgradeAttributesVersionMaxInclusive), and must not also be
    # detected as a downgrade (msidbUpgradeAttributesVersionMinInclusive).
    $upgradeAttributes = Read-Pairs 'SELECT `ActionProperty`, `Attributes` FROM `Upgrade`'
    if (-not $upgradeAttributes.ContainsKey('WIX_UPGRADE_DETECTED') -or
        ([int]$upgradeAttributes['WIX_UPGRADE_DETECTED'] -band 512) -eq 0 -or
        (-not $upgradeAttributes.ContainsKey('WIX_DOWNGRADE_DETECTED')) -or
        ([int]$upgradeAttributes['WIX_DOWNGRADE_DETECTED'] -band 256) -ne 0) {
        throw 'A same-version rebuild must replace the installed product, not install beside it'
    }

    $directories = Read-Pairs 'SELECT `Shortcut`, `Directory_` FROM `Shortcut`'
    $targets = Read-Pairs 'SELECT `Shortcut`, `Target` FROM `Shortcut`'
    $arguments = Read-Pairs 'SELECT `Shortcut`, `Arguments` FROM `Shortcut`'
    $components = Read-Pairs 'SELECT `Shortcut`, `Component_` FROM `Shortcut`'
    foreach ($shortcut in @('SessionAgentStartup', 'SessionAgentShortcut', 'SetupShortcut', 'OwnerConsoleShortcut', 'OwnerConsoleStartup')) {
        if ($targets[$shortcut] -ne 'Complete') {
            throw "Shortcut table must retain the installed Complete feature target: $shortcut"
        }
    }
    if ($directories['OwnerConsoleStartup'] -ne 'StartupFolder' -or
        $components['OwnerConsoleStartup'] -ne 'OwnerConsoleExecutable' -or
        $arguments['OwnerConsoleStartup'] -ne '--tray' -or
        $directories['SessionAgentStartup'] -ne 'StartupFolder' -or
        $directories['SessionAgentShortcut'] -ne 'VacancydStartMenu' -or
        $directories['SetupShortcut'] -ne 'VacancydStartMenu' -or
        $directories['OwnerConsoleShortcut'] -ne 'VacancydStartMenu' -or
        $components['OwnerConsoleShortcut'] -ne 'OwnerConsoleExecutable' -or
        $components['SessionAgentStartup'] -ne 'SessionAgentExecutable' -or
        $components['SessionAgentShortcut'] -ne 'SessionAgentExecutable' -or
        $components['SetupShortcut'] -ne 'ServiceExecutable') {
        throw 'MSI shortcut location or component ownership is incorrect'
    }
    # Check the built failure-action array, including the final no-op that
    # prevents SCM from repeating the last restart forever.
    $recovery = @{
        FirstFailureActionType = 'restart'
        SecondFailureActionType = 'restart'
        ThirdFailureActionType = 'none'
        RestartServiceDelayInSeconds = '5'
        ResetPeriodInDays = '1'
        Component_ = 'ServiceExecutable'
        NewService = '1'
    }
    foreach ($column in $recovery.Keys) {
        $values = Read-Pairs ('SELECT `ServiceName`, `' + $column + '` FROM `Wix4ServiceConfig`')
        if ($values['vacancyd'] -ne $recovery[$column]) {
            throw "Unexpected service recovery field: $column"
        }
    }
    $actionTypes = Read-Pairs 'SELECT `Action`, `Type` FROM `CustomAction`'
    $actionSources = Read-Pairs 'SELECT `Action`, `Source` FROM `CustomAction`'
    $actionTargets = Read-Pairs 'SELECT `Action`, `Target` FROM `CustomAction`'
    $conditions = Read-Pairs 'SELECT `Action`, `Condition` FROM `InstallExecuteSequence`'
    if ($AcceptanceFixture) {
        if ($actionTypes['Wix4FailWhenDeferred_X64'] -ne '1025' -or
            $actionTargets['Wix4FailWhenDeferred_X64'] -ne 'WixFailWhenDeferred' -or
            $conditions['Wix4FailWhenDeferred_X64'] -ne 'WIXFAILWHENDEFERRED=1 AND VersionNT > 400' -or
            [int]$sequence['Wix4FailWhenDeferred_X64'] -le [int]$sequence['StartServices'] -or
            [int]$sequence['Wix4FailWhenDeferred_X64'] -ge [int]$sequence['InstallExecute'] -or
            $actionTypes['QaFailAfterRemoval'] -ne '19' -or
            $conditions['QaFailAfterRemoval'] -ne 'WIX_UPGRADE_DETECTED' -or
            [int]$sequence['QaFailAfterRemoval'] -le [int]$sequence['RemoveExistingProducts'] -or
            [int]$sequence['QaFailAfterRemoval'] -ge [int]$sequence['InstallFinalize']) {
            throw 'QA faults must exercise distinct rollback boundaries before and after old-product removal'
        }
    } elseif ($actionTypes.ContainsKey('Wix4FailWhenDeferred_X64') -or
              $actionTypes.ContainsKey('QaFailAfterRemoval')) {
        throw 'Intentional failure actions must not appear in ordinary packages'
    }
    if ($conditions['ScheduleReboot'] -ne '(WIX_UPGRADE_DETECTED AND NOT Installed) OR (REMOVE = "ALL" AND NOT UPGRADINGPRODUCTCODE)' -or
        [int]$sequence['ScheduleReboot'] -le [int]$sequence['InstallFinalize']) {
        throw 'Successful major upgrade and explicit uninstall must request a reboot after commit'
    }
    if ($actionTypes['ReportedFailureRecovery'] -ne '3073' -or
        $actionSources['ReportedFailureRecovery'] -ne 'Wix4UtilCA_X64' -or
        $actionTargets['ReportedFailureRecovery'] -ne 'WixQuietExec' -or
        $actionTypes['SetReportedFailureRecovery'] -ne '51' -or
        $actionSources['SetReportedFailureRecovery'] -ne 'ReportedFailureRecovery' -or
        $actionTargets['SetReportedFailureRecovery'] -ne '"[System64Folder]sc.exe" failureflag vacancyd 1') {
        throw 'Reported-error flag must use the fixed checked non-impersonating deferred SCM action'
    }
    foreach ($action in @('Wix4SchedServiceConfig_X64', 'SetReportedFailureRecovery', 'ReportedFailureRecovery')) {
        if ($conditions[$action] -ne 'NOT Installed AND NOT WIX_UPGRADE_DETECTED AND $ServiceExecutable = 3' -or
            [int]$sequence[$action] -le [int]$sequence['InstallServices'] -or
            [int]$sequence[$action] -ge [int]$sequence['StartServices']) {
            throw 'Recovery actions must run only on fresh install, preserving existing upgrade/repair policy'
        }
    }
    if ([int]$sequence['SetReportedFailureRecovery'] -ge [int]$sequence['ReportedFailureRecovery']) {
        throw 'Failure-flag command must be set before its deferred action is scheduled'
    }
    if ($actionTypes['ProtectConfiguration'] -ne '3073' -or
        $actionSources['ProtectConfiguration'] -ne 'Wix4UtilCA_X64' -or
        $actionTargets['ProtectConfiguration'] -ne 'WixQuietExec' -or
        $actionTargets['SetProtectConfiguration'] -ne '"[#VacancydExe]" protect-config --config "[CommonAppDataVacancyd]vacancyd.toml" --require-admin-owner') {
        throw 'Configuration protection must use the fixed checked non-impersonating deferred action'
    }
    foreach ($action in @('SetProtectConfiguration', 'ProtectConfiguration')) {
        if ($conditions[$action] -ne 'NOT Installed AND $ServiceExecutable = 3' -or
            [int]$sequence[$action] -le [int]$sequence['InstallFiles'] -or
            [int]$sequence[$action] -ge [int]$sequence['StartServices']) {
            throw 'Configuration must be protected after file installation and before service start'
        }
    }
    $permissions = Read-Pairs 'SELECT `LockObject`, `SDDLText` FROM `MsiLockPermissionsEx`'
    if ($permissions['CommonAppDataVacancyd'] -ne 'O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)' -or
        $permissions['InstalledConfig'] -ne 'O:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)') {
        throw 'Installed configuration must be owned by Administrators and accessible only to Administrators and LocalSystem'
    }
    Write-Output 'MSI artifact checks passed: machine scope, transactional removal, owned regular-shortcut policy, bounded service recovery, explicit upgrade/uninstall reboot'
} finally {
    if ($null -ne $database) { [void][Runtime.InteropServices.Marshal]::FinalReleaseComObject($database) }
    if ($null -ne $installer) { [void][Runtime.InteropServices.Marshal]::FinalReleaseComObject($installer) }
}
