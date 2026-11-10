# Adds a directory to, or removes it from, the persistent Path; run by the
# installer and the uninstaller. NSIS strings are limited to 1024 characters
# and would truncate a long Path, so the edit is done here. The value keeps
# its unexpanded %VAR% entries and REG_EXPAND_SZ type; the caller broadcasts
# WM_SETTINGCHANGE.
#
#   update-path.ps1 -Action Add|Remove -Dir <directory> -Scope User|Machine

param(
    [Parameter(Mandatory)][ValidateSet('Add', 'Remove')][string]$Action,
    [Parameter(Mandatory)][string]$Dir,
    [Parameter(Mandatory)][ValidateSet('User', 'Machine')][string]$Scope
)

$ErrorActionPreference = 'Stop'

$key = if ($Scope -eq 'User') {
    [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment', $true)
} else {
    [Microsoft.Win32.Registry]::LocalMachine.OpenSubKey(
        'SYSTEM\CurrentControlSet\Control\Session Manager\Environment', $true)
}
try {
    $path = [string]$key.GetValue('Path', '',
        [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
    $dirNorm = $Dir.TrimEnd('\')
    $entries = @($path -split ';' | Where-Object { $_ -ne '' })
    # -ne/-eq compare case-insensitively, like Windows paths.
    $others = @($entries | Where-Object { $_.TrimEnd('\') -ne $dirNorm })
    if ($Action -eq 'Add') {
        if ($others.Count -ne $entries.Count) { return }
        $new = @($entries) + $dirNorm
    } else {
        if ($others.Count -eq $entries.Count) { return }
        $new = $others
    }
    $key.SetValue('Path', ($new -join ';'), [Microsoft.Win32.RegistryValueKind]::ExpandString)
} finally {
    $key.Close()
}
